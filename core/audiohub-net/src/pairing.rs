use crate::control::{
    read_frame, write_frame, ControlIo, ControlMsg, PROTOCOL_VERSION, VERSION_ABSENT,
};
use crate::identity::{fingerprint_of, verify_sig, LocalIdentity, PairedPeer, PeerStore};
use anyhow::{anyhow, bail, Result};
use base64::prelude::*;
use hmac::{Hmac, Mac};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use std::time::{SystemTime, UNIX_EPOCH};

const IDENT_INITIATOR: &[u8] = b"audiohub-initiator";
const IDENT_RESPONDER: &[u8] = b"audiohub-responder";
const CONFIRM_LABEL_A: &[u8] = b"audiohub-confirm-A";
const CONFIRM_LABEL_B: &[u8] = b"audiohub-confirm-B";
const VERIFY_LABEL: &[u8] = b"audiohub-verify";
/// Domain separator for the refusal signature in `ControlMsg::Unpaired`. It has
/// to be distinct from every other label here — a signature that could be
/// lifted out of the verify exchange and replayed as a refusal would authorise
/// exactly the trust deletion this signature exists to gate. Not a prefix of
/// `VERIFY_LABEL` and not prefixed by it, so the two preimages can never
/// coincide whatever nonce and fingerprints follow.
const UNPAIRED_LABEL: &[u8] = b"audiohub-unpaired";

type HmacSha256 = Hmac<Sha256>;

pub struct PairOutcome {
    pub peer: PairedPeer,
}

/// The peer answered our verify with `ControlMsg::Unpaired` AND proved it was
/// the peer: it has removed us from its store, so we must remove it from ours —
/// and with it the pair of virtual devices we publish in its name, which would
/// otherwise stay in the system list forever, permanently offline, while a
/// reconnect loop redialled a machine that has already said no.
///
/// Only ever raised for a refusal whose signature verified against the key this
/// store already holds for `fingerprint`; an unsigned or unverifiable refusal
/// is an ordinary connection failure and leaves the pairing alone. Callers must
/// still check that `fingerprint` is the peer they meant to reach — this type
/// says who signed, not who was dialled.
///
/// A distinct type rather than a message string so the caller can `downcast_ref`
/// it out of an `anyhow` chain: acting on a substring match would make an error
/// message part of the contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnpairedByPeer {
    pub fingerprint: String,
}

impl std::fmt::Display for UnpairedByPeer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the peer has unpaired from us")
    }
}

impl std::error::Error for UnpairedByPeer {}

/// True when this error chain reports that the peer unpaired from us.
pub fn was_unpaired_by_peer(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<UnpairedByPeer>().is_some())
}

/// The machine at the other end of this connection is US: the handshake came
/// back carrying our own public key.
///
/// The honest self-dial check, and the reason it lives here: an address compare
/// only knows the endpoint we aimed at, while the handshake learns the identity
/// that actually answered — through a NAT hairpin, a forwarder or a stale peer
/// record it is the only one that can tell. Measured on 2026-07-31: a peer
/// record whose address resolved to this daemon's own control port made the
/// session coordinator dial the daemon itself, which then answered its own
/// `VerifyHello` with `Unpaired` (our fingerprint is not in our own store) and
/// deleted the pairing on the strength of it.
///
/// Never a reason to touch a pairing. Nothing was learned about the peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfConnection {
    pub fingerprint: String,
}

impl std::fmt::Display for SelfConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "this connection came back to ourselves ({})", self.fingerprint)
    }
}

impl std::error::Error for SelfConnection {}

/// True when this error chain reports that we connected to ourselves.
pub fn was_self_connection(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<SelfConnection>().is_some())
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
pub fn pair_initiator<T: ControlIo + ?Sized>(
    s: &mut T,
    pin: &str,
    id: &LocalIdentity,
    my_listen_port: u16,
) -> Result<PairOutcome> {
    let _ = s.set_nodelay(true);
    // Unchanged fallback: a transport with no peer address records port 0,
    // which the store already reads as "no port we can dial" (see `conn.rs`).
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
            alias: None,
        },
    })
}

