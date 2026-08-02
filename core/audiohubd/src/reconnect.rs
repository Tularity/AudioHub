//! spec-m4c §C: reconnect + session recovery.
//!
//! Only a peer THIS daemon has connected out to gets a retry loop — a peer that
//! merely connected to us is its own side's job to re-establish, and retrying
//! toward it would mean dialling an address we were never told to dial.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use audiohub_ipc::OpenSessionParams;
use audiohub_net::identity::PeerStore;

use crate::conn::{self, ConnectOrigin};
use crate::haldev;
use crate::{dlog, lk, DaemonInner, SessionOrigin};

/// Frozen backoff ladder in seconds; the last rung repeats forever (cap 30s).
pub const BACKOFF_S: [f64; 5] = [1.0, 2.0, 5.0, 10.0, 30.0];
/// ±20%: peers that dropped together (a switch reboot) must not retry in
/// lockstep and re-create the thundering herd their reconnect is meant to heal.
pub const JITTER_FRAC: f64 = 0.2;

/// Delay before retry number `attempt` (0 = the first retry after the drop).
pub fn backoff_base_s(attempt: u32) -> f64 {
    BACKOFF_S[(attempt as usize).min(BACKOFF_S.len() - 1)]
}

/// Maps `r` in [0,1] onto [base*(1-JITTER_FRAC), base*(1+JITTER_FRAC)].
pub fn apply_jitter(base: f64, r: f64) -> f64 {
    base * (1.0 - JITTER_FRAC + 2.0 * JITTER_FRAC * r.clamp(0.0, 1.0))
}

pub fn next_delay_s(attempt: u32) -> f64 {
    apply_jitter(backoff_base_s(attempt), rand_unit())
}

fn rand_unit() -> f64 {
    use rand_core::RngCore;
    rand_core::OsRng.next_u32() as f64 / (u32::MAX as f64 + 1.0)
}

/// How often the supervisor looks for a due peer.
const SUPERVISOR_TICK: Duration = Duration::from_millis(200);

/// 恢复计划里的一条会话：开会话的参数，**外加开它的那个身份**。
///
/// 身份必须跟着参数一起走。重放过去直接调用 `conn::open_session`，而那个入口把
/// origin 硬编码成 `SessionOrigin::User`；于是一条 `Hal`（模式 B 设备协调器开的）
/// 会话在重连之后会以 `user` 的身份复活——遥测里那条 `origin=user` 的重复扬声器
/// 流就是这么来的。身份丢了不只是标签难看：设备协调器只认得自己开的 `Hal` 会话，
/// 应用停止使用虚拟设备时它不会去关一条挂着 `User` 标签的会话，而 UI 也被明令
/// 不许关 `Hal` 会话——错标之后这条会话就成了谁都不负责的孤儿。
#[derive(Clone)]
pub(crate) struct PlannedSession {
    pub params: OpenSessionParams,
    pub origin: SessionOrigin,
}

pub(crate) struct PeerRecon {
    /// The address override that worked, so a peer whose mDNS/last_addr is
    /// stale still reconnects to where we were actually told to look.
    pub addr: Option<String>,
    pub attempts: u32,
    /// `Some` = a retry is armed. `None` = connected, or never dropped.
    pub next_at: Option<Instant>,
    pub in_flight: bool,
    /// Params of the sessions WE opened, captured at disconnect. 只收
    /// `recoverable_by_replay` 认可的那些——别的会话各有各的主人。
    pub sessions: Vec<PlannedSession>,
}

impl PeerRecon {
    fn new(addr: Option<String>) -> PeerRecon {
        PeerRecon {
            addr,
            attempts: 0,
            next_at: None,
            in_flight: false,
            sessions: Vec::new(),
        }
    }
}

