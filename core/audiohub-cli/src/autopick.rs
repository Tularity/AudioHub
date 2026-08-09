//! `probe sysaudio --explain-auto` (plan §8; 用户 2026-08-09 裁定).
//!
//! plan §8 originally promised public `capture.*` IPC verbs for querying and
//! switching the system-audio capture backend. The user ruled on 2026-08-09
//! that selection stays **automatic/adaptive** and does not become IPC surface,
//! with a **test-surface probe** in place of the verbs. This is that probe.
//!
//! It answers three questions without opening a capture:
//!
//!   1. which backend `--backend auto` picks here, and why;
//!   2. why each of the others lost — separating "not on this host *yet*" from
//!      "ruled out for good" (`declined`), because only one of those is worth
//!      re-asking after an OS upgrade or a consent grant;
//!   3. whether it would **fall back correctly** if the preferred one died.
//!
//! (3) is the part that can actually be wrong, and it cannot be observed by
//! watching a healthy host: the real inventory is fixed by the OS underneath.
//! A mac never exhibits the Windows `win-proc-exclude` → `win-device-loopback`
//! fallback at all. So the probe **injects** the failure — `--simulate-unavailable`
//! forces an id to report unavailable — and asserts the rule moved on. A
//! selection rule that ignored availability would keep the dead backend and the
//! probe goes RED; that is the falsification.
//!
//! **No capture is ever opened here**, deliberately: on macOS creating the tap
//! is what raises the TCC dialog, and on a machine serving live audio the probe
//! must not touch the stream it is describing. Everything below is decided from
//! `list_backends()` declarations alone.

use anyhow::Result;
use audiohub_core::sysaudio::{
    self, AutoOutcome, BackendInfo, BACKEND_AUTO,
};

use crate::{emit_json, info, EXIT_CHECK_FAILED};

#[derive(clap::Args, Debug, Clone)]
pub struct AutoArgs {
    /// Report which system-audio backend `auto` selects and why, plus the
    /// fallback order. Opens no capture. Its own mode: the capture flags on
    /// `probe sysaudio` are ignored when this is set.
    #[arg(long)]
    pub explain_auto: bool,

    /// Force this backend id to report unavailable before deciding (repeatable).
    /// The probe then asserts `auto` did NOT pick it — this is the lever that
    /// makes the fallback claim falsifiable rather than decorative.
    #[arg(long, value_name = "ID")]
    pub simulate_unavailable: Vec<String>,
}

/// A named check with a verdict, so the caller sees which assertion moved
/// rather than one aggregate boolean.
struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
}

