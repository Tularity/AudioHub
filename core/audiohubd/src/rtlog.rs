//! 截止期线程的**延迟落盘**日志：入队在音频线程，`write` 在别的线程。
//!
//! # 病在哪
//!
//! `crate::logln` 是一次 `stderr().write_all()`。那是**两件**都不该出现在 10 ms
//! 音频节拍上的事：
//!
//! 1. **阻塞的 `write(2)`**。stderr 通常是 LaunchDaemon 重定向出来的文件或管道。
//!    文件时它可能撞上 page cache 回写、fsync 风暴、APFS 快照；管道时对端不读
//!    就是 64 KB 缓冲写满后**直接睡下去**。上界不可预知。
//! 2. **抢 `Stderr` 的进程级锁**。`std::io::stderr()` 返回的句柄内部是一把全局
//!    `ReentrantLock`。任何一个普通优先级线程（控制面、ticker、IPC）在 `write`
//!    里被抢占，音频线程就要陪它等一个调度量子。
//!
//! `HalSpeakerSource::note_short` 的令牌桶注释早就写明了这一点，并据此给欠载
//! 日志加了限流——那是**绕开**，不是解决：限流的代价是「被限流」与「没发生」
//! 从此需要额外的计数器才分得开。本模块把成因去掉，限流因此可以保持原样、
//! 而它的语义（稀疏时全打、病理时压掉并报出压了多少）一个字都不用改。
//!
//! # 怎么做的
//!
//! 每条截止期线程在进循环之前调一次 [`arm`]，之后**这条线程上的每一次
//! `dlog!` 都自动改走入队**——调用点一个字都不用改，也就不存在「漏改一处」。
//! 一条 [`SpscRing`] 的槽是预先造好的 `String`，入队 = `clear()` + 就地
//! `write_fmt` + 一次 Release store。零锁、零系统调用，稳态零分配。
//!
//! 时间戳在**入队**时取（`crate::log_uptime()`），不是落盘时取。否则日志行的
//! 时刻会变成「写手线程什么时候醒的」，而 `crate::logln` 的文档记着那条时基是
//! 用来和外部采样对齐的——把它换成落盘时刻，等于悄悄毁掉一个已知有人在用的
//! 观测手段。
//!
//! # 代价，如实写在这里
//!
//! - **行序**：来自不同线程的行按各自入队时刻标注，但落盘顺序取决于写手的
//!   轮询。逐行时间戳仍然正确且单调可排序，`sort` 一下就还原。
//! - **丢行**：环满（256 行/线程）就丢，并**计数**；写手发现计数涨了会补一行
//!   `[rtlog] 线程 X 丢了 N 行`。今天的令牌桶是**静默**压制到只剩计数器，
//!   所以这一条是观测性净增。
//! - **进程被 `abort`/SIGKILL**：最后 ≤20 ms 的行会丢。panic 不受影响
//!   （写手是另一条线程，照常把队列排干）。

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::rtsafe::SpscRing;

/// 每条截止期线程的队列深度（行）。
///
/// 256 行 × 512 B ≈ 128 KB/线程。按现场速率（欠载日志 21 小时 30 段）这个环
/// 永远是空的；它的容量只服务于**病理**情形——「环深度贴着 0 抖动」那种
/// 每 20 ms 两行的形态，256 行给写手 2.5 秒的追赶余量。
const LINES: usize = 256;

/// 每个槽预留的字节数。超长的行会让那个槽的 `String` 长一次（此后不再长），
/// 所以这是**软**上界，不是截断。现场最长的一行（欠载段首）约 240 B。
const LINE_BYTES: usize = 512;

/// 写手的轮询间隔。
///
/// **刻意不让生产者叫醒写手**：`Thread::unpark` 在 Darwin 上是
/// `pthread_mutex_lock` + `__psynch_cvsignal`，虽然有界，但日志根本不需要
/// 那点及时性——落盘晚 20 ms 对任何一种排查都无所谓。省下的是截止期线程上
/// 每 tick 一次的系统调用。
///
/// （UDP 发送线程是另一回事：那一条在媒体路径上，20 ms 的轮询会直接变成
/// `network` 一级的抖动，所以那边**必须**用 `unpark`。两处的取舍不同是刻意的。）
const POLL: Duration = Duration::from_millis(20);

struct LogLine {
    at: Duration,
    text: String,
}

