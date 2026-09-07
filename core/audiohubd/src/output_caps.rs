//! Connection-scoped native output observations. These do not enable a media mode.

use crate::{lk, DaemonInner};
use audiohub_core::output_capabilities::{
    default_output_capabilities, NativeOutputCapabilities, NativeOutputObservation,
};
use audiohub_net::secure::SessionMsg;
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

fn replace_local(
    current: &mut NativeOutputObservation,
    capabilities: NativeOutputCapabilities,
) -> bool {
    if current.capabilities == capabilities {
        return false;
    }
    let Some(revision) = current.revision.checked_add(1) else {
        return false;
    };
    *current = NativeOutputObservation {
        revision,
        capabilities,
    };
    true
}

pub(crate) fn accept_remote(
    current: &mut Option<NativeOutputObservation>,
    mut incoming: NativeOutputObservation,
) -> bool {
    if current
        .as_ref()
        .is_some_and(|value| incoming.revision <= value.revision)
    {
        return false;
    }
    if !incoming.capabilities.is_valid() {
        incoming.capabilities = NativeOutputCapabilities::Error {
            message: "peer sent invalid native output capabilities".into(),
        };
    }
    *current = Some(incoming);
    true
}

fn publish(inner: &DaemonInner, capabilities: NativeOutputCapabilities) {
    let observation = {
        let mut current = lk(&inner.native_output);
        if !replace_local(&mut current, capabilities) {
            return;
        }
        current.clone()
    };
    let peers: Vec<_> = lk(&inner.state)
        .conns
        .values()
        .filter(|conn| conn.alive.load(Ordering::SeqCst))
        .cloned()
        .collect();
    for peer in peers {
        let _ = peer.send_msg(&SessionMsg::NativeOutputCapabilities {
            observation: observation.clone(),
        });
        let _ = peer.send_msg(&SessionMsg::SpatialOutputCapabilities {
            offer: crate::spatial::offer_for(&observation),
        });
    }
}

pub(crate) fn watch_loop(inner: Arc<DaemonInner>) {
    let (request_tx, request_rx) = mpsc::sync_channel::<()>(1);
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    // Keep at most one native query outstanding. A wedged OS service must not
    // stall IPC/shutdown or spawn an unbounded succession of replacement threads.
    let worker =
        std::thread::Builder::new()
            .name("ahb-output-query".into())
            .spawn(move || {
                while request_rx.recv().is_ok() {
                    let facts = std::panic::catch_unwind(default_output_capabilities)
                        .unwrap_or_else(|_| NativeOutputCapabilities::Error {
                            message: "native output capability query panicked".into(),
                        });
                    if result_tx.send(facts).is_err() {
                        break;
                    }
                }
            });
    if worker.is_err() {
        publish(
            &inner,
            NativeOutputCapabilities::Error {
                message: "could not start native output query worker".into(),
            },
        );
        return;
    }
    // The worker owns no daemon state. Dropping the request channel ends it;
    // intentionally do not join an OS property call that may never return.
    drop(worker);
    let mut pending: Option<(Instant, u64, bool)> = None;
    let mut next_query = Instant::now();
    let mut observed_epoch = inner.dev_out_epoch.load(Ordering::Acquire);
    while !inner.shutdown.load(Ordering::SeqCst) {
        let epoch = inner.dev_out_epoch.load(Ordering::Acquire);
        if epoch != observed_epoch {
            // A previous endpoint's facts cannot describe the newly selected
            // output while its native query is pending or blocked.
            publish(&inner, NativeOutputCapabilities::Unknown);
            observed_epoch = epoch;
            next_query = Instant::now();
        }
        match result_rx.try_recv() {
            Ok(facts) => {
                if let Some((_, requested_epoch, timed_out)) = pending.take() {
                    if !timed_out && requested_epoch == epoch {
                        publish(&inner, facts);
                        observed_epoch = epoch;
                        next_query = Instant::now() + Duration::from_secs(2);
                    } else {
                        next_query = Instant::now();
                    }
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                publish(
                    &inner,
                    NativeOutputCapabilities::Error {
                        message: "native output query worker stopped".into(),
                    },
                );
                return;
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if let Some((started, _, timed_out)) = pending.as_mut() {
            if !*timed_out && started.elapsed() >= Duration::from_secs(5) {
                *timed_out = true;
                publish(
                    &inner,
                    NativeOutputCapabilities::Error {
                        message: "native output capability query timed out".into(),
                    },
                );
            }
        } else if Instant::now() >= next_query {
            if request_tx.try_send(()).is_err() {
                publish(
                    &inner,
                    NativeOutputCapabilities::Error {
                        message: "native output query could not be queued".into(),
                    },
                );
                return;
            }
            pending = Some((Instant::now(), epoch, false));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_observation_revision_changes_only_with_facts() {
        let mut current = NativeOutputObservation::default();
        assert!(!replace_local(
            &mut current,
            NativeOutputCapabilities::Unknown
        ));
        assert!(replace_local(
            &mut current,
            NativeOutputCapabilities::Unavailable
        ));
        assert_eq!(current.revision, 1);
        assert!(!replace_local(
            &mut current,
            NativeOutputCapabilities::Unavailable
        ));
        assert!(replace_local(
            &mut current,
            NativeOutputCapabilities::Unknown
        ));
        assert_eq!(current.revision, 2);
    }

    #[test]
    fn remote_output_observations_reject_reordered_updates() {
        let mut current = None;
        assert!(accept_remote(
            &mut current,
            NativeOutputObservation {
                revision: 4,
                capabilities: NativeOutputCapabilities::Unavailable,
            }
        ));
        for revision in [0, 3, 4] {
            assert!(!accept_remote(
                &mut current,
                NativeOutputObservation {
                    revision,
                    capabilities: NativeOutputCapabilities::Unknown,
                }
            ));
        }
        assert_eq!(
            current.as_ref().unwrap().capabilities,
            NativeOutputCapabilities::Unavailable
        );
        assert!(accept_remote(
            &mut current,
            NativeOutputObservation {
                revision: 5,
                capabilities: NativeOutputCapabilities::Error {
                    message: String::new()
                },
            }
        ));
        assert!(current.unwrap().capabilities.is_valid());
        // A replacement connection owns a fresh cell and accepts its revision zero.
        assert!(accept_remote(&mut None, NativeOutputObservation::default()));
    }
}