pub fn run(args: &AutoArgs, json: bool) -> Result<i32> {
    let real = sysaudio::list_backends();

    // Unknown ids are refused rather than ignored. A typo'd id would otherwise
    // inject nothing, every assertion would pass, and the run would read as
    // "fallback verified" having verified nothing at all.
    let known: Vec<&str> = real.iter().map(|b| b.id.as_str()).collect();
    let unknown: Vec<&str> = args
        .simulate_unavailable
        .iter()
        .map(|s| s.as_str())
        .filter(|s| !known.contains(s))
        .collect();
    if !unknown.is_empty() {
        anyhow::bail!(
            "--simulate-unavailable names unknown backend(s): {}. Known ids: {}",
            unknown.join(", "),
            known.join(", ")
        );
    }

    let effective = sysaudio::with_unavailable(&real, &args.simulate_unavailable);
    let baseline = sysaudio::explain_auto(&real);
    let choice = sysaudio::explain_auto(&effective);
    let chain = sysaudio::auto_fallback_chain(&effective);

    info(&format!(
        "sysaudio auto -> {}",
        choice.picked.as_deref().unwrap_or("(none available)")
    ));
    for s in &choice.steps {
        let verdict = match s.outcome {
            AutoOutcome::Picked => "PICKED",
            AutoOutcome::SkippedDeclined => "skipped (ruled out for good)",
            AutoOutcome::SkippedUnavailable => "skipped (unavailable here)",
            AutoOutcome::NotConsidered => "not considered (lower priority)",
        };
        let forced = if args.simulate_unavailable.iter().any(|i| i == &s.id) {
            " [FORCED UNAVAILABLE]"
        } else {
            ""
        };
        info(&format!("  {:<22} {}{} — {}", s.id, verdict, forced, s.note));
    }
    info(&format!(
        "fallback order from here: {}",
        if chain.is_empty() { "(nothing available)".to_string() } else { chain.join(" -> ") }
    ));

    let mut checks = Vec::new();

    // 1. The pick must be one the host actually offers, and startable.
    //    `start_backend` refuses declined ids, so a declined pick would be a
    //    hard error where a working fallback sat one row down.
    let picked_info: Option<&BackendInfo> =
        choice.picked.as_ref().and_then(|p| effective.iter().find(|b| &b.id == p));
    match picked_info {
        Some(b) => {
            checks.push(Check {
                name: "picked-is-available",
                ok: b.available && !b.declined,
                detail: format!("picked '{}' available={} declined={}", b.id, b.available, b.declined),
            });
        }
        None => {
            // Legitimate on a host with no capture backend at all. Recorded as
            // a passing check with the reason, not silently omitted.
            checks.push(Check {
                name: "picked-is-available",
                ok: true,
                detail: "no backend available on this host; auto correctly reports none".to_string(),
            });
        }
    }

    // 2. THE falsification. Anything forced unavailable must not be the pick.
    for id in &args.simulate_unavailable {
        checks.push(Check {
            name: "forced-backend-not-picked",
            ok: choice.picked.as_ref() != Some(id),
            detail: format!(
                "forced '{id}' unavailable; auto picked {}",
                choice.picked.as_deref().unwrap_or("(none)")
            ),
        });
    }

    // 3. The fallback must be reported as such. When the injection knocked out
    //    what the untouched host would have used, the pick has to have MOVED —
    //    to another backend or to none. Silently continuing is the failure mode
    //    this probe exists to catch.
    if let Some(base) = baseline.picked.as_ref() {
        if args.simulate_unavailable.iter().any(|i| i == base) {
            let moved = choice.picked.as_ref() != baseline.picked.as_ref();
            checks.push(Check {
                name: "fallback-reported",
                ok: moved,
                detail: format!(
                    "baseline pick '{base}' was forced out; auto now reports {}",
                    choice.picked.as_deref().unwrap_or("(none available)")
                ),
            });
        }
    }

    // 4. Termination/ordering of the cascade. A chain that repeats an id means
    //    the pool stopped shrinking and `auto_fallback_chain` would spin.
    let mut dedup = chain.clone();
    dedup.sort();
    dedup.dedup();
    checks.push(Check {
        name: "fallback-chain-repeat-free",
        ok: dedup.len() == chain.len(),
        detail: format!("chain {:?}", chain),
    });

    // 5. Drift guard against the shipping path. The daemon calls
    //    `resolve_backend(auto)`; this probe reports `explain_auto`. If those
    //    ever split, the probe's report would be confidently wrong — so the
    //    UNINJECTED baseline is compared with what the daemon would really get.
    let resolved = sysaudio::resolve_backend(BACKEND_AUTO).map(|b| b.id).ok();
    checks.push(Check {
        name: "probe-agrees-with-daemon",
        ok: resolved == baseline.picked,
        detail: format!(
            "resolve_backend(auto)={:?} explain_auto={:?}",
            resolved, baseline.picked
        ),
    });

    let ok = checks.iter().all(|c| c.ok);
    for c in &checks {
        info(&format!(
            "{:<28} {}  {}",
            c.name,
            if c.ok { "PASS" } else { "FAIL" },
            c.detail
        ));
    }

    emit_json(
        json,
        &serde_json::json!({
            "mode": "explain-auto",
            "simulate_unavailable": args.simulate_unavailable,
            "baseline_picked": baseline.picked,
            "picked": choice.picked,
            "steps": choice.steps,
            "fallback_chain": chain,
            "checks": checks.iter().map(|c| serde_json::json!({
                "name": c.name, "ok": c.ok, "detail": c.detail,
            })).collect::<Vec<_>>(),
            "ok": ok,
        }),
    );

    Ok(if ok { 0 } else { EXIT_CHECK_FAILED })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(sim: &[&str]) -> AutoArgs {
        AutoArgs {
            explain_auto: true,
            simulate_unavailable: sim.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// The uninjected run describes the host and must agree with itself.
    #[test]
    fn a_plain_explain_auto_run_passes_on_any_host() {
        assert_eq!(run(&args(&[]), false).expect("explain-auto must not error"), 0);
    }

    /// Knocking out whatever this host actually picked must still pass — the
    /// rule is expected to fall back or report none, and either is correct.
    /// What it must NOT do is keep the dead pick, which check 2 asserts.
    #[test]
    fn forcing_out_the_live_pick_still_passes_because_the_rule_moves() {
        let real = sysaudio::list_backends();
        let Some(pick) = sysaudio::explain_auto(&real).picked else {
            return; // no backend here; nothing to knock out
        };
        assert_eq!(run(&args(&[&pick]), false).expect("must not error"), 0);
    }

    /// A typo must not read as a clean fallback verification. Without this the
    /// probe would "verify" the fallback while injecting nothing at all.
    #[test]
    fn an_unknown_injected_id_is_an_error_not_a_silent_pass() {
        let err = run(&args(&["no-such-backend"]), false)
            .expect_err("unknown backend ids must be refused");
        assert!(
            err.to_string().contains("unknown backend"),
            "error must name the problem, got: {err}"
        );
    }

    /// Every id `list_backends` offers is injectable. A backend the probe
    /// cannot knock out is a backend whose fallback nobody can test.
    #[test]
    fn every_known_backend_can_be_injected() {
        for b in sysaudio::list_backends() {
            assert_eq!(
                run(&args(&[&b.id]), false).unwrap_or_else(|e| panic!("injecting '{}': {e}", b.id)),
                0,
                "forcing '{}' unavailable must leave the rule self-consistent",
                b.id
            );
        }
    }
}
