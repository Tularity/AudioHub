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
    /// One `IOCTL_AUDIOHUB_NOTIFY`: push a peer's volume into the slot's KS
    /// store, the way the daemon does when the peer's real device moves.
    ///
    /// Exposed as a probe verb because the endpoint-side half of the volume
    /// map — what the Windows audio engine reports back once our KS node has
    /// been told a level — cannot be measured any other way. Driving it
    /// through the daemon would need a paired peer whose real device is
    /// actually moving, which makes "the map is wrong" and "the peer sent
    /// something else" the same observation.
    Notify {
        #[arg(long, default_value_t = 0)]
        slot: u8,
        /// 0 means "whatever generation the slot carries now".
        #[arg(long, default_value_t = 0)]
        generation: u32,
        /// target the virtual MICROPHONE instead of the speaker
        #[arg(long)]
        input: bool,
        #[arg(long)]
        muted: bool,
        /// 0.0 ..= 1.0 amplitude scalar, exactly as the IOCTL carries it
        #[arg(long)]
        scalar: f32,
    },
    /// One `IOCTL_AUDIOHUB_STREAMSTAT`: how deep the WAVERT stage is for one
    /// endpoint, reported apart from the AudioHub ring.
    ///
    /// The two stages are never summed here. `spec-windows-driver.md:574`
    /// forbids it, and the reason is operational rather than pedantic: a deep
    /// WaveRT stage means the audio engine is handing packets over late and
    /// trim cannot help, while a deep ring is exactly what trim is for.
    Stat {
        #[arg(long, default_value_t = 0)]
        slot: u8,
        /// the virtual MICROPHONE instead of the speaker
        #[arg(long)]
        input: bool,
    },
    /// Print what the dB<->scalar map SAYS should happen, without touching the
    /// driver. Works on every platform.
    ///
    /// This is the reference column of the 11.1 calibration table. It has to
    /// come from here rather than be reimplemented in the harness: a shell or
    /// python copy of the mapping would be a third transcription, and the
    /// whole point of the exercise is to compare the real endpoint against THE
    /// mapping, not against something that merely resembles it.
    Volmap {
        /// One scalar. Omit to sweep the whole 0..1 range at `--steps` points.
        #[arg(long)]
        scalar: Option<f32>,
        /// Number of sweep points, inclusive of both ends.
        #[arg(long, default_value_t = 21)]
        steps: u32,
    },
}

/// The reference row for one scalar: what the driver's map turns it into, and
/// what it turns back into. Platform-independent on purpose — the harness runs
/// this arm on the Windows box, but it is checkable here.
fn volmap_row(scalar: f32) -> serde_json::Value {
    use audiohubd::halbridge_win::volmap;
    use audiohubd::halbridge_win::wire;

    let q16 = wire::scalar_to_q16(scalar);
    let ks_db_q16 = volmap::scalar_q16_to_ks_db(q16);
    // The daemon-pushed path normalises to the advertised 0.5 dB grid before
    // storing, so the level a GET can return is the quantised one, not the raw
    // one. Reporting both is what makes a readback that lands on the grid
    // distinguishable from one that missed the map entirely.
    let quant_q16 = volmap::quantize_to_step(ks_db_q16);
    serde_json::json!({
        "scalar_in": scalar,
        "scalar_q16": q16,
        "ks_db_q16": ks_db_q16,
        "ks_db": ks_db_q16 as f64 / 65536.0,
        "ks_db_quantized_q16": quant_q16,
        "ks_db_quantized": quant_q16 as f64 / 65536.0,
        "back_to_scalar_q16": volmap::ks_db_to_scalar_q16(quant_q16),
        "back_to_scalar": volmap::ks_db_to_scalar_q16(quant_q16) as f64 / 65536.0,
    })
}

fn volmap_json(scalar: Option<f32>, steps: u32) -> serde_json::Value {
    let rows: Vec<serde_json::Value> = match scalar {
        Some(s) => vec![volmap_row(s)],
        None => {
            let n = steps.max(2);
            (0..n)
                .map(|i| volmap_row(i as f32 / (n - 1) as f32))
                .collect()
        }
    };
    serde_json::json!({
        "ok": true,
        "gate_error": serde_json::Value::Null,
        "result": { "op": "volmap", "rows": rows },
    })
}

