pub const MAGIC: [u8; 4] = *b"AUHB";
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Media = 0,
    EchoReq = 1,
    EchoResp = 2,
    PullReq = 3,
    Bye = 4,
}

impl Kind {
    fn from_u8(v: u8) -> Option<Kind> {
        Some(match v {
            0 => Kind::Media,
            1 => Kind::EchoReq,
            2 => Kind::EchoResp,
            3 => Kind::PullReq,
            4 => Kind::Bye,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    PcmS16le = 0,
    PcmF32le = 1,
    Opus = 2,
    Passthrough = 255,
}

impl Codec {
    fn from_u8(v: u8) -> Option<Codec> {
        Some(match v {
            0 => Codec::PcmS16le,
            1 => Codec::PcmF32le,
            2 => Codec::Opus,
            255 => Codec::Passthrough,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    pub kind: Kind,
    pub codec: Codec,
    pub channels: u8,
    pub sample_rate: u32,
    pub session_id: u64,
    pub stream_id: u32,
    pub seq: u32,
    pub timestamp_us: u64,
    pub payload_len: u32,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PacketError {
    #[error("datagram too short")]
    TooShort,
    #[error("bad magic")]
    BadMagic,
    #[error("bad version")]
    BadVersion,
    #[error("bad kind")]
    BadKind,
    #[error("bad codec")]
    BadCodec,
    #[error("payload length mismatch")]
    LengthMismatch,
}

impl Header {
    pub fn encode(&self, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
        out.extend_from_slice(&MAGIC);
        out.push(VERSION);
        out.push(self.kind as u8);
        out.push(self.codec as u8);
        out.push(self.channels);
        out.extend_from_slice(&self.sample_rate.to_le_bytes());
        out.extend_from_slice(&self.session_id.to_le_bytes());
        out.extend_from_slice(&self.stream_id.to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.timestamp_us.to_le_bytes());
        out.extend_from_slice(&self.payload_len.to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    pub fn parse(datagram: &[u8]) -> Result<(Header, &[u8]), PacketError> {
        if datagram.len() < HEADER_LEN {
            return Err(PacketError::TooShort);
        }
        if datagram[0..4] != MAGIC {
            return Err(PacketError::BadMagic);
        }
        if datagram[4] != VERSION {
            return Err(PacketError::BadVersion);
        }
        let kind = Kind::from_u8(datagram[5]).ok_or(PacketError::BadKind)?;
        let codec = Codec::from_u8(datagram[6]).ok_or(PacketError::BadCodec)?;
        let channels = datagram[7];
        let le_u32 = |o: usize| u32::from_le_bytes(datagram[o..o + 4].try_into().unwrap());
        let le_u64 = |o: usize| u64::from_le_bytes(datagram[o..o + 8].try_into().unwrap());
        let payload_len = le_u32(36);
        if payload_len as usize != datagram.len() - HEADER_LEN {
            return Err(PacketError::LengthMismatch);
        }
        Ok((
            Header {
                kind,
                codec,
                channels,
                sample_rate: le_u32(8),
                session_id: le_u64(12),
                stream_id: le_u32(20),
                seq: le_u32(24),
                timestamp_us: le_u64(28),
                payload_len,
            },
            &datagram[HEADER_LEN..],
        ))
    }
}