/// Does NOT send the final `Ok` frame: caller must persist the peer first,
/// then send `ControlMsg::Ok {}` via `control::write_frame` (spec: responder
/// persists before Ok, initiator persists after receiving Ok).
pub fn pair_responder<T: ControlIo + ?Sized>(
    s: &mut T,
    pin: &str,
    id: &LocalIdentity,
) -> Result<PairOutcome> {
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
            alias: None,
        },
    })
}

/// What a responder signs to prove that IT is the machine refusing us.
///
/// Binds the initiator's fresh nonce (so the refusal cannot be replayed onto a
/// later connection), the responder's own fingerprint (so a refusal signed by
/// one peer cannot be presented as another's) and the fingerprint the initiator
/// claimed in its `VerifyHello` (so a refusal collected while A was dialling
/// cannot be replayed at B). Under its own label, so it is not a verify
/// signature and no verify signature is one of these.
fn unpaired_preimage(nonce_i: &[u8], fp_responder: &str, fp_initiator: &str) -> Vec<u8> {
    let mut m = Vec::with_capacity(UNPAIRED_LABEL.len() + nonce_i.len() + 32);
    m.extend_from_slice(UNPAIRED_LABEL);
    m.extend_from_slice(nonce_i);
    m.extend_from_slice(fp_responder.as_bytes());
    m.extend_from_slice(fp_initiator.as_bytes());
    m
}

/// Decides what an `Unpaired` frame is worth. `Ok(fp)` = a peer we have on file
/// proved it is refusing us, and the caller may drop that pairing; every `Err`
/// is an ordinary connection failure that must leave the store untouched.
///
/// The signature is checked against the key in OUR store, never the one on the
/// wire — a key that verifies its own signature proves nothing. The wire key is
/// used only to look the record up.
fn authenticate_unpaired(
    sig_b64: &str,
    public_key_b64: &str,
    nonce_i: &[u8],
    id: &LocalIdentity,
    store: &PeerStore,
) -> Result<String> {
    if sig_b64.is_empty() || public_key_b64.is_empty() {
        bail!(
            "the peer refused us as unpaired but did not sign the refusal (a responder from \
             before the signature existed, or something else answering on its address); keeping \
             the pairing"
        );
    }
    let fp_r = fingerprint_of(&pub_arr(public_key_b64)?);
    // Checked before the store lookup: our own fingerprint can never BE in our
    // own store, so leaving this to `find` would report the self-dial as an
    // impostor and bury the thing the operator actually has to fix.
    if fp_r == id.fingerprint {
        return Err(anyhow::Error::new(SelfConnection { fingerprint: fp_r }));
    }
    let peer = store
        .find(&fp_r)
        .ok_or_else(|| anyhow!("a machine we are not paired with ({fp_r}) refused us as unpaired"))?;
    let m = unpaired_preimage(nonce_i, &fp_r, &id.fingerprint);
    if !verify_sig(&peer.public_key_b64, &m, &b64d(sig_b64)?) {
        bail!("the refusal from {fp_r} is not signed by that peer's key; keeping the pairing");
    }
    Ok(fp_r)
}

/// The peer speaks a different control protocol version than we do.
///
/// A distinct type, like [`UnpairedByPeer`], so callers can recognise it out of
/// an `anyhow` chain without matching on message text. Unlike `UnpairedByPeer`
/// it must **never** edit the peer store: a version mismatch is a connection
/// failure between two machines that are still perfectly well paired, and the
/// user's fix is to redeploy the older end, not to pair again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolMismatch {
    pub ours: u32,
    pub theirs: u32,
}

