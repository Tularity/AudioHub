//! Post-verify encryption upgrade for the control channel (spec-m4a §2).
//! Callers run M3 verify first, then establish_* on the same TcpStream.

use std::io::{ErrorKind, Read};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use base64::prelude::*;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroize;

use crate::control::{read_frame, write_frame, ControlMsg, CONTROL_MAX_FRAME};
use crate::identity::{verify_sig, LocalIdentity, PairedPeer};

const SEC_LABEL_I: &[u8] = b"audiohub-sec-i";
const SEC_LABEL_R: &[u8] = b"audiohub-sec-r";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Length of the per-stream media salt carried by OpenStream.
pub const MEDIA_SALT_LEN: usize = 16;

/// Fresh `media_salt_b64` for an OpenStream (16 random bytes, base64).
pub fn new_media_salt_b64() -> String {
    let mut salt = [0u8; MEDIA_SALT_LEN];
    OsRng.fill_bytes(&mut salt);
    BASE64_STANDARD.encode(salt)
}

/// Decode a peer-supplied `media_salt_b64`. Rejects anything but exactly
/// MEDIA_SALT_LEN bytes so a peer cannot force a short/empty HKDF salt.
pub fn decode_media_salt(media_salt_b64: &str) -> Result<[u8; MEDIA_SALT_LEN]> {
    let raw = b64d(media_salt_b64).context("media_salt_b64")?;
    raw.try_into()
        .map_err(|_| anyhow!("media_salt_b64 must decode to {MEDIA_SALT_LEN} bytes"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionMsg {
    OpenStream {
        stream_id: u32,
        kind: String, // "mic" | "spk", from the OpenStream sender's perspective
        dir: String,  // "send" | "recv": media flow relative to the OpenStream sender
        sample_rate: u32,
        channels: u8,
        /// Required. 16 random bytes, base64. The stream OPENER generates one
        /// per stream regardless of direction; both sides feed it to
        /// MediaCrypto::new_for_stream so reopening a stream_id cannot repeat
        /// a media keystream. No serde default: a peer omitting it is invalid.
        media_salt_b64: String,
        #[serde(default)]
        verify_freq: Option<f32>,
        #[serde(default)]
        source: Option<String>,
        #[serde(default)]
        freq: Option<f32>,
        /// `source == "sysaudio"` only: which capture backend the provider must
        /// use. Absent = `sysaudio::BACKEND_AUTO`.
        #[serde(default)]
        backend: Option<String>,
        #[serde(default)]
        simulate_loss_pct: Option<f32>,
        /// spk only: mirror the consumer's slider onto the provider's real
        /// default output device (spec-m4b §A2). serde default so a peer that
        /// predates the field still opens streams.
        #[serde(default)]
        volume_sync: bool,
    },
    AcceptStream {
        stream_id: u32,
    },
    RejectStream {
        stream_id: u32,
        reason: String,
    },
    CloseStream {
        stream_id: u32,
    },
    Stats {
        stream_id: u32,
        received: u64,
        lost: u64,
        loss_pct: f64,
        jitter_ms: f64,
    },
    /// Consumer -> provider: apply this to the provider's real default output
    /// device (spec-m4b §A2). Never applies any gain to the media stream —
    /// volume is a control-plane property.
    ///
    /// `muted` absent/null = volume only, LEAVE the provider's mute control
    /// alone. A bare slider drag must not unmute a machine somebody muted.
    ///
    /// `src` is the frozen anti-ping-pong tag. In practice it is always
    /// `volume::SRC_LOCAL`: a consumer never re-emits a change it received, so
    /// nothing is ever relayed and `volume::SRC_PEER` is never put on the wire
    /// by any AudioHub build. The field stays because the wire shape is frozen
    /// and because the daemon refuses anything but `local`.
    VolumeSet {
        stream_id: u32,
        scalar: f32,
        #[serde(default)]
        muted: Option<bool>,
        src: String,
    },
    /// Provider -> consumer: what the provider's output device actually reads
    /// now. `adjustable=false` means the device has no volume we can drive, so
    /// the consumer shows the value but disables its slider.
    VolumeState {
        stream_id: u32,
        scalar: f32,
        muted: bool,
        adjustable: bool,
    },
    Ping {
        t_us: u64,
    },
    Pong {
        t_us: u64,
    },
    /// "I have unpaired from you." Sent immediately before `Bye` when the local
    /// user removes a pairing while the channel is up (plan §7.1, ruled in
    /// 2026-07-31).
    ///
    /// The refusal at the next verify (`ControlMsg::Unpaired`) covers the peer
    /// that dials US; this covers the peer that never does, which would
    /// otherwise never find out and would keep a pair of virtual devices in our
    /// name in its system list forever. A peer that predates this variant fails
    /// to parse it and drops the channel — the same thing the `Bye` a
    /// microsecond later would have done.
    Unpaired {},
    Bye {},
}

/// Media-plane AEAD keys, mapped to this endpoint's send/recv direction.
#[derive(Clone)]
pub struct MediaKeys {
    pub tx: [u8; 32],
    pub rx: [u8; 32],
}

struct DerivedKeys {
    c_tx: [u8; 32],
    c_rx: [u8; 32],
    media: MediaKeys,
}

impl Drop for DerivedKeys {
    fn drop(&mut self) {
        self.c_tx.zeroize();
        self.c_rx.zeroize();
        self.media.tx.zeroize();
        self.media.rx.zeroize();
    }
}

fn hkdf_expand(hk: &Hkdf<Sha256>, label: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    hk.expand(label, &mut out).expect("hkdf expand 32B");
    out
}

fn derive_keys(ss: &[u8], nonce_i: &[u8], nonce_r: &[u8], initiator: bool) -> DerivedKeys {
    let mut salt = Vec::with_capacity(nonce_i.len() + nonce_r.len());
    salt.extend_from_slice(nonce_i);
    salt.extend_from_slice(nonce_r);
    let hk = Hkdf::<Sha256>::new(Some(&salt), ss);
    let mut c_i2r = hkdf_expand(&hk, b"c-i2r");
    let mut c_r2i = hkdf_expand(&hk, b"c-r2i");
    let mut m_i2r = hkdf_expand(&hk, b"m-i2r");
    let mut m_r2i = hkdf_expand(&hk, b"m-r2i");
    let out = if initiator {
        DerivedKeys {
            c_tx: c_i2r,
            c_rx: c_r2i,
            media: MediaKeys { tx: m_i2r, rx: m_r2i },
        }
    } else {
        DerivedKeys {
            c_tx: c_r2i,
            c_rx: c_i2r,
            media: MediaKeys { tx: m_r2i, rx: m_i2r },
        }
    };
    // [u8; 32] is Copy, so the struct above holds copies: wipe the locals.
    c_i2r.zeroize();
    c_r2i.zeroize();
    m_i2r.zeroize();
    m_r2i.zeroize();
    out
}

// frozen preimages (spec-m4a §2)
fn sig_preimage_i(eph_i: &[u8; 32], nonce_i: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(SEC_LABEL_I.len() + 32 + nonce_i.len());
    m.extend_from_slice(SEC_LABEL_I);
    m.extend_from_slice(eph_i);
    m.extend_from_slice(nonce_i);
    m
}

fn sig_preimage_r(eph_r: &[u8; 32], nonce_r: &[u8], eph_i: &[u8; 32]) -> Vec<u8> {
    let mut m = Vec::with_capacity(SEC_LABEL_R.len() + 64 + nonce_r.len());
    m.extend_from_slice(SEC_LABEL_R);
    m.extend_from_slice(eph_r);
    m.extend_from_slice(nonce_r);
    m.extend_from_slice(eph_i);
    m
}

fn b64d(s: &str) -> Result<Vec<u8>> {
    BASE64_STANDARD
        .decode(s)
        .map_err(|e| anyhow!("bad base64: {e}"))
}

fn arr32(v: &[u8]) -> Result<[u8; 32]> {
    v.try_into().map_err(|_| anyhow!("expected 32 bytes"))
}

fn ctr_nonce(n: u64) -> [u8; 12] {
    let mut out = [0u8; 12];
    out[..8].copy_from_slice(&n.to_le_bytes());
    out
}

pub struct SecureChannel {
    stream: TcpStream,
    tx_cipher: ChaCha20Poly1305,
    rx_cipher: ChaCha20Poly1305,
    tx_n: u64,
    rx_seen: Option<u64>, // highest accepted n; anything <= is a replay
    /// Set when a write fails: the peer may have received a partial frame, and
    /// the nonce reserved for it is burned. The channel is unusable afterwards.
    poisoned: bool,
    /// Session messages that decrypted fine but did not parse (unknown variant
    /// from a newer peer, truncated JSON). Counted and skipped, never fatal.
    bad_session_msgs: u64,
    media: MediaKeys,
    peer: PairedPeer,
    rd_buf: Vec<u8>,
}

impl SecureChannel {
    pub fn establish_initiator(
        mut s: TcpStream,
        id: &LocalIdentity,
        peer: &PairedPeer,
    ) -> Result<SecureChannel> {
        let _ = s.set_nodelay(true);
        s.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;

        let eph_secret = StaticSecret::random_from_rng(OsRng);
        let eph_pub = PublicKey::from(&eph_secret);
        let mut nonce_i = [0u8; 16];
        OsRng.fill_bytes(&mut nonce_i);
        let sig = id.sign(&sig_preimage_i(eph_pub.as_bytes(), &nonce_i));
        write_frame(
            &mut s,
            &ControlMsg::SecInit {
                eph_pub_b64: BASE64_STANDARD.encode(eph_pub.as_bytes()),
                nonce_b64: BASE64_STANDARD.encode(nonce_i),
                sig_b64: BASE64_STANDARD.encode(sig),
            },
        )?;

        let (eph_r_b64, nonce_r_b64, sig_r_b64) = match read_frame(&mut s)? {
            ControlMsg::SecResp { eph_pub_b64, nonce_b64, sig_b64 } => {
                (eph_pub_b64, nonce_b64, sig_b64)
            }
            ControlMsg::Error { message } => bail!("{message}"),
            other => bail!("unexpected message: {other:?}"),
        };
        let eph_r = arr32(&b64d(&eph_r_b64)?)?;
        let nonce_r = b64d(&nonce_r_b64)?;
        let sig_r = b64d(&sig_r_b64)?;
        let m_r = sig_preimage_r(&eph_r, &nonce_r, eph_pub.as_bytes());
        if !verify_sig(&peer.public_key_b64, &m_r, &sig_r) {
            let _ = write_frame(
                &mut s,
                &ControlMsg::Error { message: "secure handshake signature invalid".into() },
            );
            bail!("secure handshake signature invalid");
        }

        let ss = eph_secret.diffie_hellman(&PublicKey::from(eph_r));
        if !ss.was_contributory() {
            bail!("degenerate x25519 shared secret");
        }
        let keys = derive_keys(ss.as_bytes(), &nonce_i, &nonce_r, true);
        Ok(SecureChannel::from_parts(s, keys, peer.clone()))
    }

    pub fn establish_responder(
        mut s: TcpStream,
        id: &LocalIdentity,
        peer: &PairedPeer,
    ) -> Result<SecureChannel> {
        let _ = s.set_nodelay(true);
        s.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;

        let (eph_i_b64, nonce_i_b64, sig_i_b64) = match read_frame(&mut s)? {
            ControlMsg::SecInit { eph_pub_b64, nonce_b64, sig_b64 } => {
                (eph_pub_b64, nonce_b64, sig_b64)
            }
            ControlMsg::Error { message } => bail!("{message}"),
            other => bail!("unexpected message: {other:?}"),
        };
        let eph_i = arr32(&b64d(&eph_i_b64)?)?;
        let nonce_i = b64d(&nonce_i_b64)?;
        let sig_i = b64d(&sig_i_b64)?;
        if !verify_sig(&peer.public_key_b64, &sig_preimage_i(&eph_i, &nonce_i), &sig_i) {
            let _ = write_frame(
                &mut s,
                &ControlMsg::Error { message: "secure handshake signature invalid".into() },
            );
            bail!("secure handshake signature invalid");
        }

        let eph_secret = StaticSecret::random_from_rng(OsRng);
        let eph_pub = PublicKey::from(&eph_secret);
        let mut nonce_r = [0u8; 16];
        OsRng.fill_bytes(&mut nonce_r);
        let sig = id.sign(&sig_preimage_r(eph_pub.as_bytes(), &nonce_r, &eph_i));
        write_frame(
            &mut s,
            &ControlMsg::SecResp {
                eph_pub_b64: BASE64_STANDARD.encode(eph_pub.as_bytes()),
                nonce_b64: BASE64_STANDARD.encode(nonce_r),
                sig_b64: BASE64_STANDARD.encode(sig),
            },
        )?;

        let ss = eph_secret.diffie_hellman(&PublicKey::from(eph_i));
        if !ss.was_contributory() {
            bail!("degenerate x25519 shared secret");
        }
        let keys = derive_keys(ss.as_bytes(), &nonce_i, &nonce_r, false);
        Ok(SecureChannel::from_parts(s, keys, peer.clone()))
    }

    fn from_parts(stream: TcpStream, keys: DerivedKeys, peer: PairedPeer) -> SecureChannel {
        SecureChannel {
            stream,
            tx_cipher: ChaCha20Poly1305::new(Key::from_slice(&keys.c_tx)),
            rx_cipher: ChaCha20Poly1305::new(Key::from_slice(&keys.c_rx)),
            tx_n: 0,
            rx_seen: None,
            poisoned: false,
            bad_session_msgs: 0,
            media: keys.media.clone(),
            peer,
            rd_buf: Vec::new(),
        }
        // `keys` (and the media copy inside it) is wiped by DerivedKeys::drop.
    }

    pub fn send(&mut self, msg: &SessionMsg) -> Result<()> {
        let plain = serde_json::to_vec(msg).context("serialize session message")?;
        self.send_raw_payload(&plain)
    }

    /// Sends an already-serialized session payload, including shapes this build
    /// cannot construct (a variant from a newer peer). Same nonce discipline as
    /// send(); exists so forward-compat handling is testable.
    #[doc(hidden)]
    pub fn send_raw_payload(&mut self, plain: &[u8]) -> Result<()> {
        if self.poisoned {
            bail!("secure channel unusable after an earlier write failure");
        }
        // Reserve the counter BEFORE encrypting: on any failure below, n is
        // burned rather than reused, because a partial write may already have
        // put ciphertext for this nonce on the wire.
        let n = self.tx_n;
        self.tx_n += 1;
        let ct = self
            .tx_cipher
            .encrypt(Nonce::from_slice(&ctr_nonce(n)), plain.as_ref())
            .map_err(|_| anyhow!("control encrypt failed"))?;
        if let Err(e) = write_frame(
            &mut self.stream,
            &ControlMsg::Enc { n, data_b64: BASE64_STANDARD.encode(ct) },
        ) {
            self.poisoned = true;
            return Err(e).context("write secure control frame");
        }
        Ok(())
    }

    /// None = nothing valid arrived before the timeout. Replayed frames
    /// (n <= highest seen) and unparseable session messages are skipped;
    /// tampering, framing errors and plaintext frames are errors.
    pub fn recv_timeout(&mut self, t: Duration) -> Result<Option<SessionMsg>> {
        if self.poisoned {
            bail!("secure channel unusable after an earlier write failure");
        }
        let deadline = Instant::now() + t;
        loop {
            while let Some(body) = self.take_frame()? {
                // Fixed message on purpose: serde quotes the offending input
                // (e.g. "unknown variant `...`"), which would echo peer text.
                let msg: ControlMsg = match serde_json::from_slice(&body) {
                    Ok(m) => m,
                    Err(_) => bail!("malformed control frame on encrypted channel"),
                };
                match msg {
                    ControlMsg::Enc { n, data_b64 } => {
                        if self.rx_seen.map_or(false, |seen| n <= seen) {
                            continue; // replay
                        }
                        let ct = b64d(&data_b64)?;
                        let pt = self
                            .rx_cipher
                            .decrypt(Nonce::from_slice(&ctr_nonce(n)), ct.as_ref())
                            .map_err(|_| anyhow!("control decrypt failed (tampered frame?)"))?;
                        self.rx_seen = Some(n);
                        match serde_json::from_slice::<SessionMsg>(&pt) {
                            Ok(sm) => return Ok(Some(sm)),
                            Err(_) => {
                                // Authenticated but undecodable (newer peer,
                                // truncated JSON): skip it. Only decrypt and
                                // framing failures may drop the whole conn.
                                self.bad_session_msgs += 1;
                                if self.bad_session_msgs == 1 {
                                    eprintln!(
                                        "[audiohub-net] secure: skipping undecodable session \
                                         message(s) from peer; further ones are only counted"
                                    );
                                }
                                continue;
                            }
                        }
                    }
                    // Post-handshake the peer speaks only Enc. Anything else is
                    // an off-path injection: fail with a FIXED message and never
                    // echo peer-supplied text (a plaintext Error{message} used
                    // to tear the session down and print the attacker's string).
                    _ => bail!("plaintext control frame on encrypted channel"),
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            let remaining = (deadline - now).max(Duration::from_millis(1));
            self.stream.set_read_timeout(Some(remaining))?;
            let mut tmp = [0u8; 4096];
            match self.stream.read(&mut tmp) {
                Ok(0) => bail!("connection closed by peer"),
                Ok(k) => self.rd_buf.extend_from_slice(&tmp[..k]),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return Ok(None)
                }
                Err(e) => return Err(e).context("read secure channel"),
            }
        }
    }

    // one length-prefixed frame from rd_buf, if complete
    fn take_frame(&mut self) -> Result<Option<Vec<u8>>> {
        if self.rd_buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes(self.rd_buf[..4].try_into().unwrap()) as usize;
        if len > CONTROL_MAX_FRAME {
            bail!("control frame too large: {len} bytes");
        }
        if self.rd_buf.len() < 4 + len {
            return Ok(None);
        }
        let body = self.rd_buf[4..4 + len].to_vec();
        self.rd_buf.drain(..4 + len);
        Ok(Some(body))
    }

    pub fn media_keys(&self) -> MediaKeys {
        self.media.clone()
    }

    pub fn peer(&self) -> &PairedPeer {
        &self.peer
    }

    pub fn peer_addr(&self) -> std::io::Result<SocketAddr> {
        self.stream.peer_addr()
    }

    /// Next nonce counter to be reserved by send(); also the number of nonces
    /// consumed so far (a failed send burns one).
    pub fn tx_counter(&self) -> u64 {
        self.tx_n
    }

    /// Decrypted-but-undecodable session messages skipped so far.
    pub fn bad_session_msgs(&self) -> u64 {
        self.bad_session_msgs
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }
}

impl Drop for SecureChannel {
    fn drop(&mut self) {
        // Control-plane keys live inside the ChaCha ciphers (wiped by their own
        // Drop); the media keys are our plain copies.
        self.media.tx.zeroize();
        self.media.rx.zeroize();
    }
}