/// 这条会话断线后**该不该由通用重放机制**重新打开。
///
/// 只有 `User` 会话该，而这条规则是「谁拥有它」推出来的：
///
/// - `User`（UI / CLI / probe 开的）：没有任何别的机制记得它。重放是它唯一的
///   救生索，去掉就等于断线后再也回不来。
/// - `Hal`（模式 B 设备协调器开的）：协调器**本身就是一套声明式恢复机制**。
///   `haldev::coordinate_sessions` 每 200 ms 检查一次「应用还在用这个虚拟设备吗
///   （`io_out`/`io_in`），而它的会话还在吗」；会话随连接一起没了，它就把死掉的
///   id 清掉、重开一条，并把新 id 写回 `sess_out`/`sess_in`。让重放**也**开一条，
///   就是两套恢复机制各开一路：
///
///   两条 tx 流拿着同一个 `SourceSpec::HalSpeaker { slot }`，在 tx 引擎里命中
///   同一个 `SourceEnt`（`engine.rs` 的 `sources.entry`，第二条只把 `refs` 加 1，
///   不新建源），每 tick 读**同一个 `ent.frame`**，两条 `rung` 都是 0 不重采样，
///   于是 `f32_to_s16le` 出来的载荷逐字节相同。对端把两路直接相加 = 精确
///   +6.02 dB，撞上 `engine.rs` 里 `soft_clip` 的 0.8 阈值削顶失真。每断一次网
///   多一路，随重连次数线性累积。麦克风方向同理：两条 rx 流写进同一个
///   `hal_slot` 的桶，虚拟麦克风也响 +6 dB。
///
///   协调器还比重放**更正确**：它看的是「此刻应用是否仍在用这个设备」，而重放
///   计划是断线那一刻的快照——若应用早已停止播放，重放照样会把会话硬拉回来。
/// - `Peer`：对端开的，`SessionEntry.replay` 本来就是 `None`，重开是对端的事。
pub(crate) fn recoverable_by_replay(origin: SessionOrigin) -> bool {
    matches!(origin, SessionOrigin::User)
}

/// 两条会话是否指向**同一条媒体链路**：同一个方向（`kind`）、同一个音源
/// （`source`/`freq`/`backend`/`hal`）、同一个落点（`bridge`/`monitor`）。
///
/// 调用方已经按对端指纹过滤过，所以这里不比 `peer`：那是个选择器（可以是名字、
/// 指纹或指纹前缀），字面比较并不可靠。
///
/// `freq` 走 bit 比较，和 `SourceSpec::Tone { freq_bits }` 的做法一致——f32 没有
/// `Eq`，而这里需要一个全序的相等判断。
fn same_media_intent(a: &OpenSessionParams, b: &OpenSessionParams) -> bool {
    a.kind == b.kind
        && a.source == b.source
        && a.freq.map(f32::to_bits) == b.freq.map(f32::to_bits)
        && a.backend == b.backend
        && a.hal == b.hal
        && a.bridge == b.bridge
        && a.monitor == b.monitor
}

/// 重放一条计划时该做什么。抽成纯函数，是因为这里正是出过 bug 的三个判断：
/// 身份、模式闸门、去重。
#[derive(Debug, PartialEq)]
pub(crate) enum ReplayAction {
    /// 重开，并且用**这个** origin —— 不是硬编码的 `User`。
    Open(SessionOrigin),
    /// 同一条媒体链路已经在线了（典型情形：设备协调器抢先恢复了它那一路）。
    SkipDuplicate,
    /// 本机模式已经不允许这条会话了。
    SkipMode(String),
}

