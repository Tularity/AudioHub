use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::TcpStream;

pub const CONTROL_MAX_FRAME: usize = 65536;

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
    VerifyHello {
        fingerprint: String,
        nonce_b64: String,
    },
    VerifyChallenge {
        nonce_b64: String,
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
