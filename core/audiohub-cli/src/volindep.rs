//! `probe sysaudio --check-volume-independence` (plan §7.1 / §10 / §11.13).
//!
//! Measures whether the system-audio capture level follows this machine's
//! output volume. plan §7.1 makes that the precondition for the 「静音本机输出」
//! switch — 「捕获点位需在音量/静音之前，否则静音连镜像一起无声」——and it is
//! also why plan §7.2 can promise 「满幅传输、不做音量处理」: if the capture
//! tracked the local knob, turning the local speakers down would quietly turn
//! the peer's copy down too, which is not what the user asked for.
//!
//! # It moves a knob that belongs to the user
//!
//! Three deliberate brakes, in the order they bite:
//!
//! 1. **Off unless asked.** The whole path is behind
//!    `--check-volume-independence`; no other probe invocation reaches it.
//! 2. **It only ever turns the volume DOWN.** The loud leg is whatever the user
//!    already had. Nothing here can raise a volume, so the worst case while it
//!    runs is quieter-than-expected, never louder.
//! 3. **It always puts it back**, via [`OutputVolumeGuard`] — including on an
//!    early `?`, a panic, or a `^C` unwind.
//!
//! `--vi-device` exists for the same reason: on a machine whose default output
//! is carrying real audio, point the probe at some other output (a virtual card
//! with no speakers behind it) and the run is inaudible.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use audiohub_core::dsp;
use audiohub_core::sysaudio;
use audiohub_core::volindep::{
    judge_volume_independence, VolumeCoupling, VolumeIndependenceInput, VolumeLeg,
};
use audiohub_core::volume::OutputVolumeGuard;
use audiohub_net::media::{FrameSource, SysAudioSource};

const FRAME_MS: u32 = 10;
/// After a volume write, how long the device and the capture pipeline are given
/// to settle before a leg starts. The capture keeps being drained throughout —
/// see [`pump`] — so none of the transition survives into the next leg.
const SETTLE: Duration = Duration::from_millis(400);
/// Lead-in for the tone player: cpal stream start plus the ring's pre-roll.
const WARMUP: Duration = Duration::from_millis(800);

#[derive(clap::ValueEnum, Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExpectCoupling {
    Independent,
    Follows,
}

#[derive(clap::Args, Debug)]
pub struct ViArgs {
    /// plan §7.1: measure whether the system-audio capture follows THIS
    /// machine's output volume. CHANGES THE OUTPUT VOLUME (downwards only) and
    /// restores it. Off unless this flag is given.
    #[arg(id = "vi-check", long = "check-volume-independence")]
    pub check_volume_independence: bool,

    /// Output device whose volume is measured, by name. Omitted = the system
    /// default — which on a machine that is playing something is the user's
    /// own knob. Point this at a silent virtual card to run inaudibly.
    #[arg(id = "vi-device", long = "vi-device")]
    pub device: Option<String>,

    /// Probe tone in Hz. The level is measured narrowband at this frequency, so
    /// room noise and unrelated program audio do not count as signal. `0`
    /// switches to broadband RMS (only useful with --vi-external).
    #[arg(id = "vi-tone", long = "vi-tone", default_value_t = 1000.0)]
    pub tone: f32,

    /// Amplitude of the probe tone, 0..1.
    #[arg(id = "vi-amp", long = "vi-amp", default_value_t = 0.3)]
    pub amp: f32,

    /// Seconds per measurement leg (there are 3 or 4 of them).
    #[arg(id = "vi-secs", long = "vi-secs", default_value_t = 2.0)]
    pub secs: f32,

    /// Quiet leg = current volume x this. Must be < 1: this probe never raises
    /// a volume.
    #[arg(id = "vi-low", long = "vi-low", default_value_t = 0.25)]
    pub low: f32,

    /// Refuse to start when the device volume is below this — a knob already
    /// near zero leaves no room to turn it down measurably.
    #[arg(id = "vi-min-volume", long = "vi-min-volume", default_value_t = 0.15)]
    pub min_volume: f32,

    /// How far two levels may differ and still count as the same reading.
    #[arg(id = "vi-tolerance-db", long = "vi-tolerance-db", default_value_t = 3.0)]
    pub tolerance_db: f32,

    /// Absolute level floor. The floor actually applied is the larger of this
    /// and the measured ambient level + 12 dB.
    #[arg(id = "vi-floor", long = "vi-floor", default_value_t = 0.002)]
    pub floor: f32,

