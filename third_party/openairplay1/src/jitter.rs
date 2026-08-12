//! Sequence-ordered jitter buffer for RAOP audio.
//!
//! UDP audio packets arrive out of order, duplicated, or not at all. This
//! buffer accepts them keyed by their 16-bit RTP sequence number and releases
//! them strictly in order, so the player sees a clean stream. Gaps are
//! reported for retransmit; a gap that can't be filled before the buffer runs
//! too far ahead is skipped and reported as lost so the player can conceal it
//! with silence and keep timing.

/// Capacity in packets. Packets more than this far ahead of the
/// next-to-deliver are dropped on insert, so they can never alias a live slot.
pub const CAPACITY: usize = 512;

/// Maximum lead (highest received seq minus next-to-deliver) before the buffer
/// gives up on a missing packet and force-skips it. At 352 frames per packet
/// and 44.1 kHz this is ~1 s of audio, which bounds how long a gap can stall
/// playback while retransmits are attempted (one every
/// [`RESEND_BACKOFF`](crate::session)).
///
/// **This must stay strictly below [`CAPACITY`]**, and the constructor asserts
/// it: `insert` drops anything `CAPACITY` or more ahead, so with the two equal
/// the highest stored sequence can never reach the skip threshold and a gap
/// the sender never fills stalls the stream forever.
pub const MAX_LEAD: usize = 128;

/// Signed distance from `b` to `a` on the 16-bit sequence circle: positive
/// when `a` is ahead of `b`, handling wraparound at 0xFFFF→0x0000.
pub fn seq_diff(a: u16, b: u16) -> i32 {
    (a.wrapping_sub(b) as i16) as i32
}

/// What `pop_ready` hands to the player, in order.
#[derive(Debug, PartialEq, Eq)]
pub enum Delivery {
    /// A recovered ALAC frame for this sequence number.
    Packet { seq: u16, frame: Vec<u8> },
    /// This sequence number was lost; the player should insert silence.
    Lost { seq: u16 },
}

pub struct JitterBuffer {
    slots: Vec<Option<Vec<u8>>>,
    /// Next sequence to deliver; `None` until the first packet arrives.
    next_seq: Option<u16>,
    /// Highest sequence stored, for lead/skip decisions.
    highest_seq: Option<u16>,
    /// Force-skip when the lead reaches this many packets (≤ CAPACITY).
    max_lead: usize,
}

impl Default for JitterBuffer {
    fn default() -> Self {
        JitterBuffer::new(MAX_LEAD)
    }
}

impl JitterBuffer {
    pub fn new(max_lead: usize) -> JitterBuffer {
        // Strictly below CAPACITY: see MAX_LEAD — at CAPACITY the force-skip
        // can never fire and a permanent gap stalls playback forever.
        assert!(max_lead < CAPACITY);
        JitterBuffer {
            slots: (0..CAPACITY).map(|_| None).collect(),
            next_seq: None,
            highest_seq: None,
            max_lead,
        }
    }

    fn slot(seq: u16) -> usize {
        seq as usize % CAPACITY
    }

    /// Discard everything and (optionally) set the next expected sequence.
    /// Used on FLUSH/seek so stale audio isn't played.
    pub fn reset(&mut self, next_seq: Option<u16>) {
        for slot in &mut self.slots {
            *slot = None;
        }
        self.next_seq = next_seq;
        self.highest_seq = None;
    }

    /// Insert a received packet. Packets older than the next-to-deliver, or so
    /// far ahead they'd alias a live slot, are dropped.
    pub fn insert(&mut self, seq: u16, frame: Vec<u8>) {
        let next = *self.next_seq.get_or_insert(seq);
        let ahead = seq_diff(seq, next);
        // Older than what we're about to deliver, or beyond one ring of lead.
        if ahead < 0 || ahead >= CAPACITY as i32 {
            return;
        }
        self.slots[Self::slot(seq)] = Some(frame);
        if self.highest_seq.is_none_or(|h| seq_diff(seq, h) > 0) {
            self.highest_seq = Some(seq);
        }
    }

    /// Release as many in-order packets as are ready. Stops at the first gap
    /// unless the buffer has run `max_lead` ahead, in which case the missing
    /// packet at the front is skipped (reported `Lost`) so playback proceeds.
    pub fn pop_ready(&mut self) -> Vec<Delivery> {
        let mut out = Vec::new();
        let Some(highest) = self.highest_seq else {
            return out;
        };
        while let Some(next) = self.next_seq {
            let idx = Self::slot(next);
            if let Some(frame) = self.slots[idx].take() {
                out.push(Delivery::Packet { seq: next, frame });
                self.next_seq = Some(next.wrapping_add(1));
            } else if seq_diff(highest, next) >= self.max_lead as i32 {
                // Gap at the front and we're too far ahead — give up on it.
                out.push(Delivery::Lost { seq: next });
                self.next_seq = Some(next.wrapping_add(1));
            } else {
                break;
            }
        }
        out
    }