struct Queue {
    /// 线程名，只为把「谁丢了行」说清楚。
    who: String,
    ring: SpscRing<LogLine>,
    /// 写手上次见到的丢行数。**只由写手写**。
    seen_rejected: AtomicU64,
}

/// 所有已武装线程的队列。注册只在线程启动时发生一次，不在任何热路径上。
static QUEUES: Mutex<Vec<Arc<Queue>>> = Mutex::new(Vec::new());

thread_local! {
    /// 本线程的队列。`None` = 没武装 ⇒ `logln` 照旧直接写。
    ///
    /// `const` 初始化：走 `#[thread_local]`，读一次是一条指令，**不做惰性
    /// 初始化、不分配**。这一点是必须的——`logln` 在每条线程上都会被调用，
    /// 包括那些一辈子只记一行日志的线程。
    static LOCAL: std::cell::RefCell<Option<Arc<Queue>>> = const { std::cell::RefCell::new(None) };
}

/// 把**本线程**的 `dlog!` 切到延迟落盘。截止期线程进循环之前调一次。
///
/// 幂等：重复调用只换掉队列（不会注册两份）。
pub(crate) fn arm(who: &str) {
    let q = arm_local(who);
    let mut all = crate::lk(&QUEUES);
    all.push(q);
}

/// `arm` 的前一半：只装到本线程的 TLS 上，**不**注册给写手。
///
/// 单测用这一半。理由不是洁癖：同一个测试进程里另有几条测试会起真 daemon，
/// 那会拉起 `ahb-rtlog` 写手，而写手会把**所有已注册队列**排干——包括测试
/// 刚塞进去还没来得及检查的那几行。用注册版写断言，等于让一条并发的
/// daemon 测试随机把这里判红。
fn arm_local(who: &str) -> Arc<Queue> {
    let q = Arc::new(Queue {
        who: who.to_string(),
        ring: SpscRing::new(LINES, |_| LogLine {
            at: Duration::ZERO,
            text: String::with_capacity(LINE_BYTES),
        }),
        seen_rejected: AtomicU64::new(0),
    });
    let prev = LOCAL.with(|c| c.borrow_mut().replace(q.clone()));
    if let Some(p) = prev {
        crate::lk(&QUEUES).retain(|x| !Arc::ptr_eq(x, &p));
    }
    q
}

/// 解除武装（测试用；生产里线程活到进程结束，不需要）。
#[cfg(test)]
pub(crate) fn disarm() {
    if let Some(p) = LOCAL.with(|c| c.borrow_mut().take()) {
        crate::lk(&QUEUES).retain(|x| !Arc::ptr_eq(x, &p));
    }
}

/// `crate::logln` 的分流点。返回 `true` = 已入队，调用方**不要**再写 stderr。
///
/// 环满时同样返回 `true`：满了就退回阻塞 `write`，等于把本模块要消灭的东西在
/// 最坏的时刻（日志正在爆发）放回截止期线程。丢行由 `SpscRing` 计数，写手负责
/// 报出来。
pub(crate) fn try_defer(at: Duration, args: fmt::Arguments<'_>) -> bool {
    LOCAL
        .try_with(|c| {
            // `try_borrow`：`arm` 之外没有人会在借用期间记日志，但一次
            // 意外的重入不值得 panic 掉一条 daemon 线程。
            let Ok(b) = c.try_borrow() else { return false };
            let Some(q) = b.as_ref() else { return false };
            q.ring.produce(|line| {
                line.at = at;
                line.text.clear();
                let _ = fmt::Write::write_fmt(&mut line.text, args);
                true
            });
            true
        })
        .unwrap_or(false)
}

/// 把所有队列排干，返回落盘的行数。写手线程与关机路径共用。
fn drain_all(scratch: &mut String) -> usize {
    // 队列表只在注册时被改，这里拿一份快照就放锁：写 stderr 的过程中绝不能还
    // 持着注册表的锁，那会让「起一条新线程」被日志阻塞。
    let queues: Vec<Arc<Queue>> = crate::lk(&QUEUES).clone();
    let mut n = 0;
    for q in &queues {
        loop {
            let mut at = Duration::ZERO;
            // 拷进**写手自己的**缓冲，而不是把槽里的 `String` 拿走：拿走会让
            // 槽退回空串，下一次 `produce` 就得重新为它分配容量 —— 那正好把
            // 「稳态零分配」还给了截止期线程。
            let got = q.ring.consume(|l| {
                at = l.at;
                scratch.clear();
                scratch.push_str(&l.text);
            });
            if !got {
                break;
            }
            crate::logln_direct(at, format_args!("{scratch}"));
            n += 1;
        }
        let d = q.ring.rejected();
        let seen = q.seen_rejected.load(Ordering::Relaxed);
        if d > seen {
            q.seen_rejected.store(d, Ordering::Relaxed);
            crate::logln_direct(
                crate::log_uptime(),
                format_args!(
                    "[rtlog] 线程 {} 的日志队列满了，丢了 {} 行（累计 {d}）",
                    q.who,
                    d - seen
                ),
            );
            n += 1;
        }
    }
    n
}