impl std::fmt::Display for ProtocolMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.theirs == VERSION_ABSENT {
            write!(
                f,
                "the peer speaks a control protocol from before mode advertisement existed \
                 (we speak v{}); both machines have to run this build",
                self.ours
            )
        } else {
            write!(
                f,
                "control protocol mismatch: we speak v{}, the peer speaks v{}; both machines \
                 have to run the same build",
                self.ours, self.theirs
            )
        }
    }
}

impl std::error::Error for ProtocolMismatch {}

/// True when this error chain reports a control protocol version mismatch.
pub fn was_protocol_mismatch(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<ProtocolMismatch>().is_some())
}

/// Strict equality (plan §13). Deliberately not `theirs >= ours` or any other
/// ordering: mode advertisement changes what a peer is *allowed to do*, not
/// just what it can say, so "close enough" has no safe reading. The Windows
/// driver handshake already uses this exact posture for the same reason.
pub fn check_protocol(theirs: u32) -> Result<()> {
    if theirs == PROTOCOL_VERSION {
        return Ok(());
    }
    Err(anyhow::Error::new(ProtocolMismatch {
        ours: PROTOCOL_VERSION,
        theirs,
    }))
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
pub fn verify_initiator<T: ControlIo + ?Sized>(
    s: &mut T,
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
            version: PROTOCOL_VERSION,
        },
    )?;
    let nonce_r = match read_frame(s)? {
        ControlMsg::VerifyChallenge { nonce_b64, version } => {
            // Before anything is signed or stored. A refused version leaves the
            // pairing alone — the two machines are still paired, one of them is
            // just running a build that cannot state its mode.
            check_protocol(version)?;
            b64d(&nonce_b64)?
        }
        // The responder says it does not know us any more. Reported as a typed
        // error so the daemon can drop its own half of the pairing (and the
        // peer's virtual devices) instead of retrying forever — but ONLY once
        // the refusal is proved to come from that peer. Everything else here is
        // a failed connection, and a failed connection never edits the store.
        ControlMsg::Unpaired { sig_b64, public_key_b64 } => {
            let fingerprint = authenticate_unpaired(&sig_b64, &public_key_b64, &nonce_i, id, store)?;
            return Err(anyhow::Error::new(UnpairedByPeer { fingerprint }));
        }
        ControlMsg::Error { message } => bail!("{message}"),
        other => bail!("unexpected message: {other:?}"),
    };
    let (sig_r, pub_r_b64, name_r) = match read_frame(s)? {
        ControlMsg::VerifyResponse { sig_b64, public_key_b64, name } => {
            (b64d(&sig_b64)?, public_key_b64, name)
        }
        ControlMsg::Error { message } => bail!("{message}"),
        other => bail!("unexpected message: {other:?}"),
    };
    let fp_r = fingerprint_of(&pub_arr(&pub_r_b64)?);
    // The other end is this very daemon. Only reachable if our own fingerprint
    // somehow reached our own store, but reported the same way as on the
    // refusal path so a self-dial always ends in one recognisable error rather
    // than "unknown peer" — which reads as a stranger and hides the loop.
    if fp_r == id.fingerprint {
        return Err(anyhow::Error::new(SelfConnection { fingerprint: fp_r }));
    }
    let mut peer = store
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
    // Only AFTER the signature check: the name is unauthenticated text from the
    // wire, and adopting it before the peer has proved who it is would let
    // anyone who can reach this port relabel a device in our system list.
    // The caller persists it (spec-m5b §5.3 — this used to be discarded here,
    // which is why a renamed peer stayed under its old name forever).
    adopt_name(&mut peer, name_r);
    Ok(peer)
}

/// A peer's self-reported computer name, taken only when it is usable. Bounded
/// because it becomes a device name inside a fixed-size wire field, and a peer
/// that sends 64 KB of name must not be able to truncate the rest of it.
fn adopt_name(peer: &mut PairedPeer, name: String) {
    let name = name.trim();
    if name.is_empty() {
        return;
    }
    let mut end = name.len().min(96);
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    peer.name = name[..end].to_string();
}