    /// Missing sequence numbers between the next-to-deliver and the highest
    /// received, for the retransmit requester.
    pub fn missing(&self) -> Vec<u16> {
        let (Some(next), Some(highest)) = (self.next_seq, self.highest_seq) else {
            return Vec::new();
        };
        let mut missing = Vec::new();
        let mut seq = next;
        while seq_diff(highest, seq) >= 0 {
            if self.slots[Self::slot(seq)].is_none() {
                missing.push(seq);
            }
            seq = seq.wrapping_add(1);
        }
        missing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(tag: u8) -> Vec<u8> {
        vec![tag]
    }

    fn pkt(seq: u16, tag: u8) -> Delivery {
        Delivery::Packet {
            seq,
            frame: frame(tag),
        }
    }

    #[test]
    fn seq_diff_handles_wrap() {
        assert_eq!(seq_diff(5, 3), 2);
        assert_eq!(seq_diff(3, 5), -2);
        assert_eq!(seq_diff(0, 0xFFFF), 1);
        assert_eq!(seq_diff(0xFFFF, 0), -1);
        assert_eq!(seq_diff(1, 0xFFFE), 3);
    }

    #[test]
    fn in_order_passthrough() {
        let mut jb = JitterBuffer::default();
        jb.insert(10, frame(1));
        jb.insert(11, frame(2));
        assert_eq!(jb.pop_ready(), vec![pkt(10, 1), pkt(11, 2)]);
        assert_eq!(jb.pop_ready(), vec![]);
    }

    #[test]
    fn reorders_before_delivery() {
        let mut jb = JitterBuffer::default();
        jb.insert(10, frame(1));
        jb.insert(12, frame(3)); // arrives before 11
        assert_eq!(jb.pop_ready(), vec![pkt(10, 1)]); // 11 missing, hold
        jb.insert(11, frame(2));
        assert_eq!(jb.pop_ready(), vec![pkt(11, 2), pkt(12, 3)]);
    }

    #[test]
    fn drops_duplicate_and_late_packets() {
        let mut jb = JitterBuffer::default();
        jb.insert(10, frame(1));
        assert_eq!(jb.pop_ready(), vec![pkt(10, 1)]);
        // 10 already delivered (next_seq is now 11); re-inserting is ignored.
        jb.insert(10, frame(9));
        jb.insert(11, frame(2));
        assert_eq!(jb.pop_ready(), vec![pkt(11, 2)]);
    }

    #[test]
    fn reports_missing_sequences() {
        let mut jb = JitterBuffer::default();
        jb.insert(10, frame(1));
        jb.insert(13, frame(4)); // 11, 12 missing
        assert_eq!(jb.missing(), vec![11, 12]);
        jb.insert(11, frame(2));
        assert_eq!(jb.missing(), vec![12]);
    }

    #[test]
    fn forced_skip_when_too_far_ahead() {
        let mut jb = JitterBuffer::new(4);
        jb.insert(10, frame(1));
        assert_eq!(jb.pop_ready(), vec![pkt(10, 1)]);
        // 11 never arrives; 12,13,14,15 do. Lead reaches max_lead=4 → skip 11.
        for s in [12, 13, 14, 15] {
            jb.insert(s, frame(s as u8));
        }
        let out = jb.pop_ready();
        assert_eq!(
            out[0],
            Delivery::Lost { seq: 11 },
            "missing 11 is concealed"
        );
        assert_eq!(
            &out[1..],
            &[pkt(12, 12), pkt(13, 13), pkt(14, 14), pkt(15, 15)]
        );
    }

    #[test]
    fn delivers_across_u16_wrap() {
        let mut jb = JitterBuffer::default();
        jb.insert(0xFFFE, frame(1));
        jb.insert(0xFFFF, frame(2));
        jb.insert(0x0000, frame(3));
        jb.insert(0x0001, frame(4));
        assert_eq!(
            jb.pop_ready(),
            vec![
                pkt(0xFFFE, 1),
                pkt(0xFFFF, 2),
                pkt(0x0000, 3),
                pkt(0x0001, 4),
            ]
        );
    }

    #[test]
    fn missing_across_wrap() {
        let mut jb = JitterBuffer::default();
        jb.insert(0xFFFF, frame(1));
        jb.insert(0x0001, frame(3)); // 0x0000 missing
        assert_eq!(jb.missing(), vec![0x0000]);
    }

    #[test]
    fn a_permanent_gap_is_skipped_so_playback_resumes() {
        // The stall a real iPhone triggers: FLUSH re-arms the buffer at a
        // sequence the sender has already sent and will not send again, so
        // the packet at the front never arrives. Everything after it must
        // still reach the player.
        let mut jb = JitterBuffer::default();
        jb.reset(Some(1000));
        for i in 1..=2000u16 {
            jb.insert(1000 + i, frame(1));
        }
        let delivered = jb.pop_ready();
        assert!(
            !delivered.is_empty(),
            "a gap the sender never fills must not stall the stream forever"
        );
    }

    #[test]
    fn reset_drops_buffered_audio() {
        let mut jb = JitterBuffer::default();
        jb.insert(10, frame(1));
        jb.insert(11, frame(2));
        jb.reset(Some(100));
        assert_eq!(jb.pop_ready(), vec![]);
        jb.insert(100, frame(7));
        assert_eq!(jb.pop_ready(), vec![pkt(100, 7)]);
    }
}
