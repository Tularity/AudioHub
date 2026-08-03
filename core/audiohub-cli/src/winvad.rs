//! `audiohub probe winvad` — a direct, daemon-free client of the Windows
//! virtual-audio driver's control device.
//!
//! # Why this exists
//!
//! Every earlier way of exercising `IOCTL_AUDIOHUB_BIND_SET` went through the
//! daemon, which means through pairing with a real peer over the network. That
//! makes a driver-level defect (an endpoint that fails to come back on the
//! second bind) indistinguishable from a daemon-level one, and it drags a
//! second machine — one that is serving a user's audio — into every
//! experiment. This subcommand talks to `\\.\AudioHubVadCtl` and nothing else,
//! so a pair/unpair cycle is two IOCTLs with observable replies.
//!
//! It deliberately does NOT loop internally. Each invocation is one step, so
//! the caller can interleave `pnputil /enum-interfaces` and
//! `Get-PnpDevice -Class AudioEndpoint` between steps and get a per-step
//! record of what the system did. A self-contained loop would only be able to
//! report what the DRIVER claims — which is exactly the thing under suspicion.
//!
//! # The fault-injection switches
//!
//! `--fail-render`, `--fail-capture`, `--skip-rollback` and `--legacy-unbind`
//! set `AH_BINDFLAG_*` debug bits that the daemon never sets. They exist
//! because "the driver reports a half-failed install honestly" cannot be
//! tested without a way to make one half fail, and `--legacy-unbind`
//! reproduces the M6-2 speaker-loss defect on demand against a FIXED driver —
//! so one binary can show the failure appearing and going away again.

use anyhow::Result;

#[derive(clap::Subcommand)]
pub enum WinvadCmd {
    /// Handshake and dump the driver's own account of its slots.
    Status,
    /// One `BIND_SET`.
    Set {
        #[arg(long, default_value_t = 0)]
        slot: u8,
        /// exactly 16 lowercase hex digits (the peer fingerprint)
        #[arg(long)]
        peer_key: String,
        /// The BARE peer name, exactly as `haldev::display_names` would emit
        /// it. The "AudioHub - " prefix is added by the wire encoder, so
        /// passing a prefixed name here doubles it.
        #[arg(long, default_value = "Probe Peer")]
        display: String,
        #[arg(long)]
        online: bool,
        /// fault injection: make the speaker half fail
        #[arg(long)]
        fail_render: bool,
        /// fault injection: make the microphone half fail (after the speaker
        /// half succeeded — this is the rollback test)
        #[arg(long)]
        fail_capture: bool,
        /// fault injection: leave the partial install in place so `published`
        /// can be observed instead of rolled back
        #[arg(long)]
        skip_rollback: bool,
        /// fault injection: skip the per-peer pin-name write, so the fallback
        /// to the INF's generic direction names can be observed
        #[arg(long)]
        fail_endpoint_name: bool,
    },
    /// One `BIND_CLEAR`. `--generation 0` means "whatever is there now".
    Clear {
        #[arg(long, default_value_t = 0)]
        slot: u8,
        #[arg(long, default_value_t = 0)]
        generation: u32,
        /// fault injection: unregister the physical connection through the
        /// TOPOLOGY port even when the WAVE port owns it — the M6-2 defect
        #[arg(long)]
        legacy_unbind: bool,
    },
}

#[cfg(not(windows))]
pub fn dispatch(_cmd: WinvadCmd, _json: bool) -> Result<i32> {
    Err(anyhow::anyhow!(
        "probe winvad drives the Windows kernel driver and only exists on Windows"
    ))
}

