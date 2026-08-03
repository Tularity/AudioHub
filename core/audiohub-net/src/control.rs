use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::TcpStream;

pub const CONTROL_MAX_FRAME: usize = 65536;

/// Peer-to-peer control protocol version, compared for **strict equality** on
/// every verify (see `pairing::check_protocol`). A mismatch refuses the
/// connection; it never edits the peer store.
///
/// ## Why equality, and why this exists at all now
///
/// Until plan §13 there was no peer version negotiation whatsoever — the
/// `Hello` variant below carries a `version` field that nothing has ever sent
/// or read. Adding message *variants* did not need one: `secure.rs` skips a
/// `SessionMsg` it cannot decode and keeps the connection, so new telemetry
/// degrades to "the peer never tells us" (that guarantee is documented on
/// `SessionMsg::Unpaired` and still holds).
///
/// **Mode advertisement is not that shape.** A build that predates §13 is
/// simultaneously a provider and a consumer, and it has no field in which to
/// say so. It would therefore list a machine sitting in mode A/B as usable,
/// open streams that get refused, and — the part that is not merely cosmetic —
/// it can still be the relay leg of the §13 cycle, because *its* half of the
/// exclusion does not exist. Skipping an unparseable `ModeState` would leave
/// that gap open and silent. Refusing the connection makes it loud, and the
/// fix (upgrade both ends) is the same either way.
///
/// Version 1 = the unversioned pre-§13 protocol. Peers of that vintage send no
/// `version` field at all, which `serde(default)` reads as [`VERSION_ABSENT`]
/// so the refusal can name the problem instead of failing as a parse error.
pub const PROTOCOL_VERSION: u32 = 2;

/// What a missing `version` field decodes to: a peer old enough to have no
/// version at all. Distinct from any real version so the refusal message can
/// say "upgrade it" rather than quoting a number the peer never sent.
pub const VERSION_ABSENT: u32 = 0;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMsg {
    Hello {
        version: u32,
        name: String,
        fingerprint: String,
    },
    PairInit {
        spake_msg_b64: String,
        name: String,
        listen_port: u16,
    },
    PairResp {
        spake_msg_b64: String,
        name: String,
    },
    PairConfirmA {
        mac_b64: String,
        public_key_b64: String,
    },
    PairConfirmB {
        mac_b64: String,
        public_key_b64: String,
    },
    /// The initiator's opening frame, and the first place it can state its
    /// protocol version. `serde(default)` so a pre-§13 peer decodes as
    /// [`VERSION_ABSENT`] and gets a refusal that names the problem, instead of
    /// a bare "parse control message" that names nothing.
    VerifyHello {
        fingerprint: String,
        nonce_b64: String,
        #[serde(default)]
        version: u32,
    },
    /// The responder's first frame, and the mirror of the version above: both
    /// ends have to learn the other's, and this is the earliest frame in which
    /// the responder speaks.
    VerifyChallenge {
        nonce_b64: String,
        #[serde(default)]
        version: u32,
    },
    VerifyResponse {
        sig_b64: String,
        public_key_b64: String,
        name: String,
    },
    SecInit {
        eph_pub_b64: String,
        nonce_b64: String,
        sig_b64: String,
    },
    SecResp {
        eph_pub_b64: String,
        nonce_b64: String,
        sig_b64: String,
    },
    Enc {
        n: u64,
        data_b64: String,
    },
    Ok {},
    /// "I am not paired with you any more." Sent instead of the generic
    /// `Error{unknown peer}` when a verify arrives from a fingerprint we do not
    /// know (spec-m5b OPEN QUESTION 5, plan §7.1 — the owner ruled this in).
    ///
    /// AUTHENTICATED, because acting on it DELETES A TRUST RELATIONSHIP. This
    /// frame answers `VerifyHello`, i.e. it arrives before either side has
    /// signed anything, so an empty version of it is a bare assertion by
    /// whoever happens to answer on the peer's address — a stale service that
    /// inherited the port, an attacker who can occupy or intercept it, or (as
    /// measured on 2026-07-31, when a peer record pointed at this daemon's own
    /// control port) the daemon itself. Any of them could make us silently
    /// destroy a pairing.
    ///
    /// The responder still knows its OWN key when it no longer knows ours, so
    /// it can prove who it is: `sig_b64` signs the initiator's nonce under a
    /// domain separator of its own (`pairing::UNPAIRED_LABEL`), and the
    /// initiator acts only when the key's fingerprint is the peer it meant to
    /// reach AND the signature holds against the key it has on file.
    ///
    /// Wire-compatible in the safe direction, both ways. `serde(default)` makes
    /// an old responder's empty `{"type":"unpaired"}` parse here with empty
    /// strings — unauthenticated, so the initiator keeps the pairing. In the
    /// other direction serde ignores unknown fields on a struct variant, so an
    /// old initiator reads a new responder's frame as the empty one it expects.
    Unpaired {
        #[serde(default)]
        sig_b64: String,
        #[serde(default)]
        public_key_b64: String,
    },
    Error {
        message: String,
    },
}

pub fn write_frame(s: &mut TcpStream, msg: &ControlMsg) -> std::io::Result<()> {
    let body = serde_json::to_vec(msg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if body.len() > CONTROL_MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "control frame too large",
        ));
    }
    s.write_all(&(body.len() as u32).to_le_bytes())?;
    s.write_all(&body)?;
    s.flush()
}

/// Returns the decoded message, including `ControlMsg::Error` as-is (caller interprets).
pub fn read_frame(s: &mut TcpStream) -> Result<ControlMsg> {
    let mut len_bytes = [0u8; 4];
    s.read_exact(&mut len_bytes).context("read frame length")?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    if len > CONTROL_MAX_FRAME {
        bail!("control frame too large: {len} bytes");
    }
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).context("read frame body")?;
    serde_json::from_slice(&body).context("parse control message")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape `Unpaired` had before the refusal was signed. A peer running
    /// that build must survive a new responder's frame, so this asserts the
    /// property the wire compatibility rests on: serde ignores fields a struct
    /// variant does not declare (no `deny_unknown_fields` anywhere here).
    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum LegacyControlMsg {
        Unpaired {},
        Ok {},
    }

    #[test]
    fn a_signed_refusal_still_parses_where_the_frame_carried_no_fields() {
        let json = serde_json::to_vec(&ControlMsg::Unpaired {
            sig_b64: "c2ln".into(),
            public_key_b64: "a2V5".into(),
        })
        .expect("serialize");
        let old: LegacyControlMsg = serde_json::from_slice(&json)
            .expect("an old peer must still parse the frame, not crash on it");
        assert!(matches!(old, LegacyControlMsg::Unpaired {}));
    }

    /// The other direction: an old responder sends the bare frame, and the new
    /// initiator has to READ it (to log "unauthenticated, keeping the pairing")
    /// rather than fail at the parse. That is what `serde(default)` buys.
    #[test]
    fn an_old_refusal_parses_here_as_an_unsigned_one() {
        let msg: ControlMsg =
            serde_json::from_slice(br#"{"type":"unpaired"}"#).expect("parse legacy frame");
        match msg {
            ControlMsg::Unpaired { sig_b64, public_key_b64 } => {
                assert!(sig_b64.is_empty(), "no signature in the legacy frame");
                assert!(public_key_b64.is_empty(), "no key in the legacy frame");
            }
            other => panic!("expected Unpaired, got {other:?}"),
        }
    }
}