/// `live` 是这个对端**当前在线**的本地会话参数集（调用方按指纹过滤后传入）。
///
/// 三道关，缺一不可：
/// 1. 去重——保证同一个 `(对端, spec, 方向)` 不会因为重连多出第二路；
/// 2. 模式闸门——`haldev::refuse_ui_session`，和 IPC 路径 (`ipcserv.rs` 的
///    `SESSION_OPEN`) 调的是同一个函数。重放过去完全绕过它：模式 B 明令禁止
///    UI 开会话，一条模式 A 时期开的用户会话却能靠一次断线在模式 B 下复活。
///    `Hal` 会话不过这道关——它本身就是模式 B 的路径（协调器传 `override_mode:
///    true` 也是这个道理）；
/// 3. 身份——把原来的 origin 原样带出去。
pub(crate) fn plan_replay(
    planned: &PlannedSession,
    live: &[OpenSessionParams],
    mode: &str,
) -> ReplayAction {
    if live.iter().any(|l| same_media_intent(l, &planned.params)) {
        return ReplayAction::SkipDuplicate;
    }
    if !matches!(planned.origin, SessionOrigin::Hal { .. }) {
        if let Some(why) = haldev::refuse_ui_session(mode, planned.params.override_mode) {
            return ReplayAction::SkipMode(why);
        }
    }
    ReplayAction::Open(planned.origin)
}

/// 这个对端此刻在线的、**本机开的**会话参数。对端开的会话 `replay` 是 `None`，
/// 不参与去重：它们是另一个方向的角色，不可能和我们要重开的撞车。
fn live_intents(inner: &DaemonInner, fp: &str) -> Vec<OpenSessionParams> {
    lk(&inner.state)
        .sessions
        .values()
        .filter(|e| e.conn.fp == fp)
        .filter_map(|e| e.replay.as_ref().map(|p| (**p).clone()))
        .collect()
}

/// Records that this daemon connected out to `fp` (and how), which is what
/// makes the peer eligible for the retry loop at all. Returns the recovery plan
/// the cleared retry was holding: this connect took that retry's place, so the
/// caller owes it the same replay — nothing else will ever do it.
#[must_use = "these sessions are only recoverable here; dropping them loses them"]
pub(crate) fn note_outbound(
    inner: &DaemonInner,
    fp: &str,
    addr: Option<&str>,
) -> Vec<PlannedSession> {
    let mut m = lk(&inner.recon);
    let e = m
        .entry(fp.to_string())
        .or_insert_with(|| PeerRecon::new(addr.map(str::to_string)));
    if addr.is_some() {
        e.addr = addr.map(str::to_string);
    }
    e.attempts = 0;
    e.next_at = None;
    std::mem::take(&mut e.sessions)
}

/// Arms the retry loop after a drop, on the first backoff rung.
pub(crate) fn arm(inner: &DaemonInner, fp: &str, sessions: Vec<PlannedSession>) {
    arm_in(inner, fp, sessions, false)
}

/// Arms with no delay: a newer conn to this peer already took over, so there is
/// nothing to back off from. The supervisor's next tick finds the live conn
/// (connect_peer returns it immediately) and replays the plan on it.
pub(crate) fn arm_now(inner: &DaemonInner, fp: &str, sessions: Vec<PlannedSession>) {
    arm_in(inner, fp, sessions, true)
}

/// `sessions` is ADDED to the recovery plan, never substituted for it: each
/// drop contributes the streams that were live on ITS connection and the sets
/// are disjoint (a session belongs to exactly one conn), so a set arriving
/// while a retry is already armed must not be thrown away.
fn arm_in(inner: &DaemonInner, fp: &str, sessions: Vec<PlannedSession>, immediate: bool) {
    if inner.shutdown.load(Ordering::SeqCst) {
        return;
    }
    let armed = {
        let mut m = lk(&inner.recon);
        match m.get_mut(fp) {
            // no entry = never connected out to this peer, or disarmed on
            // purpose (explicit disconnect / unpair): not ours to retry
            None => None,
            Some(e) => {
                e.sessions.extend(sessions);
                let n = e.sessions.len();
                match e.next_at {
                    // already retrying: keep the ladder where it is, except
                    // that a takeover has a live conn waiting and must not sit
                    // out a 30s rung
                    Some(t) => {
                        let now = Instant::now();
                        if immediate && now < t {
                            e.next_at = Some(now);
                            Some((0.0, n))
                        } else {
                            None
                        }
                    }
                    None => {
                        e.attempts = 0;
                        let d = if immediate { 0.0 } else { next_delay_s(0) };
                        e.next_at = Some(Instant::now() + Duration::from_secs_f64(d));
                        Some((d, n))
                    }
                }
            }
        }
    };
    if let Some((d, n)) = armed {
        if immediate {
            dlog!("[audiohubd] peer {fp}: a newer control channel took over; re-opening {n} session(s) on it");
        } else {
            dlog!(
                "[audiohubd] peer {fp}: control channel lost, reconnecting in {d:.1}s \
                 ({n} session(s) to recover)"
            );
        }
    }
}