    /// Do not play the tone; measure whatever is already playing. Needed for
    /// backends that exclude our own process tree, where a tone we start
    /// cannot be heard by the capture at all.
    #[arg(id = "vi-external", long = "vi-external")]
    pub external: bool,

    /// Skip the mute leg (the leg that answers
    /// `sysaudio::capture_survives_local_mute`).
    #[arg(id = "vi-skip-mute", long = "vi-skip-mute")]
    pub skip_mute: bool,

    /// Fail the run unless the coupling comes out as this. For regression
    /// scripts: without it a probe that has stopped measuring anything still
    /// exits 0 as long as it says "inconclusive" politely.
    #[arg(id = "vi-expect", long = "vi-expect", value_enum)]
    pub expect: Option<ExpectCoupling>,

    /// Fail the run unless the mute leg comes out as this.
    #[arg(id = "vi-expect-survives-mute", long = "vi-expect-survives-mute")]
    pub expect_survives_mute: Option<bool>,
}

/// Kills the tone player on every exit path, including panic and `^C` unwind —
/// a leaked child would go on playing a 1 kHz tone into the user's speakers
/// after the probe that started it is gone.
struct TonePlayer(Child);

impl Drop for TonePlayer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl TonePlayer {
    fn start(freq: f32, amp: f32, secs: f32, device: Option<&str>) -> Result<TonePlayer> {
        // A separate PROCESS, not an in-process cpal stream: mac-catap builds
        // its tap with initStereoGlobalTapButExcludeProcesses on our own audio
        // process object, so a tone this process plays is precisely the one
        // thing the capture is guaranteed not to contain. A child has its own
        // pid and is not excluded.
        //
        // The exception is win-proc-exclude, which excludes the process TREE —
        // there the child is silent to the capture too, the ambient floor
        // catches it, and the caller is told to use --vi-external.
        let exe = std::env::current_exe().context("locating this executable to spawn the tone")?;
        let mut cmd = Command::new(exe);
        cmd.args([
            "probe",
            "tone",
            "--freq",
            &freq.to_string(),
            "--amp",
            &amp.to_string(),
            "--secs",
            &secs.to_string(),
        ]);
        if let Some(d) = device {
            cmd.args(["--device", d]);
        }
        let child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawning the tone player")?;
        Ok(TonePlayer(child))
    }
}

/// Reads the capture for `secs`, returning the samples when `keep` is set and
/// discarding them otherwise.
///
/// Settling periods go through here too rather than through `sleep`: the
/// backend keeps producing during a volume transition, `SysAudioSource` holds a
/// one-second FIFO, and a `sleep` would let the pre-transition audio sit in it
/// and bleed into the next leg's samples.
fn pump(src: &mut SysAudioSource, secs: f32, keep: bool) -> Vec<f32> {
    let mut out = Vec::new();
    let mut frame: Vec<f32> = Vec::new();
    let start = Instant::now();
    let deadline = start + Duration::from_secs_f32(secs);
    let mut tick = 0u64;
    while Instant::now() < deadline {
        let next = start + Duration::from_millis(tick * FRAME_MS as u64);
        let now = Instant::now();
        if now < next {
            std::thread::sleep(next - now);
        }
        tick += 1;
        src.next_frame(&mut frame);
        if keep {
            out.extend_from_slice(&frame);
        }
    }
    out
}

/// Keeps `level` on its historical scale: `dsp::tone_amplitude` returns the true
/// amplitude A, the old whole-leg expression returned A/√2. See `measure`'s
/// "⚠ `level` is an RMS" note — the two absolute thresholds (`--vi-floor` and
/// `signal_floor`) are pinned to the old scale.
const NARROWBAND_SCALE: f32 = std::f32::consts::FRAC_1_SQRT_2;

struct Measured {
    level: f32,
    rms: f32,
    peak: f32,
    samples: usize,
}

