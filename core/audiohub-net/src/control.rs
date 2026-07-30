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
