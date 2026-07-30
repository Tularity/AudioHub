use crate::control::{read_frame, write_frame, ControlMsg};
use crate::identity::{fingerprint_of, verify_sig, LocalIdentity, PairedPeer, PeerStore};
use anyhow::{anyhow, bail, Result};
use base64::prelude::*;
use hmac::{Hmac, Mac};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use std::net::TcpStream;
use std::time::{SystemTime, UNIX_EPOCH};

const IDENT_INITIATOR: &[u8] = b"audiohub-initiator";
const IDENT_RESPONDER: &[u8] = b"audiohub-responder";
const CONFIRM_LABEL_A: &[u8] = b"audiohub-confirm-A";
const CONFIRM_LABEL_B: &[u8] = b"audiohub-confirm-B";
const VERIFY_LABEL: &[u8] = b"audiohub-verify";

type HmacSha256 = Hmac<Sha256>;

pub struct PairOutcome {
    pub peer: PairedPeer,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn b64d(s: &str) -> Result<Vec<u8>> {
    BASE64_STANDARD
        .decode(s)
        .map_err(|e| anyhow!("bad base64: {e}"))
}

// per-sender transcript: MAC covers only the sender's own public key
fn confirm_hmac(k: &[u8], label: &[u8], spake_a: &[u8], spake_b: &[u8], pub_sender: &[u8]) -> HmacSha256 {
    let mut h = Sha256::new();
    h.update(spake_a);
    h.update(spake_b);
    h.update(pub_sender);
    let transcript = h.finalize();
    let mut mac = HmacSha256::new_from_slice(k).expect("hmac accepts any key length");
    mac.update(label);
    mac.update(&transcript);
    mac
}

fn confirm_mac(k: &[u8], label: &[u8], spake_a: &[u8], spake_b: &[u8], pub_sender: &[u8]) -> Vec<u8> {
    confirm_hmac(k, label, spake_a, spake_b, pub_sender)
        .finalize()
        .into_bytes()
        .to_vec()
}

fn confirm_mac_ok(
    k: &[u8],
    label: &[u8],
    spake_a: &[u8],
    spake_b: &[u8],
    pub_sender: &[u8],
    mac: &[u8],
) -> bool {
    confirm_hmac(k, label, spake_a, spake_b, pub_sender)
        .verify_slice(mac)
        .is_ok()
}

fn pub_arr(pub_b64: &str) -> Result<[u8; 32]> {
    let bytes = b64d(pub_b64)?;
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("peer public key must be 32 bytes"))
}

/// Runs the full initiator side including waiting for the final `Ok`.
/// Caller persists (store.upsert + save) after this returns Ok, and fills peer.last_addr.
pub fn pair_initiator(
    s: &mut TcpStream,
    pin: &str,
    id: &LocalIdentity,
    my_listen_port: u16,
) -> Result<PairOutcome> {
    let _ = s.set_nodelay(true);
    let target_port = s.peer_addr().map(|a| a.port()).unwrap_or(0);

    let (state, msg_a) = Spake2::<Ed25519Group>::start_a(
        &Password::new(pin.as_bytes()),
        &Identity::new(IDENT_INITIATOR),
        &Identity::new(IDENT_RESPONDER),
    );
    write_frame(
        s,
        &ControlMsg::PairInit {
            spake_msg_b64: BASE64_STANDARD.encode(&msg_a),
            name: id.name.clone(),
            listen_port: my_listen_port,
        },
    )?;
    let (msg_b, peer_name) = match read_frame(s)? {
        ControlMsg::PairResp { spake_msg_b64, name } => (b64d(&spake_msg_b64)?, name),
        ControlMsg::Error { message } => bail!("{message}"),
        other => bail!("unexpected message: {other:?}"),
    };
    let k = state
        .finish(&msg_b)
        .map_err(|e| anyhow!("spake2 finish failed: {e:?}"))?;

    let mac_a = confirm_mac(&k, CONFIRM_LABEL_A, &msg_a, &msg_b, &id.public_key_bytes());
    write_frame(
        s,
        &ControlMsg::PairConfirmA {
            mac_b64: BASE64_STANDARD.encode(&mac_a),
            public_key_b64: id.public_key_b64(),
        },
    )?;
    let (mac_b, pub_b_b64) = match read_frame(s)? {
        ControlMsg::PairConfirmB { mac_b64, public_key_b64 } => (b64d(&mac_b64)?, public_key_b64),
        ControlMsg::Error { message } => bail!("{message}"),
        other => bail!("unexpected message: {other:?}"),
    };
    let pub_b = pub_arr(&pub_b_b64)?;
    if !confirm_mac_ok(&k, CONFIRM_LABEL_B, &msg_a, &msg_b, &pub_b, &mac_b) {
        let _ = write_frame(s, &ControlMsg::Error { message: "pin mismatch".into() });
        bail!("pin mismatch");
    }
    match read_frame(s)? {
        ControlMsg::Ok {} => {}
        ControlMsg::Error { message } => bail!("{message}"),
        other => bail!("unexpected message: {other:?}"),
    }
    Ok(PairOutcome {
        peer: PairedPeer {
            name: peer_name,
            fingerprint: fingerprint_of(&pub_b),
            public_key_b64: pub_b_b64,
            last_addr: None,
            port: target_port,
            added_unix: now_unix(),
        },
    })
}

