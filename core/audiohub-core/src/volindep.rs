//! plan §7.1: does this machine's output volume reach the system-audio
//! capture, or does the capture sit upstream of it?
//!
//! The answer is the precondition plan §7.1 puts on 「静音本机输出」——
//! 「捕获点位需在音量/静音之前，否则静音连镜像一起无声」——and it is per
//! backend, not per platform. `sysaudio::capture_survives_local_mute` records
//! what plan EXPECTS; this module is how a measurement either confirms or
//! contradicts it.
//!
//! Split out of the measurement on purpose: the rules below decide what the
//! §7.1 table says, so they have to be exercisable on a build machine with no
//! speakers, no volume knob, and no consent prompt. `probe sysaudio
//! --check-volume-independence` supplies the four levels; everything that turns
//! levels into a verdict is here.

use crate::sysaudio::capture_survives_local_mute;

/// How a backend's capture level responded to this machine's output volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeCoupling {
    /// The level did not move when the volume did: the capture point sits
    /// UPSTREAM of the volume control — plan §7.1's 「捕获点位需在音量/静音之前」.
    Independent,
    /// The level tracked the volume one-for-one: the capture point is
    /// downstream of it (post-mix).
    Follows,
    /// Neither hypothesis fits the numbers, or the run could not tell them
    /// apart. **Never** a synonym for `Independent`: the commonest way to
    /// measure "the level did not change" is to measure nothing at all.
    Inconclusive,
}

/// One measurement leg: the volume the device was actually at, and the level
/// captured while it was there.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct VolumeLeg {
    /// Scalar the device READ BACK, not the one that was requested — macOS
    /// quantises several built-in outputs to 1/16 steps, and predicting the
    /// level drop from the requested value predicts from a number the hardware
    /// never used.
    pub scalar: f32,
    /// Linear level captured during the leg: narrowband amplitude at the probe
    /// tone, or broadband RMS when the caller measured ambient program audio.
    pub level: f32,
}

/// Everything [`judge_volume_independence`] needs. Separated from the
/// measurement so the decision can be tested without an audio device — the
/// rules below are the part that decides what the plan §7.1 table says, and
/// they must be exercisable on a machine with no speakers.
#[derive(Debug, Clone)]
pub struct VolumeIndependenceInput<'a> {
    pub backend_id: &'a str,
    /// Level measured with the tone generator OFF. The verdict has to beat this
    /// by [`AMBIENT_MARGIN_DB`] or it is measuring the room, not the signal.
    pub ambient_level: f32,
    /// Volume left where the user had it.
    pub baseline: VolumeLeg,
    /// Volume turned DOWN. Never up: this runs on machines carrying somebody's
    /// real audio.
    pub low: VolumeLeg,
    /// The mute leg, or `None` when the caller skipped it.
    pub muted: Option<VolumeLeg>,
    /// How far two levels may differ and still count as the same reading.
    pub tolerance_db: f32,
    /// Absolute level floor. The floor actually applied is the larger of this
    /// and the ambient level plus [`AMBIENT_MARGIN_DB`].
    pub floor: f32,
}

/// How far the tone must sit above the ambient reading before any leg counts.
/// 12 dB = 4× in amplitude: enough that a room, a fan, or a neighbouring app
/// cannot be mistaken for the tone, and low enough that a quiet but real tone
/// still qualifies.
pub const AMBIENT_MARGIN_DB: f32 = 12.0;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VolumeIndependenceReport {
    pub backend_id: String,
    pub coupling: VolumeCoupling,
    /// Measured answer to `capture_survives_local_mute`'s question.
    /// `None` = the mute leg was skipped or did not decide.
    pub survives_mute: Option<bool>,
    /// The floor every leg was judged against.
    pub signal_floor: f32,
    pub ambient_level: f32,
    pub baseline_level: f32,
    pub low_level: f32,
    pub mute_level: Option<f32>,
    /// 20·log10(low / baseline) — what was measured.
    pub observed_low_db: f32,
    /// 20·log10(low_scalar / baseline_scalar) — what a post-mix capture would
    /// have shown. The two hypotheses of this probe are `0 dB` and this value.
    pub predicted_low_db: f32,
    pub observed_mute_db: Option<f32>,
    pub tolerance_db: f32,
    /// What [`capture_survives_local_mute`] claims for this backend today.
    pub table_says: Option<bool>,
    /// The mute leg measured the OPPOSITE of `table_says`. A true here is the
    /// whole reason this probe exists: somebody has to move that row.
    pub contradicts_table: bool,
    pub notes: Vec<String>,
    /// A verdict was reached — NOT "the backend is independent". `Follows` is
    /// a perfectly conclusive run, and is the expected answer for
    /// `win-device-loopback`.
    pub conclusive: bool,
}