/// `tone_hz > 0` measures narrowband amplitude at that frequency; `0` measures
/// broadband RMS. Narrowband is the default because the alternative counts the
/// room, the user's music and the fan as signal.
///
/// # ⚠ `level` is an RMS, not an amplitude — a pre-existing 3.01 dB discrepancy
///
/// The old comment here said `sqrt(2*power)` "puts `level` on the same scale as
/// --vi-amp and as `peak`". It does not: `goertzel_power` normalises a pure tone
/// of amplitude A to `A²/4`, so `sqrt(2·A²/4)` is `A/√2` — the tone's **RMS**,
/// 3.01 dB below the amplitude it claimed to report.
///
/// This is **deliberately preserved** by [`NARROWBAND_SCALE`]. Every judgement
/// in `audiohub_core::volindep` is a dB *ratio* between legs, where a constant
/// factor cancels — but `--vi-floor` (default 0.002) and the ambient-derived
/// `signal_floor` are **absolute**, so silently moving the scale 3 dB would
/// loosen two tuned thresholds as a side effect of a measurement-validity fix.
/// Correcting it is a separate decision with its own evidence; it is not this
/// change's to make.
///
/// # Why this is windowed per 100 ms and not measured over the whole leg
///
/// The tone this reads was played by `probe tone --device`, which goes through
/// `LivePlayback::on_device` — and that path *always* carries a clock-servoed
/// variable-rate resampler, even at 48k<->48k. The servo bends the played tone
/// by however much the card's crystal is off, up to its 500 ppm clamp.
///
/// A single Goertzel over a whole 2 s leg has 0.5 Hz bins. 500 ppm on a 1 kHz
/// tone is 0.5 Hz — **exactly one bin** — so the old whole-leg single-bin read
/// landed on a null and reported a level near zero for a perfectly good tone.
/// Against a 3 dB `--vi-tol` that is not a rounding error, it is the whole
/// measurement. Chopping into 100 ms windows puts the bins at 10 Hz and
/// [`dsp::tone_amplitude`] sums the Hann main lobe, so a 500 ppm bend costs
/// nothing measurable. Median over windows also rejects the click at leg
/// boundaries. Same root cause as the `verify_tone` defect (conformance
/// §二.14); fixed here at the same time so this instrument does not have to be
/// rediscovered later.
fn measure(samples: &[f32], rate: u32, tone_hz: f32) -> Measured {
    let rms = if samples.is_empty() {
        0.0
    } else {
        (samples.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>() / samples.len() as f64)
            .sqrt() as f32
    };
    let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    let level = if tone_hz > 0.0 && !samples.is_empty() {
        let win = (rate / 10) as usize; // 100 ms -> 10 Hz bins
        let mut levels: Vec<f32> = samples
            .chunks(win.max(1))
            .filter(|c| c.len() == win.max(1))
            .map(|c| dsp::tone_amplitude(c, rate, tone_hz) * NARROWBAND_SCALE)
            .collect();
        if levels.is_empty() {
            // Leg shorter than one window: fall back to the whole buffer. The
            // CLI rejects `--vi-secs <= 0.5`, so this is unreachable in practice.
            dsp::tone_amplitude(samples, rate, tone_hz) * NARROWBAND_SCALE
        } else {
            levels.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            levels[levels.len() / 2]
        }
    } else {
        rms
    };
    Measured {
        level,
        rms,
        peak,
        samples: samples.len(),
    }
}

fn leg_json(name: &str, scalar: f32, muted: bool, m: &Measured) -> serde_json::Value {
    serde_json::json!({
        "leg": name,
        "scalar": scalar,
        "muted": muted,
        "level": m.level,
        "rms": m.rms,
        "peak": m.peak,
        "samples": m.samples,
    })
}