/// Stops retrying `fp` for good (explicit disconnect / unpair).
pub(crate) fn disarm(inner: &DaemonInner, fp: &str) {
    lk(&inner.recon).remove(fp);
}

/// `(reconnecting, retry_in_s)` per fingerprint, for `peers.list`.
pub(crate) fn snapshot(inner: &DaemonInner) -> HashMap<String, (bool, Option<f64>)> {
    let now = Instant::now();
    lk(&inner.recon)
        .iter()
        .map(|(fp, e)| {
            let retry_in = e.next_at.map(|t| t.saturating_duration_since(now).as_secs_f64());
            (fp.clone(), (e.next_at.is_some(), retry_in))
        })
        .collect()
}

pub(crate) fn supervisor_loop(inner: Arc<DaemonInner>) {
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        std::thread::sleep(SUPERVISOR_TICK);
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        let due: Vec<String> = {
            let mut m = lk(&inner.recon);
            let now = Instant::now();
            let mut v = Vec::new();
            for (fp, e) in m.iter_mut() {
                if e.in_flight || e.next_at.map_or(true, |t| now < t) {
                    continue;
                }
                e.in_flight = true;
                v.push(fp.clone());
            }
            v
        };
        // one thread per due peer: a connect blocks for up to the TCP connect +
        // handshake timeout, and one unreachable peer must not delay the others
        for fp in due {
            let i = inner.clone();
            let f = fp.clone();
            let spawned = std::thread::Builder::new()
                .name("ahb-retry".into())
                .spawn(move || {
                    if std::panic::catch_unwind(AssertUnwindSafe(|| attempt(&i, &f))).is_err() {
                        dlog!("[audiohubd] peer {f}: reconnect attempt panicked");
                    }
                    if let Some(e) = lk(&i.recon).get_mut(&f) {
                        e.in_flight = false;
                    }
                });
            if spawned.is_err() {
                if let Some(e) = lk(&inner.recon).get_mut(&fp) {
                    e.in_flight = false;
                }
            }
        }
    }
}

/// Re-opens the sessions captured at the drop. Shared by the retry loop and by
/// a user-initiated connect that took an armed retry's place.
fn replay_sessions(inner: &Arc<DaemonInner>, fp: &str, sessions: Vec<PlannedSession>) {
    if sessions.is_empty() {
        return;
    }
    dlog!(
        "[audiohubd] peer {fp}: reconnected; recovering {} session(s)",
        sessions.len()
    );
    for p in sessions {
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        // 现场**每条都重新读一次**，不能在循环外读一次了事：上一条刚开出来的
        // 会话，对下一条来说就是「已经在线」；而设备协调器跑在另一个线程上，
        // 完全可能在我们两条之间抢先把它那一路恢复好。
        let live = live_intents(inner, fp);
        let mode = haldev::effective_mode(inner);
        let origin = match plan_replay(&p, &live, mode) {
            ReplayAction::Open(o) => o,
            ReplayAction::SkipDuplicate => {
                dlog!(
                    "[audiohubd] peer {fp}: the {} session is already live again; not opening a \
                     second one",
                    p.params.kind
                );
                continue;
            }
            ReplayAction::SkipMode(why) => {
                dlog!(
                    "[audiohubd] peer {fp}: not recovering the {} session — {why}",
                    p.params.kind
                );
                continue;
            }
        };
        // open_session_from mints a FRESH stream id and a FRESH media salt;
        // OpenSessionParams carries neither, so a replay cannot re-use the old
        // pair and re-create the AEAD nonce reuse defect. `origin` is the one
        // the session was opened with, never a hardcoded `User`.
        match conn::open_session_from(inner, &p.params, origin) {
            Ok(info) => dlog!(
                "[audiohubd] peer {fp}: recovered {} session as stream {}",
                p.params.kind,
                info.id
            ),
            Err(e) => dlog!(
                "[audiohubd] peer {fp}: could not recover the {} session: {e:#}",
                p.params.kind
            ),
        }
    }
}