#[cfg(windows)]
pub fn dispatch(cmd: WinvadCmd, json: bool) -> Result<i32> {
    use audiohubd::halbridge_win::session as ahsession;
    use audiohubd::halbridge_win::wire;

    let session = match ahsession::Session::open() {
        Ok(s) => s,
        Err(e) => {
            let v = serde_json::json!({
                "ok": false,
                "stage": "open",
                "error": e.text(),
                "driver_present": e.driver_present(),
            });
            if json {
                println!("{v}");
            } else {
                eprintln!("[winvad] open failed: {}", e.text());
            }
            return Ok(super::EXIT_CHECK_FAILED);
        }
    };

    let head = serde_json::json!({
        "session_id": session.session_id,
        "slot_count": session.slot_count,
        "protocol": session.driver_protocol,
        "client_check": session.client_check,
        "caps": session.caps,
    });

    // The probe's exit code is what a shell harness reads, so it must reflect
    // the SAME gate the daemon applies — not merely "the IOCTL came back".
    let mut gate: Option<std::result::Result<(), String>> = None;

    let body = match cmd {
        WinvadCmd::Status => serde_json::json!({ "op": "status" }),
        WinvadCmd::Set {
            slot,
            ref peer_key,
            ref display,
            online,
            fail_render,
            fail_capture,
            skip_rollback,
            fail_endpoint_name,
        } => {
            let mut flags = 0u32;
            if fail_render {
                flags |= wire::BINDFLAG_FAIL_RENDER;
            }
            if fail_capture {
                flags |= wire::BINDFLAG_FAIL_CAPTURE;
            }
            if skip_rollback {
                flags |= wire::BINDFLAG_SKIP_ROLLBACK;
            }
            if fail_endpoint_name {
                flags |= wire::BINDFLAG_FAIL_ENDPOINT_NAME;
            }
            let r = session.bind_set_with(slot, peer_key, display, online, flags)?;
            gate = Some(wire::bind_outcome(true, &r));
            serde_json::json!({
                "op": "set",
                "slot": slot,
                "peer_key": peer_key,
                "display": display,
                "debug_flags": flags,
                "reply": bind_reply_json(&r),
            })
        }
        WinvadCmd::Clear {
            slot,
            generation,
            legacy_unbind,
        } => {
            let flags = if legacy_unbind {
                wire::BINDFLAG_LEGACY_UNBIND
            } else {
                0
            };
            let r = session.bind_clear_with(slot, generation, flags)?;
            gate = Some(wire::bind_outcome(false, &r));
            serde_json::json!({
                "op": "clear",
                "slot": slot,
                "generation_sent": generation,
                "debug_flags": flags,
                "reply": bind_reply_json(&r),
            })
        }
    };

    let slots = session.query_slots()?;
    let mut arr = Vec::new();
    for (i, s) in slots.slots.iter().enumerate().take(slots.slot_count as usize) {
        if s.state == wire::SLOT_FREE && s.peer_key.is_empty() && s.published == 0 {
            continue;
        }
        arr.push(serde_json::json!({
            "slot": i,
            "state": s.state,
            "generation": s.generation,
            "peer_key": s.peer_key,
            "published": s.published,
            "published_label": wire::published_label(s.published),
            // "The slot's published mask matches the state it claims": BOUND
            // must mean both halves, FREE must mean neither. Anything else is
            // the defect being guarded against, and it gets its own flag so a
            // shell harness does not have to reimplement the comparison.
            //
            // Deliberately NOT `state != BOUND || published == BOTH`: that
            // reads `true` for a FREE slot the driver is still holding filters
            // for, which is exactly one of the states worth catching.
            "whole": match s.state {
                wire::SLOT_BOUND => s.published == wire::PUB_BOTH,
                _ => s.published == 0,
            },
        }));
    }

    let (ok, gate_error) = match &gate {
        None => (true, None),
        Some(Ok(())) => (true, None),
        Some(Err(e)) => (false, Some(e.clone())),
    };

    let out = serde_json::json!({
        "ok": ok,
        "gate_error": gate_error,
        "session": head,
        "result": body,
        "slots": arr,
    });
    if json {
        println!("{out}");
    } else {
        println!("{}", serde_json::to_string_pretty(&out)?);
    }
    Ok(if ok { 0 } else { super::EXIT_CHECK_FAILED })
}

#[cfg(windows)]
fn bind_reply_json(r: &audiohubd::halbridge_win::wire::BindReply) -> serde_json::Value {
    use audiohubd::halbridge_win::wire;
    serde_json::json!({
        "status": r.status,
        "status_label": wire::status_label(r.status),
        "state": r.state,
        "generation": r.generation,
        "stage": r.stage,
        "stage_label": wire::stage_label(r.stage),
        "nt_status": format!("0x{:08x}", r.nt_status),
        "published": r.published,
        "published_label": wire::published_label(r.published),
        "flags": r.flags,
        // The one degradation that leaves `status` OK. Printed unconditionally
        // so a harness reads it as a field rather than having to notice its
        // absence.
        "endpoint_name_fallback": r.endpoint_name_fell_back(),
    })
}