/// 回收已经死掉的线程留下的队列。
///
/// 一条 daemon 线程活到进程结束，所以生产环境里这件事不会发生；但**测试进程**
/// 会在同一个进程里起停十几台 daemon，不回收就是每台 2×128 KB 的泄漏。
///
/// 判据：全局表是唯一持有者（TLS 那一份随线程一起没了）**且**环已经排空。
/// 后半句是承重的——线程死前记的最后几行还在环里，先扔队列就等于丢掉
/// 「它到底怎么没的」那几行，而那正是最该留的几行。
fn reap_dead_queues() {
    crate::lk(&QUEUES).retain(|q| Arc::strong_count(q) > 1 || q.ring.len() > 0);
}

/// 落盘线程。`stop` 为真且队列排空后返回。
pub(crate) fn writer_loop(stop: impl Fn() -> bool) {
    let mut scratch = String::with_capacity(LINE_BYTES);
    loop {
        let n = drain_all(&mut scratch);
        reap_dead_queues();
        if stop() {
            // 关机宽限：`stop()` 为真的那一刻，两条音频循环多半还在跑最后几个
            // tick，而它们退出前记的行（「跳 tick」「欠载结束」）恰恰是排查
            // 关机期问题唯一的线索。连排到两轮全空为止，上限 500 ms。
            let deadline = std::time::Instant::now() + Duration::from_millis(500);
            let mut quiet = 0;
            while quiet < 2 && std::time::Instant::now() < deadline {
                if drain_all(&mut scratch) == 0 {
                    quiet += 1;
                } else {
                    quiet = 0;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            return;
        }
        if n == 0 {
            std::thread::sleep(POLL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 武装之后，`dlog!` **不能**再落到 stderr，而必须能从队列里原样取回来。
    ///
    /// 注入对照：把 `crate::logln` 里的 `if rtlog::try_defer(..) { return; }`
    /// 删掉（= 回到直接写），`queued` 会是 0，本条变红。
    #[test]
    fn arming_a_thread_moves_its_logging_into_the_queue() {
        let q = arm_local("test-armed");
        crate::dlog!("[test] 一行 {}", 42);
        crate::dlog!("[test] 又一行");
        let mut queued: Vec<String> = Vec::new();
        while q.ring.consume(|l| queued.push(l.text.clone())) {}
        disarm();
        assert_eq!(
            queued,
            vec!["[test] 一行 42".to_string(), "[test] 又一行".to_string()],
            "武装后的 dlog! 没有进队列 —— 阻塞 write 还留在截止期线程上"
        );
        // 解除武装之后回到直接写（不校验 stderr 内容，只校验不再入队）。
        assert!(
            !try_defer(Duration::ZERO, format_args!("x")),
            "disarm 之后仍在入队"
        );
    }

    /// 队列满了要丢并**计数**，绝不阻塞、绝不退回阻塞 write。
    #[test]
    fn a_full_queue_drops_and_counts_instead_of_writing_inline() {
        let q = arm_local("test-full");
        for i in 0..(LINES + 7) {
            assert!(
                try_defer(Duration::ZERO, format_args!("line {i}")),
                "第 {i} 行退回了直接写 —— 日志爆发时正是最不能这么做的时刻"
            );
        }
        let rejected = q.ring.rejected();
        disarm();
        assert_eq!(rejected, 7, "丢行没有被计数");
    }

    /// 时间戳必须是**入队**时刻，不是落盘时刻。
    #[test]
    fn the_timestamp_is_taken_when_the_line_is_queued() {
        let q = arm_local("test-ts");
        try_defer(Duration::from_secs(7), format_args!("old line"));
        std::thread::sleep(Duration::from_millis(5));
        let mut at = Duration::MAX;
        q.ring.consume(|l| at = l.at);
        disarm();
        assert_eq!(at, Duration::from_secs(7), "时间戳被落盘时刻覆盖了");
    }
}