/// Replays off-thread: the caller is `connect_peer`, and a session open dials
/// the same peer back plus waits on an ack — doing that inline would re-enter
/// the connect path and stall an IPC request for as long as the opens take.
pub(crate) fn spawn_replay(inner: &Arc<DaemonInner>, fp: &str, sessions: Vec<PlannedSession>) {
    if sessions.is_empty() || inner.shutdown.load(Ordering::SeqCst) {
        return;
    }
    let i = inner.clone();
    let f = fp.to_string();
    let spawned = std::thread::Builder::new()
        .name("ahb-replay".into())
        .spawn(move || {
            if std::panic::catch_unwind(AssertUnwindSafe(|| replay_sessions(&i, &f, sessions)))
                .is_err()
            {
                dlog!("[audiohubd] peer {f}: session recovery panicked");
            }
        });
    if spawned.is_err() {
        dlog!("[audiohubd] peer {fp}: could not spawn the session recovery thread");
    }
}

fn attempt(inner: &Arc<DaemonInner>, fp: &str) {
    if inner.shutdown.load(Ordering::SeqCst) {
        return;
    }
    // An unpaired peer must not be dialled forever: the store is the authority
    // on who we are allowed to talk to, whoever removed the entry.
    if let Ok(store) = PeerStore::load_at(Some(&inner.cfg_dir)) {
        if !store.list().iter().any(|p| p.fingerprint == fp) {
            disarm(inner, fp);
            dlog!("[audiohubd] peer {fp} is no longer paired; reconnect stopped");
            return;
        }
    }
    let addr = lk(&inner.recon).get(fp).and_then(|e| e.addr.clone());
    match conn::connect_peer(inner, fp, addr.as_deref(), ConnectOrigin::Retry) {
        Ok(_) => {
            let sessions = {
                let mut m = lk(&inner.recon);
                match m.get_mut(fp) {
                    Some(e) => {
                        e.attempts = 0;
                        e.next_at = None;
                        std::mem::take(&mut e.sessions)
                    }
                    // disarmed while the connect was in flight (an explicit
                    // disconnect): drop what we just built, recover nothing
                    None => {
                        conn::drop_conn(inner, fp, "disconnected while reconnecting");
                        return;
                    }
                }
            };
            replay_sessions(inner, fp, sessions);
        }
        Err(e) => {
            let next = {
                let mut m = lk(&inner.recon);
                match m.get_mut(fp) {
                    Some(ent) if ent.next_at.is_some() => {
                        ent.attempts = ent.attempts.saturating_add(1);
                        let d = next_delay_s(ent.attempts);
                        ent.next_at = Some(Instant::now() + Duration::from_secs_f64(d));
                        Some((ent.attempts, d))
                    }
                    _ => None, // disarmed mid-attempt: stay stopped
                }
            };
            if let Some((n, d)) = next {
                dlog!("[audiohubd] peer {fp}: reconnect attempt {n} failed ({e:#}); retry in {d:.1}s");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audiohub_ipc::{MODE_A, MODE_B, SOURCE_HAL_SPEAKER, SOURCE_TONE};

    fn base(kind: &str) -> OpenSessionParams {
        OpenSessionParams {
            peer: "aabbccdd".to_string(),
            kind: kind.to_string(),
            source: None,
            freq: None,
            backend: None,
            monitor: false,
            verify_freq: None,
            simulate_loss_pct: None,
            volume_sync: false,
            bridge: None,
            hal: false,
            override_mode: false,
        }
    }

    /// 设备协调器为一个槽位的虚拟扬声器开的会话（haldev.rs `SessCmd::Open`）。
    fn hal_spk() -> OpenSessionParams {
        OpenSessionParams {
            source: Some(SOURCE_HAL_SPEAKER.to_string()),
            volume_sync: true,
            override_mode: true, // 协调器就是模式 B 本身
            ..base("spk")
        }
    }

    /// 设备协调器为同一槽位的虚拟麦克风开的会话（另一个方向，同一个 bug）。
    fn hal_mic() -> OpenSessionParams {
        OpenSessionParams { hal: true, ..base("mic") }
    }

    /// UI / CLI 开的普通会话。
    fn user_tone() -> OpenSessionParams {
        OpenSessionParams {
            source: Some(SOURCE_TONE.to_string()),
            freq: Some(440.0),
            ..base("spk")
        }
    }

    /// 一台 daemon 上「本机开的、还活着的」会话表——复现这个 bug 只需要这么多。
    #[derive(Default)]
    struct Live(Vec<PlannedSession>);

    impl Live {
        fn open(&mut self, params: &OpenSessionParams, origin: SessionOrigin) {
            self.0.push(PlannedSession { params: params.clone(), origin });
        }

        /// `replay_sessions` 里 `live_intents` 拿到的东西。
        fn intents(&self) -> Vec<OpenSessionParams> {
            self.0.iter().map(|s| s.params.clone()).collect()
        }

        /// conn.rs `teardown_conn`：连接没了，会话全拆，只有
        /// `recoverable_by_replay` 认可的进入恢复计划。
        fn drop_connection(&mut self) -> Vec<PlannedSession> {
            let plan: Vec<PlannedSession> = self
                .0
                .iter()
                .filter(|s| recoverable_by_replay(s.origin))
                .cloned()
                .collect();
            self.0.clear();
            plan
        }

        /// haldev.rs `coordinate_sessions`。**故意是「瞎」的**：协调器只记得自己
        /// 开的那个 stream id（`sess_out`/`sess_in`），那个 id 随连接一起死了它就
        /// 重开一条。它看不见重放刚开出来的会话，也不可能看见——重放不写
        /// `sess_out`。所以 `plan_replay` 的去重救不了协调器这一路：`Hal` 会话
        /// 必须一开始就不进恢复计划。
        fn coordinator_pass(&mut self, slot: u8, params: &OpenSessionParams) {
            self.open(params, SessionOrigin::Hal { slot });
        }

        /// reconnect.rs `replay_sessions`：逐条决策，每条都重新读一次现场。
        fn replay_pass(&mut self, plan: &[PlannedSession], mode: &str) {
            for p in plan {
                let live = self.intents();
                if let ReplayAction::Open(origin) = plan_replay(p, &live, mode) {
                    self.open(&p.params, origin);
                }
            }
        }

        fn matching(&self, p: &OpenSessionParams) -> Vec<&PlannedSession> {
            self.0.iter().filter(|s| same_media_intent(&s.params, p)).collect()
        }
    }

    /// 这就是那个 bug：mac→win 出现两路载荷逐字节相同的扬声器流
    /// （`origin=hal` 一路 + `origin=user` 一路），对端相加 = +6.02 dB 削顶。
    ///
    /// 断线后跑一次完整重连：设备协调器和通用重放都会动，而且它们在不同线程上，
    /// 先后顺序不定——两种顺序都必须只留下一条流。
    #[test]
    fn a_reconnect_leaves_exactly_one_speaker_stream_per_spec() {
        let spk = hal_spk();

        let mut before = Live::default();
        before.coordinator_pass(0, &spk);
        let plan = before.drop_connection();

        // 第一道也是最要紧的一道：协调器自己会恢复，它的会话不该进恢复计划。
        // 协调器看不见重放开的会话，所以这一条一旦失守，下面的去重挡不住。
        assert!(
            plan.is_empty(),
            "设备协调器的会话归协调器恢复，不该同时进入通用恢复计划"
        );

        for coordinator_first in [true, false] {
            let mut live = Live::default();
            if coordinator_first {
                live.coordinator_pass(0, &spk);
                live.replay_pass(&plan, MODE_B);
            } else {
                live.replay_pass(&plan, MODE_B);
                live.coordinator_pass(0, &spk);
            }
            let same = live.matching(&spk);
            assert_eq!(
                same.len(),
                1,
                "同一个 spec 只能有一条扬声器流（coordinator_first={coordinator_first}）"
            );
            assert_eq!(
                same[0].origin,
                SessionOrigin::Hal { slot: 0 },
                "重连后 origin 必须仍是 Hal，不能被重放改写成 User"
            );
        }
    }

    /// 麦克风方向是同一个 bug 的另一半：两条 rx 流写进同一个 `hal_slot` 的桶，
    /// 虚拟麦克风一样 +6 dB。
    #[test]
    fn the_microphone_direction_is_covered_too() {
        let mic = hal_mic();
        let mut live = Live::default();
        live.coordinator_pass(2, &mic);
        let plan = live.drop_connection();
        assert!(plan.is_empty());

        let mut live = Live::default();
        live.coordinator_pass(2, &mic);
        live.replay_pass(&plan, MODE_B);
        assert_eq!(live.matching(&mic).len(), 1);
        assert_eq!(live.0[0].origin, SessionOrigin::Hal { slot: 2 });
    }

    /// 第二道网：万一将来某次改动又把一条 `Hal` 会话塞回恢复计划，重放也不该在
    /// 协调器已经恢复它之后再开一条，而且身份要原样带出去。
    #[test]
    fn a_replay_never_adds_a_second_stream_for_a_link_already_live() {
        let spk = hal_spk();
        let planned = PlannedSession { params: spk.clone(), origin: SessionOrigin::Hal { slot: 0 } };

        assert_eq!(
            plan_replay(&planned, &[spk.clone()], MODE_B),
            ReplayAction::SkipDuplicate,
            "协调器已经恢复了这条链路，重放不能再开第二条"
        );
        assert_eq!(
            plan_replay(&planned, &[], MODE_B),
            ReplayAction::Open(SessionOrigin::Hal { slot: 0 }),
            "没人恢复过就该开——而且用原来的身份，不是硬编码的 User"
        );
    }

    /// 重放路径必须过和 `ipcserv.rs` 的 `SESSION_OPEN` 同一道模式闸门
    /// （`haldev::refuse_ui_session`）。以前它完全绕过：模式 A 时期开的 UI 会话，
    /// 靠一次断线就能在模式 B 下复活。
    #[test]
    fn the_replay_path_goes_through_the_same_mode_gate() {
        let ui = PlannedSession { params: user_tone(), origin: SessionOrigin::User };
        assert_eq!(
            plan_replay(&ui, &[], MODE_A),
            ReplayAction::Open(SessionOrigin::User),
            "模式 A 下用户会话照常恢复"
        );
        assert!(
            matches!(plan_replay(&ui, &[], MODE_B), ReplayAction::SkipMode(_)),
            "模式 B 不允许 UI 开会话，一次断线也不该成为后门"
        );

        // CLI / probe 的 override 仍然通行，和 IPC 路径的判断完全一致。
        let cli = PlannedSession {
            params: OpenSessionParams { override_mode: true, ..user_tone() },
            origin: SessionOrigin::User,
        };
        assert_eq!(plan_replay(&cli, &[], MODE_B), ReplayAction::Open(SessionOrigin::User));
    }

    /// 修复不能把正常的重连恢复弄坏：UI/CLI 开的会话没有别的主人，断线后必须
    /// 靠重放回来，而且只回来一条。
    #[test]
    fn a_user_session_still_recovers_after_a_reconnect() {
        let tone = user_tone();
        let mut live = Live::default();
        live.open(&tone, SessionOrigin::User);

        let plan = live.drop_connection();
        assert_eq!(plan.len(), 1, "用户会话必须进入恢复计划——重放是它唯一的救生索");

        live.replay_pass(&plan, MODE_A);
        let same = live.matching(&tone);
        assert_eq!(same.len(), 1, "恢复一条，不多不少");
        assert_eq!(same[0].origin, SessionOrigin::User);
    }

    /// 用户会话和协调器会话在同一条连接上一起掉线：各自走各自的恢复机制，
    /// 谁也不多开、谁也不漏。
    #[test]
    fn the_two_recovery_mechanisms_do_not_overlap() {
        let spk = hal_spk();
        let tone = user_tone();
        let mut live = Live::default();
        live.coordinator_pass(0, &spk);
        live.open(&tone, SessionOrigin::User);

        let plan = live.drop_connection();
        assert_eq!(plan.len(), 1, "恢复计划里只该有那条用户会话");

        // 模式 B 下 UI 会话过不了闸门，所以这里用 override 版本代表 CLI/probe。
        let plan = vec![PlannedSession {
            params: OpenSessionParams { override_mode: true, ..tone.clone() },
            origin: SessionOrigin::User,
        }];
        live.coordinator_pass(0, &spk);
        live.replay_pass(&plan, MODE_B);

        assert_eq!(live.matching(&spk).len(), 1);
        assert_eq!(live.matching(&tone).len(), 1);
        assert_eq!(live.0.len(), 2);
    }

    /// 去重的键必须**窄**：不同方向、不同音源、不同落点是不同的链路，误判成
    /// 重复会静默吞掉一条本该恢复的会话。
    #[test]
    fn different_links_are_not_deduplicated() {
        let spk = hal_spk();
        assert!(!same_media_intent(&spk, &hal_mic()), "方向不同");
        assert!(
            !same_media_intent(&spk, &OpenSessionParams { source: Some(SOURCE_TONE.into()), ..spk.clone() }),
            "音源不同"
        );
        assert!(
            !same_media_intent(&user_tone(), &OpenSessionParams { freq: Some(880.0), ..user_tone() }),
            "探针频率不同就是两条流"
        );
        let mic = hal_mic();
        assert!(
            !same_media_intent(&mic, &OpenSessionParams { bridge: Some("BlackHole 2ch".into()), ..mic.clone() }),
            "桥接落点不同"
        );
        assert!(
            !same_media_intent(&mic, &OpenSessionParams { monitor: true, ..mic.clone() }),
            "本机监听与否不同"
        );
        assert!(
            !same_media_intent(
                &OpenSessionParams { backend: Some("wasapi".into()), ..base("spk") },
                &OpenSessionParams { backend: Some("tap".into()), ..base("spk") }
            ),
            "sysaudio 后端不同"
        );
    }

    /// `peer` 不在去重键里，这是有意的：它是个**选择器**（名字 / 指纹 / 指纹
    /// 前缀都合法），同一台对端可以有好几种写法，字面比较必然出错。调用方
    /// （`live_intents`）已经按解析后的指纹过滤过，所以两台不同对端的会话根本
    /// 不会拿来互比。
    #[test]
    fn the_peer_selector_is_not_part_of_the_key() {
        let a = hal_spk();
        let b = OpenSessionParams { peer: "aabb".to_string(), ..hal_spk() }; // 同一台，前缀写法
        assert!(same_media_intent(&a, &b));
    }
}