pub fn run(args: &ViArgs, backend: &str, json: bool) -> Result<i32> {
    if !(args.low > 0.0 && args.low < 1.0) {
        bail!(
            "--vi-low must be in (0,1): this probe only ever turns the volume DOWN, \
             and {} would {}",
            args.low,
            if args.low >= 1.0 { "raise it" } else { "mute it" }
        );
    }
    if args.secs <= 0.5 {
        bail!("--vi-secs must exceed 0.5s; the tone detector discards the first 200ms of each leg");
    }
    if args.external && args.tone > 0.0 {
        crate::info(&format!(
            "--vi-external: playing nothing, measuring whatever is already producing {} Hz",
            args.tone
        ));
    }

    let info = sysaudio::resolve_backend(backend)?;
    if !info.available {
        bail!(
            "sysaudio backend '{}' is not available: {}",
            info.id,
            info.note
        );
    }
    if info.excludes_self && !args.external {
        crate::info(&format!(
            "note: backend {} excludes this process; the tone is played from a CHILD \
             process so the capture can hear it",
            info.id
        ));
    }

    let dev = args.device.as_deref();
    match dev {
        None => crate::info(
            "WARNING: no --vi-device, so this moves the SYSTEM DEFAULT output volume — \
             on a machine that is playing something, that is the user's own knob. \
             It is restored, but it is audible while the probe runs.",
        ),
        Some(d) => crate::info(&format!("volume under test: output device {d:?}")),
    }

    let mut guard = OutputVolumeGuard::capture(dev)?;
    let original = guard.original();
    if original.muted {
        bail!(
            "output device is already muted (scalar {:.3}); unmute it before measuring, \
             a muted baseline makes every leg silent",
            original.scalar
        );
    }
    if original.scalar < args.min_volume {
        bail!(
            "output volume is {:.3}, below --vi-min-volume {:.3}: there is no room to turn \
             it down by a measurable amount, and this probe will not turn it up",
            original.scalar,
            args.min_volume
        );
    }

    let rate = SysAudioSource::OUT_RATE;
    let mut src = SysAudioSource::new(FRAME_MS, backend)?;
    let capture_rate = src.capture_rate();
    crate::info(&format!(
        "backend {} ({} Hz), volume {:.3}, legs {:.1}s each",
        info.id, capture_rate, original.scalar, args.secs
    ));

    // Leg 0 — ambient, tone not running. This is the number that stops silence
    // from being read as independence.
    crate::info("leg ambient: nothing playing from us");
    let ambient = measure(&pump(&mut src, args.secs, true), rate, args.tone);

    let player = if args.external {
        None
    } else {
        // One player for every remaining leg: restarting it between legs would
        // put a stream start inside each measurement.
        let need = WARMUP.as_secs_f32()
            + args.secs * 3.0
            + SETTLE.as_secs_f32() * 3.0
            + 3.0;
        Some(TonePlayer::start(
            args.tone,
            args.amp,
            need,
            args.device.as_deref(),
        )?)
    };
    let _ = pump(&mut src, WARMUP.as_secs_f32(), false);

    // Leg 1 — baseline, volume exactly as the user left it.
    crate::info(&format!("leg baseline: volume {:.3}", original.scalar));
    let baseline = measure(&pump(&mut src, args.secs, true), rate, args.tone);

    // Leg 2 — quieter. `set_volume` returns the READ-BACK scalar: macOS
    // quantises, so the prediction has to be made from what the device took.
    let want_low = original.scalar * args.low;
    let low_state = guard.set_volume(want_low)?;
    crate::info(&format!(
        "leg low: asked {want_low:.3}, device took {:.3}",
        low_state.scalar
    ));
    let _ = pump(&mut src, SETTLE.as_secs_f32(), false);
    let low = measure(&pump(&mut src, args.secs, true), rate, args.tone);

    // Leg 3 — muted, back at the original volume.
    let restored = guard.set_volume(original.scalar)?;
    let _ = pump(&mut src, SETTLE.as_secs_f32(), false);
    let mute_leg = if args.skip_mute {
        None
    } else {
        let st = guard.set_mute(true)?;
        crate::info(&format!("leg mute: volume {:.3} muted={}", st.scalar, st.muted));
        let _ = pump(&mut src, SETTLE.as_secs_f32(), false);
        let m = measure(&pump(&mut src, args.secs, true), rate, args.tone);
        Some((st, m))
    };

    // Explicit, so a restore that FAILED is reported. The Drop underneath is
    // the net for the paths that never get here.
    guard.restore().context("restoring the output volume")?;
    drop(player);
    drop(src);
    let after = audiohub_core::volume::get_output_volume(dev)?;
    crate::info(&format!(
        "restored: scalar {:.3} muted={} (was {:.3}/{})",
        after.scalar, after.muted, original.scalar, original.muted
    ));

    let input = VolumeIndependenceInput {
        backend_id: &info.id,
        ambient_level: ambient.level,
        baseline: VolumeLeg {
            scalar: original.scalar,
            level: baseline.level,
        },
        low: VolumeLeg {
            scalar: low_state.scalar,
            level: low.level,
        },
        muted: mute_leg.as_ref().map(|(st, m)| VolumeLeg {
            scalar: st.scalar,
            level: m.level,
        }),
        tolerance_db: args.tolerance_db,
        floor: args.floor,
    };
    let report = judge_volume_independence(&input);

    crate::info(&format!(
        "ambient={:.6} baseline={:.6} low={:.6} mute={:?} floor={:.6}",
        ambient.level, baseline.level, low.level, report.mute_level, report.signal_floor
    ));
    crate::info(&format!(
        "observed {:.2}dB vs independent 0.00dB / follows {:.2}dB (tolerance {:.2}dB)",
        report.observed_low_db, report.predicted_low_db, report.tolerance_db
    ));
    crate::info(&format!(
        "coupling={:?} survives_mute={:?} table_says={:?} conclusive={}",
        report.coupling, report.survives_mute, report.table_says, report.conclusive
    ));
    for n in &report.notes {
        crate::info(&format!("note: {n}"));
    }

    let mut failures: Vec<String> = Vec::new();
    if !report.conclusive {
        failures.push("no verdict was reached".to_string());
    }
    if report.contradicts_table {
        failures.push(format!(
            "measurement contradicts sysaudio::capture_survives_local_mute({:?})",
            info.id
        ));
    }
    if let Some(want) = args.expect {
        let got = match report.coupling {
            VolumeCoupling::Independent => Some(ExpectCoupling::Independent),
            VolumeCoupling::Follows => Some(ExpectCoupling::Follows),
            VolumeCoupling::Inconclusive => None,
        };
        if got != Some(want) {
            failures.push(format!("--vi-expect {want:?} but measured {:?}", report.coupling));
        }
    }
    if let Some(want) = args.expect_survives_mute {
        if report.survives_mute != Some(want) {
            failures.push(format!(
                "--vi-expect-survives-mute {want} but measured {:?}",
                report.survives_mute
            ));
        }
    }
    for f in &failures {
        crate::info(&format!("FAIL: {f}"));
    }

    let ok = failures.is_empty();
    crate::emit_json(
        json,
        &serde_json::json!({
            "check": "volume-independence",
            "backend": info.id,
            "excludes_self": info.excludes_self,
            "capture_rate": capture_rate,
            "sample_rate": rate,
            "device": args.device,
            "tone_hz": args.tone,
            "external": args.external,
            "legs": {
                "ambient": leg_json("ambient", original.scalar, false, &ambient),
                "baseline": leg_json("baseline", original.scalar, false, &baseline),
                "low": leg_json("low", low_state.scalar, low_state.muted, &low),
                "mute": mute_leg
                    .as_ref()
                    .map(|(st, m)| leg_json("mute", st.scalar, st.muted, m)),
            },
            "restore": {
                "original_scalar": original.scalar,
                "original_muted": original.muted,
                "mid_run_scalar": restored.scalar,
                "after_scalar": after.scalar,
                "after_muted": after.muted,
                "clean": (after.scalar - original.scalar).abs() <= 0.02
                    && after.muted == original.muted,
            },
            "report": report,
            "failures": failures,
            "ok": ok,
        }),
    );
    Ok(if ok { 0 } else { crate::EXIT_CHECK_FAILED })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    /// Conformance §二.14, generalised to this instrument.
    ///
    /// `measure` is the *worst* instance of the single-bin defect in the tree,
    /// not the mildest: it used to run one Goertzel over a whole `--vi-secs`
    /// leg, so its bins were `1/secs` Hz wide — 0.5 Hz at the 2 s default.
    /// The play servo's own clamp (500 ppm) moves a 1 kHz tone by 0.5 Hz, i.e.
    /// **exactly one bin**, straight onto a null. `--vi-tol` is 3 dB; the error
    /// this could produce is unbounded.
    ///
    /// Injection control: revert `measure`'s `level` arm to
    /// `(2.0 * dsp::goertzel_power(samples, rate, tone_hz)).max(0.0).sqrt()`
    /// and this goes red at the 500 ppm row (measured −41 dB of level error).
    #[test]
    fn the_measured_level_survives_a_legal_play_servo_bend() {
        const SR: u32 = 48_000;
        const AMP: f32 = 0.3; // --vi-amp default
        let secs = 2.0f32; // --vi-secs default
        let n = (SR as f32 * secs) as usize;
        // --vi-tol default is 3 dB and the two hypotheses sit 12 dB apart, so a
        // full decibel of instrument error is already a third of the budget.
        const MAX_ERR_DB: f32 = 0.5;
        for &ppm in &[0.0f32, 100.0, -100.0, 300.0, -300.0, 500.0, -500.0] {
            let bent = 1000.0f32 * (1.0 + ppm * 1e-6);
            let m = super::measure(&audiohub_core::dsp::gen_sine(bent, SR, n, AMP), SR, 1000.0);
            // Historical scale: `level` is the tone's RMS (see NARROWBAND_SCALE).
            let want = AMP * super::NARROWBAND_SCALE;
            let err_db = 20.0 * (m.level / want).log10();
            assert!(
                err_db.abs() <= MAX_ERR_DB,
                "a {ppm} ppm bend (a legal play-servo correction) moved the measured \
                 level by {err_db:.2} dB (read {:.5}, expected {want:.5}). Against a 3 dB \
                 --vi-tol that is the measurement, not a rounding error — this is \
                 the §二.14 defect in the volume-independence probe.",
                m.level
            );
        }
        // The rejection side still has to work, or the above proves nothing:
        // a tone 200 Hz away must NOT be read as level.
        let off = super::measure(&audiohub_core::dsp::gen_sine(1200.0, SR, n, AMP), SR, 1000.0);
        assert!(
            off.level < AMP * super::NARROWBAND_SCALE * 0.05,
            "a 1200 Hz tone read {:.5} when asked for 1000 Hz; the passband is too wide",
            off.level
        );
    }

    /// Production source only. The `#[cfg(test)]` tail is cut off because these
    /// assertions name the very strings they forbid, and a scan that included
    /// itself would fail on its own needles.
    fn read(rel: &str) -> String {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src").join(rel);
        let src = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!("cannot read {} ({e}); update this test if the file moved", path.display())
        });
        let marker = "#[cfg(test)]";
        match src.find(marker) {
            Some(i) => src[..i].to_string(),
            None => src,
        }
    }

    /// # Why this group reads source instead of running the probe
    ///
    /// The thing under test is "the user's volume knob always comes back".
    /// Exercising it for real means turning this machine's volume down and
    /// muting its speakers inside a `cargo test` — on a host that is currently
    /// carrying a live mode B session, that is not a test environment, that is
    /// the scene. The verdict logic is a pure function tested in
    /// `audiohub_core::volindep`; what is left unwatched is whether the restore
    /// path is still wired the way it has to be, and that is what these check.
    ///
    /// A raw `volume::set_output_volume` in this file would work perfectly in
    /// every test and leave the user's volume moved the first time a leg
    /// panicked. That is the regression these exist to catch.
    #[test]
    fn every_volume_write_goes_through_the_restoring_guard() {
        let src = read("volindep.rs");
        assert!(
            src.contains("OutputVolumeGuard::capture(dev)?"),
            "the probe no longer takes an OutputVolumeGuard — nothing restores the volume \
             on an early return or a panic"
        );
        assert!(
            src.contains("guard.set_volume(") && src.contains("guard.set_mute("),
            "volume/mute writes must go through the guard, not through volume::set_* directly"
        );
        assert!(
            src.contains("guard.restore().context("),
            "restore() must be called explicitly and its error surfaced; Drop cannot report \
             that putting the volume back FAILED"
        );
        for direct in ["volume::set_output_volume(", "volume::set_output_mute("] {
            assert!(
                !src.contains(direct),
                "{direct} is called directly here, bypassing the guard's panic-path restore"
            );
        }
    }

    /// plan §7.1's brake: the loud leg is whatever the user already had, and
    /// the only volume this probe ever writes is a fraction of it. Nothing here
    /// may turn a volume UP.
    #[test]
    fn the_probe_can_only_ever_turn_the_volume_down() {
        let src = read("volindep.rs");
        assert!(
            src.contains("let want_low = original.scalar * args.low;"),
            "the quiet leg must be derived from the CURRENT volume, not from an absolute \
             scalar that could exceed it"
        );
        assert!(
            src.contains("if !(args.low > 0.0 && args.low < 1.0)"),
            "--vi-low must be range-checked; low >= 1.0 would raise the user's volume"
        );
    }

    /// The tone player is a child process, and a child that outlives the probe
    /// goes on playing 1 kHz into the speakers with nobody left to stop it.
    #[test]
    fn the_tone_player_is_killed_on_every_exit_path() {
        let src = read("volindep.rs");
        assert!(
            src.contains("impl Drop for TonePlayer"),
            "TonePlayer has no Drop: a panic mid-run would leave the tone playing"
        );
    }

    /// The probe must not be reachable without asking for it by name.
    #[test]
    fn the_check_is_off_unless_explicitly_requested() {
        let main = read("main.rs");
        assert!(
            main.contains("if vi.check_volume_independence {"),
            "probe sysaudio must only enter the volume path behind \
             --check-volume-independence"
        );
    }
}