/// Does NOT send the final `Ok` frame: caller must persist the peer first,
/// then send `ControlMsg::Ok {}` via `control::write_frame` (spec: responder
/// persists before Ok, initiator persists after receiving Ok).
pub fn pair_responder(s: &mut TcpStream, pin: &str, id: &LocalIdentity) -> Result<PairOutcome> {
    let _ = s.set_nodelay(true);

    let (msg_a, peer_name, listen_port) = match read_frame(s)? {
        ControlMsg::PairInit { spake_msg_b64, name, listen_port } => {
            (b64d(&spake_msg_b64)?, name, listen_port)
        }
        ControlMsg::Error { message } => bail!("{message}"),
        other => bail!("unexpected message: {other:?}"),
    };
    let (state, msg_b) = Spake2::<Ed25519Group>::start_b(
        &Password::new(pin.as_bytes()),
        &Identity::new(IDENT_INITIATOR),
        &Identity::new(IDENT_RESPONDER),
    );
    write_frame(
        s,
        &ControlMsg::PairResp {
            spake_msg_b64: BASE64_STANDARD.encode(&msg_b),
            name: id.name.clone(),
        },
    )?;
    let k = state
        .finish(&msg_a)
        .map_err(|e| anyhow!("spake2 finish failed: {e:?}"))?;

    let (mac_a, pub_a_b64) = match read_frame(s)? {
        ControlMsg::PairConfirmA { mac_b64, public_key_b64 } => (b64d(&mac_b64)?, public_key_b64),
        ControlMsg::Error { message } => bail!("{message}"),
        other => bail!("unexpected message: {other:?}"),
    };
    let pub_a = pub_arr(&pub_a_b64)?;
    if !confirm_mac_ok(&k, CONFIRM_LABEL_A, &msg_a, &msg_b, &pub_a, &mac_a) {
        let _ = write_frame(s, &ControlMsg::Error { message: "pin mismatch".into() });
        bail!("pin mismatch");
    }
    let mac_b = confirm_mac(&k, CONFIRM_LABEL_B, &msg_a, &msg_b, &id.public_key_bytes());
    write_frame(
        s,
        &ControlMsg::PairConfirmB {
            mac_b64: BASE64_STANDARD.encode(&mac_b),
            public_key_b64: id.public_key_b64(),
        },
    )?;
    Ok(PairOutcome {
        peer: PairedPeer {
            name: peer_name,
            fingerprint: fingerprint_of(&pub_a),
            public_key_b64: pub_a_b64,
            last_addr: None,
            port: listen_port,
            added_unix: now_unix(),
        },
    })
}

fn verify_preimage(nonce: &[u8], fp_first: &str, fp_second: &str) -> Vec<u8> {
    let mut m = Vec::with_capacity(VERIFY_LABEL.len() + nonce.len() + 32);
    m.extend_from_slice(VERIFY_LABEL);
    m.extend_from_slice(nonce);
    m.extend_from_slice(fp_first.as_bytes());
    m.extend_from_slice(fp_second.as_bytes());
    m
}