#[cfg(not(windows))]
pub fn dispatch(cmd: WinvadCmd, json: bool) -> Result<i32> {
    // `volmap` is pure arithmetic and is the ONE arm that must answer here:
    // the mapping is developed on macOS and only executed on Windows, so a
    // reference column that cannot be printed on the development machine
    // cannot be reviewed before it is trusted.
    if let WinvadCmd::Volmap { scalar, steps } = &cmd {
        let out = volmap_json(*scalar, *steps);
        if json {
            println!("{out}");
        } else {
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        return Ok(0);
    }
    Err(anyhow::anyhow!(
        "probe winvad drives the Windows kernel driver and only exists on Windows"
    ))
}

#[cfg(windows)]
pub fn dispatch(cmd: WinvadCmd, json: bool) -> Result<i32> {
    use audiohubd::halbridge_win::session as ahsession;
    use audiohubd::halbridge_win::wire;

    // Answered before the control device is opened: the map is the driver's
    // arithmetic, not the driver's state, and a harness must be able to print
    // the reference column on a machine where the driver failed to load.
    if let WinvadCmd::Volmap { scalar, steps } = &cmd {
        let out = volmap_json(*scalar, *steps);
        if json {
            println!("{out}");
        } else {
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        return Ok(0);
    }

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
        // Broken out because a missing bit is a REPORTABLE degradation, not a
        // detail: without volume_event the peer -> this machine half of volume
        // sync does nothing, and without streamstat the WaveRT stage is
        // unmeasurable.
        "has_streamstat": session.has_streamstat(),
        "has_volume_event": session.has_volume_event(),
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
        WinvadCmd::Notify {
            slot,
            generation,
            input,
            muted,
            scalar,
        } => {
            // A notify carrying a generation the slot has moved past is DROPPED
            // by the driver, silently and on purpose. `--generation 0` asks the
            // driver for "current", which is what a probe wants; anything else
            // is the caller deliberately testing the stale-generation path.
            let gen = if generation == 0 {
                session
                    .query_slots()?
                    .slots
                    .get(slot as usize)
                    .map(|s| s.generation)
                    .unwrap_or(0)
            } else {
                generation
            };
            let applied = session.notify(slot, gen, input, muted, scalar)?;
            // The reference row travels with the measurement rather than being
            // looked up later: the harness compares one JSON object against one
            // endpoint reading taken immediately after, and a table assembled
            // from two separate invocations can silently pair the wrong rows.
            serde_json::json!({
                "op": "notify",
                "slot": slot,
                "generation_used": gen,
                "input": input,
                "muted": muted,
                "applied": applied,
                "expect": volmap_row(scalar),
            })
        }
        WinvadCmd::Stat { slot, input } => {
            let stat = session.stream_stat(slot, input)?;
            match stat {
                // NOT an error and NOT zeroes: a driver that cannot measure
                // this stage must say so, or the ring's depth silently becomes
                // "the latency".
                None => serde_json::json!({
                    "op": "stat",
                    "slot": slot,
                    "input": input,
                    "supported": false,
                    "wavert": serde_json::Value::Null,
                }),
                Some(st) => serde_json::json!({
                    "op": "stat",
                    "slot": slot,
                    "input": input,
                    "supported": true,
                    // Named `wavert` rather than `stage` so nothing downstream
                    // can pair it with the ring's numbers by accident.
                    "wavert": {
                        "present": st.present(),
                        "buffer_bytes": st.buffer_bytes,
                        "resident_bytes": st.resident_bytes,
                        "frame_bytes": st.frame_bytes,
                        "sample_rate": st.sample_rate,
                        "packet_bytes": st.packet_bytes,
                        "updates": st.updates,
                        "resident_frames": st.resident_frames(),
                        "resident_ms": st.resident_ms(),
                        "capacity_frames": st.capacity_frames(),
                    },
                }),
            }
        }
        // Answered before the session was opened; this arm exists so the match
        // stays exhaustive without a catch-all that would swallow a future verb.
        WinvadCmd::Volmap { .. } => unreachable!("volmap is handled before the session opens"),
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