/// Amplitude ratio in dB, floored so a silent leg yields a large negative
/// number instead of −inf/NaN (both of which compare false against everything
/// and would turn a dead capture into a passed test).
fn ratio_db(num: f32, den: f32) -> f32 {
    const EPS: f32 = 1e-9;
    20.0 * (num.max(EPS) / den.max(EPS)).log10()
}

/// Decides plan §7.1's question from three (or four) measured levels.
///
/// Two hypotheses, and only two: the capture sits **before** the volume control
/// (turning it down changes nothing, observed ≈ 0 dB) or **after** it (the
/// level follows, observed ≈ `predicted_low_db`). Anything that fits neither —
/// including the case where the two legs' volumes were too close for the
/// hypotheses to differ by more than the tolerance — comes back
/// [`VolumeCoupling::Inconclusive`] with a note saying why. That is the point:
/// a run with nothing playing produces two equal levels, and "equal" is exactly
/// what independence looks like. The ambient leg and the floor exist so that
/// run reports "no signal" instead of "independent".
pub fn judge_volume_independence(input: &VolumeIndependenceInput) -> VolumeIndependenceReport {
    let mut notes: Vec<String> = Vec::new();
    let tol = input.tolerance_db.abs();
    let margin = 10f32.powf(AMBIENT_MARGIN_DB / 20.0);
    let signal_floor = input.floor.max(input.ambient_level * margin);

    let observed_low_db = ratio_db(input.low.level, input.baseline.level);
    let predicted_low_db = ratio_db(input.low.scalar, input.baseline.scalar);
    let observed_mute_db = input.muted.map(|m| ratio_db(m.level, input.baseline.level));

    let mut coupling = VolumeCoupling::Inconclusive;
    let mut survives_mute: Option<bool> = None;

    if !(input.baseline.level >= signal_floor) {
        // Also catches NaN. Everything downstream is undecidable, and saying so
        // is the single most important thing this function does.
        notes.push(format!(
            "no signal: baseline level {:.6} is below the floor {:.6} \
             (absolute {:.6}, ambient {:.6} + {AMBIENT_MARGIN_DB:.0}dB). \
             Nothing was measured, so nothing is concluded — check that audio is \
             actually playing, and that this backend does not exclude the player.",
            input.baseline.level, signal_floor, input.floor, input.ambient_level
        ));
    } else if predicted_low_db > -2.0 * tol {
        notes.push(format!(
            "legs not separable: volume went {:.3} -> {:.3}, so a post-mix capture \
             would drop only {predicted_low_db:.2}dB, which is not more than \
             2x the {tol:.2}dB tolerance away from the 0dB an upstream capture \
             would show. Lower --vi-low or raise the device volume first.",
            input.baseline.scalar, input.low.scalar
        ));
    } else {
        let flat_err = observed_low_db.abs();
        let follow_err = (observed_low_db - predicted_low_db).abs();
        if flat_err <= tol {
            coupling = VolumeCoupling::Independent;
        } else if follow_err <= tol {
            coupling = VolumeCoupling::Follows;
        } else {
            notes.push(format!(
                "neither hypothesis fits: observed {observed_low_db:.2}dB is \
                 {flat_err:.2}dB from 'independent' (0dB) and {follow_err:.2}dB from \
                 'follows' ({predicted_low_db:.2}dB), both outside the {tol:.2}dB \
                 tolerance. Something else moved during the run."
            ));
        }

        if let (Some(m), Some(db)) = (input.muted, observed_mute_db) {
            if m.level >= signal_floor && db.abs() <= tol {
                survives_mute = Some(true);
            } else if m.level < signal_floor {
                survives_mute = Some(false);
            } else {
                notes.push(format!(
                    "mute leg undecided: level {:.6} is above the floor {signal_floor:.6} \
                     but {db:.2}dB below baseline, so the mute neither passed the \
                     capture through nor silenced it.",
                    m.level
                ));
            }
        }
    }

    if coupling == VolumeCoupling::Follows && survives_mute == Some(true) {
        notes.push(
            "self-inconsistent: the level follows the volume yet survives the mute. \
             On a device whose mute is the volume stage this cannot both be true."
                .to_string(),
        );
    }

    let table_says = capture_survives_local_mute(input.backend_id);
    let contradicts_table = match (table_says, survives_mute) {
        (Some(t), Some(m)) => t != m,
        _ => false,
    };
    if contradicts_table {
        notes.push(format!(
            "CONTRADICTS the plan §7.1 table: capture_survives_local_mute({:?}) says {:?}, \
             this run measured {:?}. One run on one host does not rewrite the table — \
             report it and have a human move the row.",
            input.backend_id,
            table_says.expect("checked"),
            survives_mute.expect("checked"),
        ));
    }

    let conclusive = coupling != VolumeCoupling::Inconclusive
        && (input.muted.is_none() || survives_mute.is_some());

    VolumeIndependenceReport {
        backend_id: input.backend_id.to_string(),
        coupling,
        survives_mute,
        signal_floor,
        ambient_level: input.ambient_level,
        baseline_level: input.baseline.level,
        low_level: input.low.level,
        mute_level: input.muted.map(|m| m.level),
        observed_low_db,
        predicted_low_db,
        observed_mute_db,
        tolerance_db: tol,
        table_says,
        contradicts_table,
        notes,
        conclusive,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sysaudio::{
        BACKEND_MAC_CATAP, BACKEND_WIN_DEVICE_LOOPBACK, BACKEND_WIN_PROC_EXCLUDE,
    };

    /// Levels a real run produces: a 0.30 tone, ambient 60 dB under it, the low
    /// leg at a quarter of the volume.
    fn input<'a>(
        backend: &'a str,
        baseline_level: f32,
        low_level: f32,
        mute_level: Option<f32>,
    ) -> VolumeIndependenceInput<'a> {
        VolumeIndependenceInput {
            backend_id: backend,
            ambient_level: 0.0003,
            baseline: VolumeLeg {
                scalar: 0.8,
                level: baseline_level,
            },
            low: VolumeLeg {
                scalar: 0.2,
                level: low_level,
            },
            muted: mute_level.map(|level| VolumeLeg { scalar: 0.8, level }),
            tolerance_db: 3.0,
            floor: 0.002,
        }
    }

    #[test]
    fn a_level_that_ignores_the_knob_reads_as_upstream_capture() {
        let r = judge_volume_independence(&input(BACKEND_MAC_CATAP, 0.30, 0.30, Some(0.30)));
        assert_eq!(r.coupling, VolumeCoupling::Independent);
        assert_eq!(r.survives_mute, Some(true));
        assert!(r.conclusive, "notes: {:?}", r.notes);
        assert!(
            !r.contradicts_table,
            "plan §7.1 already predicts true for mac-catap"
        );
    }

    /// 0.8 -> 0.2 is -12.04 dB; a post-mix capture drops by exactly that.
    #[test]
    fn a_level_that_tracks_the_knob_reads_as_post_mix_and_is_still_conclusive() {
        let r = judge_volume_independence(&input(
            BACKEND_WIN_DEVICE_LOOPBACK,
            0.30,
            0.075,
            Some(0.0000),
        ));
        assert_eq!(r.coupling, VolumeCoupling::Follows);
        assert_eq!(r.survives_mute, Some(false));
        assert!(
            r.conclusive,
            "'follows' is a verdict, not a failure — it is the expected answer here"
        );
        assert!(
            (r.predicted_low_db - -12.04).abs() < 0.05,
            "{}",
            r.predicted_low_db
        );
    }

    /// The failure mode this whole probe is built against: nothing is playing,
    /// so both legs read the same and 'the same' is what independence looks
    /// like. It must come back Inconclusive, never Independent.
    #[test]
    fn silence_is_not_independence() {
        let mut i = input(BACKEND_MAC_CATAP, 0.0, 0.0, Some(0.0));
        i.ambient_level = 0.0;
        let r = judge_volume_independence(&i);
        assert_eq!(
            r.coupling,
            VolumeCoupling::Inconclusive,
            "two silent legs are equal, and equal must not be read as upstream capture"
        );
        assert_eq!(r.survives_mute, None);
        assert!(!r.conclusive);
        assert!(
            r.notes.iter().any(|n| n.contains("no signal")),
            "{:?}",
            r.notes
        );
    }

    /// Same trap one step subtler: the room IS making noise, the tone is not
    /// playing, and the noise is steady enough to look flat across both legs.
    #[test]
    fn ambient_noise_alone_cannot_carry_a_verdict() {
        let mut i = input(BACKEND_MAC_CATAP, 0.02, 0.02, Some(0.02));
        i.ambient_level = 0.02; // the "signal" IS the ambient reading
        let r = judge_volume_independence(&i);
        assert_eq!(r.coupling, VolumeCoupling::Inconclusive);
        assert!(!r.conclusive);
        assert!(
            r.signal_floor > i.baseline.level,
            "floor {} must sit above an ambient-only baseline {}",
            r.signal_floor,
            i.baseline.level
        );
    }

    /// Two hypotheses 12 dB apart are separable; two 1 dB apart are not, and
    /// claiming either one would be claiming the tolerance is smaller than it is.
    #[test]
    fn legs_too_close_to_tell_the_hypotheses_apart_are_refused() {
        let mut i = input(BACKEND_MAC_CATAP, 0.30, 0.30, None);
        i.low.scalar = 0.75; // 0.8 -> 0.75 is only -0.56 dB
        let r = judge_volume_independence(&i);
        assert_eq!(r.coupling, VolumeCoupling::Inconclusive);
        assert!(
            r.notes.iter().any(|n| n.contains("not separable")),
            "{:?}",
            r.notes
        );
    }

    /// A drop that matches neither 0 dB nor -12 dB is a third thing, and the
    /// probe has to say so rather than round to the nearer story.
    #[test]
    fn a_drop_that_fits_neither_hypothesis_is_reported_not_rounded() {
        // -6 dB: 6 dB from 'independent', 6 dB from 'follows', tolerance 3 dB.
        let r = judge_volume_independence(&input(BACKEND_MAC_CATAP, 0.30, 0.15, None));
        assert_eq!(r.coupling, VolumeCoupling::Inconclusive);
        assert!(
            r.notes
                .iter()
                .any(|n| n.contains("neither hypothesis fits")),
            "{:?}",
            r.notes
        );
    }

    /// The one output that makes the probe worth running: a measurement that
    /// disagrees with the table has to be loud about it.
    #[test]
    fn a_measurement_that_contradicts_the_plan_table_says_so() {
        // plan §7.1 says win-device-loopback does NOT survive a mute; measure
        // that it does.
        let r =
            judge_volume_independence(&input(BACKEND_WIN_DEVICE_LOOPBACK, 0.30, 0.30, Some(0.30)));
        assert_eq!(r.survives_mute, Some(true));
        assert_eq!(r.table_says, Some(false));
        assert!(r.contradicts_table);
        assert!(
            r.notes.iter().any(|n| n.contains("CONTRADICTS")),
            "{:?}",
            r.notes
        );
    }

    /// The row plan §7.1 leaves blank is the row this probe was ordered for.
    /// An unknown table entry must not be reported as a contradiction.
    #[test]
    fn filling_in_an_unmeasured_row_is_not_a_contradiction() {
        let r = judge_volume_independence(&input(BACKEND_WIN_PROC_EXCLUDE, 0.30, 0.30, Some(0.30)));
        assert_eq!(
            r.table_says, None,
            "plan §7.1 leaves win-proc-exclude 待实测"
        );
        assert_eq!(r.survives_mute, Some(true));
        assert!(!r.contradicts_table);
        assert!(r.conclusive);
    }

    /// Mute that neither passed the signal through nor killed it: 20 dB down is
    /// outside the 3 dB tolerance but still above the floor. No answer exists.
    #[test]
    fn a_partially_attenuating_mute_yields_no_answer() {
        let r = judge_volume_independence(&input(BACKEND_MAC_CATAP, 0.30, 0.30, Some(0.03)));
        assert_eq!(r.coupling, VolumeCoupling::Independent);
        assert_eq!(r.survives_mute, None);
        assert!(
            !r.conclusive,
            "a mute leg that was run but did not decide is not a verdict"
        );
        assert!(
            r.notes.iter().any(|n| n.contains("mute leg undecided")),
            "{:?}",
            r.notes
        );
    }

    /// Skipping the mute leg is allowed and still conclusive for the volume
    /// question — the two must not be conflated.
    #[test]
    fn skipping_the_mute_leg_still_settles_the_volume_question() {
        let r = judge_volume_independence(&input(BACKEND_MAC_CATAP, 0.30, 0.30, None));
        assert_eq!(r.coupling, VolumeCoupling::Independent);
        assert_eq!(r.survives_mute, None);
        assert!(r.conclusive);
        assert_eq!(r.mute_level, None);
    }

    /// NaN reaches this function whenever a capture buffer was empty. It must
    /// not slip through the floor comparison as "greater than".
    #[test]
    fn a_nan_level_cannot_pass_the_floor() {
        let r = judge_volume_independence(&input(BACKEND_MAC_CATAP, f32::NAN, f32::NAN, None));
        assert_eq!(r.coupling, VolumeCoupling::Inconclusive);
        assert!(!r.conclusive);
    }
}