// Wire order note: the responder sends its VerifyResponse right after the
// challenge (before the initiator's), because the initiator cannot know the
// responder's fingerprint — needed in its own signature preimage — earlier.
// Signature preimages are exactly per spec.
pub fn verify_initiator(
    s: &mut TcpStream,
    id: &LocalIdentity,
    store: &PeerStore,
) -> Result<PairedPeer> {
    let _ = s.set_nodelay(true);

    let mut nonce_i = [0u8; 16];
    OsRng.fill_bytes(&mut nonce_i);
    write_frame(
        s,
        &ControlMsg::VerifyHello {
            fingerprint: id.fingerprint.clone(),
            nonce_b64: BASE64_STANDARD.encode(nonce_i),
        },
    )?;
    let nonce_r = match read_frame(s)? {
        ControlMsg::VerifyChallenge { nonce_b64 } => b64d(&nonce_b64)?,
        ControlMsg::Error { message } => bail!("{message}"),
        other => bail!("unexpected message: {other:?}"),
    };
    let (sig_r, pub_r_b64) = match read_frame(s)? {
        ControlMsg::VerifyResponse { sig_b64, public_key_b64, name: _ } => {
            (b64d(&sig_b64)?, public_key_b64)
        }
        ControlMsg::Error { message } => bail!("{message}"),
        other => bail!("unexpected message: {other:?}"),
    };
    let fp_r = fingerprint_of(&pub_arr(&pub_r_b64)?);
    let peer = store
        .find(&fp_r)
        .ok_or_else(|| anyhow!("unknown peer"))?
        .clone();
    // verify with the stored public key, not the one from the wire
    let m_r = verify_preimage(&nonce_i, &fp_r, &id.fingerprint);
    if !verify_sig(&peer.public_key_b64, &m_r, &sig_r) {
        let _ = write_frame(
            s,
            &ControlMsg::Error { message: "signature verification failed".into() },
        );
        bail!("signature verification failed");
    }
    let m_i = verify_preimage(&nonce_r, &id.fingerprint, &fp_r);
    write_frame(
        s,
        &ControlMsg::VerifyResponse {
            sig_b64: BASE64_STANDARD.encode(id.sign(&m_i)),
            public_key_b64: id.public_key_b64(),
            name: id.name.clone(),
        },
    )?;
    match read_frame(s)? {
        ControlMsg::Ok {} => {}
        ControlMsg::Error { message } => bail!("{message}"),
        other => bail!("unexpected message: {other:?}"),
    }
    Ok(peer)
}

pub fn verify_responder(
    s: &mut TcpStream,
    id: &LocalIdentity,
    store: &PeerStore,
) -> Result<PairedPeer> {
    let _ = s.set_nodelay(true);

    let (fp_i, nonce_i) = match read_frame(s)? {
        ControlMsg::VerifyHello { fingerprint, nonce_b64 } => (fingerprint, b64d(&nonce_b64)?),
        ControlMsg::Error { message } => bail!("{message}"),
        other => bail!("unexpected message: {other:?}"),
    };
    let peer = match store.find(&fp_i) {
        Some(p) => p.clone(),
        None => {
            let _ = write_frame(s, &ControlMsg::Error { message: "unknown peer".into() });
            bail!("unknown peer");
        }
    };
    let mut nonce_r = [0u8; 16];
    OsRng.fill_bytes(&mut nonce_r);
    write_frame(
        s,
        &ControlMsg::VerifyChallenge {
            nonce_b64: BASE64_STANDARD.encode(nonce_r),
        },
    )?;
    let m_r = verify_preimage(&nonce_i, &id.fingerprint, &fp_i);
    write_frame(
        s,
        &ControlMsg::VerifyResponse {
            sig_b64: BASE64_STANDARD.encode(id.sign(&m_r)),
            public_key_b64: id.public_key_b64(),
            name: id.name.clone(),
        },
    )?;
    let sig_i = match read_frame(s)? {
        ControlMsg::VerifyResponse { sig_b64, public_key_b64: _, name: _ } => b64d(&sig_b64)?,
        ControlMsg::Error { message } => bail!("{message}"),
        other => bail!("unexpected message: {other:?}"),
    };
    let m_i = verify_preimage(&nonce_r, &fp_i, &id.fingerprint);
    if !verify_sig(&peer.public_key_b64, &m_i, &sig_i) {
        let _ = write_frame(
            s,
            &ControlMsg::Error { message: "signature verification failed".into() },
        );
        bail!("signature verification failed");
    }
    write_frame(s, &ControlMsg::Ok {})?;
    Ok(peer)
}