pub fn verify_responder<T: ControlIo + ?Sized>(
    s: &mut T,
    id: &LocalIdentity,
    store: &PeerStore,
) -> Result<PairedPeer> {
    let _ = s.set_nodelay(true);

    let (fp_i, nonce_i) = match read_frame(s)? {
        ControlMsg::VerifyHello { fingerprint, nonce_b64, version } => {
            // Checked BEFORE the store lookup, so a version-mismatched peer is
            // told about the version rather than about its fingerprint — and,
            // more importantly, so it can never reach the `Unpaired` branch
            // below. That branch makes the initiator DELETE a pairing, and a
            // build we cannot speak to must not be able to trigger it.
            if let Err(e) = check_protocol(version) {
                let _ = write_frame(s, &ControlMsg::Error { message: e.to_string() });
                return Err(e);
            }
            (fingerprint, b64d(&nonce_b64)?)
        }
        ControlMsg::Error { message } => bail!("{message}"),
        other => bail!("unexpected message: {other:?}"),
    };
    let mut peer = match store.find(&fp_i) {
        Some(p) => p.clone(),
        None => {
            // Not the generic error any more: to a peer that WAS paired with us
            // this is the only signal that its copy of the pairing is dead, and
            // without it the pair of virtual devices bearing our name sits in
            // its system list forever (spec-m5b OPEN QUESTION 5). It carries no
            // more information than the refusal it replaces — an unknown
            // fingerprint is refused either way.
            //
            // Signed, because the initiator deletes a pairing over it. We no
            // longer know who `fp_i` is, but we do still know our own key, and
            // that is exactly what the initiator needs: proof that the refusal
            // came from the machine it dialled rather than from whatever else
            // reached that address first.
            let m = unpaired_preimage(&nonce_i, &id.fingerprint, &fp_i);
            let _ = write_frame(
                s,
                &ControlMsg::Unpaired {
                    sig_b64: BASE64_STANDARD.encode(id.sign(&m)),
                    public_key_b64: id.public_key_b64(),
                },
            );
            bail!("unknown peer");
        }
    };
    let mut nonce_r = [0u8; 16];
    OsRng.fill_bytes(&mut nonce_r);
    write_frame(
        s,
        &ControlMsg::VerifyChallenge {
            nonce_b64: BASE64_STANDARD.encode(nonce_r),
            version: PROTOCOL_VERSION,
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
    let (sig_i, name_i) = match read_frame(s)? {
        ControlMsg::VerifyResponse { sig_b64, public_key_b64: _, name } => {
            (b64d(&sig_b64)?, name)
        }
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
    // Same rule as the initiator side: adopted only once the signature holds.
    adopt_name(&mut peer, name_i);
    Ok(peer)
}

#[cfg(test)]
mod protocol_version_tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;

    fn tmp(tag: &str) -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let p = std::env::temp_dir().join(format!("ahb-pair-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).expect("mkdir");
        p
    }

    struct Party {
        id: LocalIdentity,
        dir: PathBuf,
    }

    impl Party {
        fn new(tag: &str) -> Party {
            let dir = tmp(tag);
            let id = LocalIdentity::load_or_create_at(Some(&dir)).expect("identity");
            Party { id, dir }
        }

        fn trust(&self, other: &Party) {
            let mut s = PeerStore::load_at(Some(&self.dir)).expect("store");
            s.upsert(PairedPeer {
                name: other.id.name.clone(),
                fingerprint: other.id.fingerprint.clone(),
                public_key_b64: other.id.public_key_b64(),
                last_addr: Some("127.0.0.1".into()),
                port: 47810,
                added_unix: 0,
                alias: None,
            });
            s.save().expect("save store");
        }
    }

    /// Serves ONE `verify_responder` on loopback, out of `dir`'s identity and
    /// store. Returns the listening address and the join handle.
    ///
    /// The responder is the PRODUCTION function — the whole point of these
    /// tests is that the shipped code refuses, not that a hand-written check
    /// would.
    fn serve(dir: &PathBuf) -> (std::net::SocketAddr, std::thread::JoinHandle<Result<PairedPeer>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let dir = dir.clone();
        let h = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().expect("accept");
            let id = LocalIdentity::load_or_create_at(Some(&dir)).expect("identity");
            let store = PeerStore::load_at(Some(&dir)).expect("store");
            verify_responder(&mut s, &id, &store)
        });
        (addr, h)
    }

    /// One real verify exchange, with the initiator's `VerifyHello.version`
    /// overridden. Returns what the responder concluded and what it replied.
    fn exchange(
        fingerprint: &str,
        hello_version: u32,
        dir: &PathBuf,
    ) -> (Result<PairedPeer>, Result<ControlMsg>) {
        let (addr, h) = serve(dir);
        let mut c = TcpStream::connect(addr).expect("connect");
        let mut nonce_i = [0u8; 16];
        OsRng.fill_bytes(&mut nonce_i);
        write_frame(
            &mut c,
            &ControlMsg::VerifyHello {
                fingerprint: fingerprint.to_string(),
                nonce_b64: BASE64_STANDARD.encode(nonce_i),
                version: hello_version,
            },
        )
        .expect("write hello");
        // Read the reply so the responder is never blocked on its own write.
        let reply = read_frame(&mut c);
        drop(c);
        (h.join().expect("responder thread"), reply)
    }

    fn pair_of(tag: &str) -> (Party, Party) {
        let a = Party::new(&format!("{tag}-init"));
        let b = Party::new(&format!("{tag}-resp"));
        a.trust(&b);
        b.trust(&a);
        (a, b)
    }

    fn cleanup(parties: [&Party; 2]) {
        for p in parties {
            let _ = std::fs::remove_dir_all(&p.dir);
        }
    }

    /// The control case. Without it every assertion below would pass just as
    /// well against a responder that refuses everything — which is exactly the
    /// shape of "green test, dead feature" this project keeps paying for.
    #[test]
    fn a_matching_version_clears_the_gate() {
        let (a, b) = pair_of("match");
        let (out, _) = exchange(&a.id.fingerprint, PROTOCOL_VERSION, &b.dir);
        // The exchange still fails afterwards — our stub initiator never signs
        // — but it must NOT fail as a version mismatch.
        let e = out.expect_err("the stub initiator cannot complete the exchange");
        assert!(
            !was_protocol_mismatch(&e),
            "a peer speaking our version must clear the version gate, got: {e:#}"
        );
        cleanup([&a, &b]);
    }

    /// A peer one version behind is refused by the real responder, before any
    /// signature is exchanged.
    #[test]
    fn a_stale_version_is_refused_by_the_real_responder() {
        let (a, b) = pair_of("stale");
        let (out, reply) = exchange(&a.id.fingerprint, PROTOCOL_VERSION - 1, &b.dir);
        let e = out.expect_err("must be refused");
        assert!(was_protocol_mismatch(&e), "expected a version mismatch, got: {e:#}");
        assert!(
            matches!(reply, Ok(ControlMsg::Error { .. })),
            "the initiator has to be TOLD, not just dropped: {reply:?}"
        );
        cleanup([&a, &b]);
    }

    /// A peer that predates versioning entirely sends no `version` field at
    /// all, which decodes as `VERSION_ABSENT`. It is refused, and the message
    /// names the actual problem rather than quoting a v0 it never sent.
    ///
    /// This is the case that matters: plan §13 mode advertisement is not
    /// additive, so a build that cannot state its mode must not be talked to.
    /// Before this change such a peer connected happily and was listed as
    /// usable while it was simultaneously a provider and a consumer.
    #[test]
    fn a_peer_from_before_versioning_is_refused_and_told_why() {
        let (a, b) = pair_of("absent");
        let (out, _) = exchange(&a.id.fingerprint, VERSION_ABSENT, &b.dir);
        let e = out.expect_err("must be refused");
        assert!(was_protocol_mismatch(&e), "expected a version mismatch, got: {e:#}");
        let text = format!("{e:#}");
        assert!(
            text.contains("before mode advertisement"),
            "the message must name the real problem, not quote a version nobody sent: {text}"
        );
        cleanup([&a, &b]);
    }

    /// The version check has to sit AHEAD of the store lookup, because the
    /// lookup's miss path answers `Unpaired` — a frame the initiator acts on by
    /// DELETING a pairing. A build we cannot speak to must not be able to reach
    /// it.
    ///
    /// Pinned with a fingerprint the responder has never heard of: a responder
    /// that looked up first would answer `Unpaired` here instead of refusing on
    /// the version, and this test would catch exactly that reordering.
    #[test]
    fn a_version_mismatch_is_decided_before_the_peer_is_looked_up() {
        let b = Party::new("order-resp");
        let (out, reply) = exchange("ffffffffffffffff", VERSION_ABSENT, &b.dir);
        let e = out.expect_err("must be refused");
        assert!(was_protocol_mismatch(&e), "expected a version mismatch, got: {e:#}");
        match reply {
            Ok(ControlMsg::Error { .. }) => {}
            Ok(ControlMsg::Unpaired { .. }) => panic!(
                "the responder answered Unpaired to a version-mismatched stranger: acting on \
                 that frame deletes a pairing, and a peer we refuse to speak to must not be \
                 able to trigger it"
            ),
            other => panic!("unexpected reply: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&b.dir);
    }

    /// **A refused handshake leaves the peer store byte-identical.**
    ///
    /// M8 acceptance (design §6, P3 item 5). The M8 deployment rule is "rebuild
    /// both ends inside one window", and the safety net for getting that wrong
    /// is this gate. A net that *also* edited the store while refusing would be
    /// worse than none: the operator's fix is to upgrade and reconnect, and
    /// they would be reconnecting into a record something already touched
    /// while it was refusing to talk.
    ///
    /// Byte comparison of the file, not a field-by-field one: the failure being
    /// guarded against is "something wrote to it", and a field comparison only
    /// covers the fields whoever wrote the test thought of.
    ///
    /// Injection control: make `verify_responder` call `store.upsert(..)`
    /// before `check_protocol` and this goes red on the byte compare.
    #[test]
    fn a_refused_version_leaves_both_peer_stores_untouched() {
        let (a, b) = pair_of("nostorewrite");
        let store_path = |p: &Party| p.dir.join("paired_peers.json");
        let before: Vec<Vec<u8>> =
            [&a, &b].iter().map(|p| std::fs::read(store_path(p)).expect("store")).collect();

        // One version behind: the shape M8 produces when only one end is rebuilt.
        let (out, reply) = exchange(&a.id.fingerprint, PROTOCOL_VERSION - 1, &b.dir);
        assert!(was_protocol_mismatch(&out.expect_err("must be refused")));
        assert!(matches!(reply, Ok(ControlMsg::Error { .. })), "the peer must be told: {reply:?}");

        for (p, was) in [&a, &b].iter().zip(before) {
            let now = std::fs::read(store_path(p)).expect("store");
            assert_eq!(
                now, was,
                "{}'s peer store changed during a refused handshake",
                p.id.fingerprint
            );
        }
        cleanup([&a, &b]);
    }

    /// Strict equality in BOTH directions. "Newer is fine" is the reading that
    /// quietly reopens the gap: a newer peer may define a mode whose exclusion
    /// rules this build cannot honour, and we would list it as usable.
    #[test]
    fn the_gate_is_equality_not_a_minimum() {
        assert!(check_protocol(PROTOCOL_VERSION).is_ok());
        assert!(check_protocol(PROTOCOL_VERSION + 1).is_err(), "a newer peer is refused too");
        assert!(check_protocol(PROTOCOL_VERSION - 1).is_err());
    }
}
