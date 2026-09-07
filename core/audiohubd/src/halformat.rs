//! Per-speaker format transactions for the macOS HAL v4 bridge.

use audiohub_core::spatial_output::SpeakerLayout;
use std::time::{Duration, Instant};

pub(crate) const MESSAGE_ID: i32 = 0x4148_0006;
pub(crate) const OFFER: u32 = 1;
pub(crate) const QUIESCED: u32 = 2;
pub(crate) const ACCEPTED: u32 = 3;
pub(crate) const PREPARE: u32 = 4;
pub(crate) const COMMITTED: u32 = 5;
pub(crate) const READY: u32 = 6;
pub(crate) const ABORTED: u32 = 7;
pub(crate) const STEREO_MASK: u32 = 1;

pub(crate) fn channels(layout: u32) -> Option<u8> {
    match layout {
        0 => Some(2),
        1 => Some(6),
        2 => Some(8),
        3 => Some(12),
        _ => None,
    }
}

pub(crate) fn spatial_layout(layout: u32) -> Option<SpeakerLayout> {
    match layout {
        1 => Some(SpeakerLayout::Surround51),
        2 => Some(SpeakerLayout::Surround71),
        3 => Some(SpeakerLayout::Immersive714),
        _ => None,
    }
}

pub(crate) fn layout_id(layout: SpeakerLayout) -> u32 {
    match layout {
        SpeakerLayout::Surround51 => 1,
        SpeakerLayout::Surround71 => 2,
        SpeakerLayout::Immersive714 => 3,
    }
}

pub(crate) fn valid_mask(mask: u32) -> bool {
    mask & 1 != 0 && mask & !15 == 0
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Payload {
    pub op: u32,
    pub endpoint: u32,
    pub generation: u32,
    pub layout: u32,
    pub supported_mask: u32,
    pub epoch: u32,
    pub session_id: u64,
    pub request_id: u64,
}

impl Payload {
    pub fn valid(&self) -> bool {
        (OFFER..=ABORTED).contains(&self.op)
            && self.endpoint < 32
            && self.endpoint % 2 == 0
            && self.generation != 0
            && self.session_id != 0
            && channels(self.layout).is_some()
            && valid_mask(self.supported_mask)
            && (self.op == ABORTED || self.supported_mask & (1 << self.layout) != 0)
    }
    fn same_transaction(&self, other: &Self) -> bool {
        self.endpoint == other.endpoint
            && self.generation == other.generation
            && self.epoch == other.epoch
            && self.session_id == other.session_id
            && self.request_id == other.request_id
    }
    fn same_format(&self, other: &Self) -> bool {
        self.same_transaction(other)
            && self.layout == other.layout
            && self.supported_mask == other.supported_mask
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Unknown,
    Offered,
    Preparing,
    Quiesced,
    Committed,
    Accepting,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Effect {
    Ignore,
    Quiesce(Payload),
    Accept(Payload),
    Publish(Payload),
    Failed,
}

pub(crate) struct State {
    pub phase: Phase,
    pub current: Option<Payload>,
    pub pending: Option<Payload>,
    allowed: u32,
    next_request: u64,
    last_offer: Option<(u32, u32)>,
    high_epoch: u32,
    failures: u8,
    progress_at: Option<Instant>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            phase: Phase::Unknown,
            current: None,
            pending: None,
            allowed: 1,
            next_request: 1,
            last_offer: None,
            high_epoch: 0,
            failures: 0,
            progress_at: None,
        }
    }
}

impl State {
    pub fn offer(
        &mut self,
        endpoint: u32,
        generation: u32,
        session_id: u64,
        mask: u32,
        layout: u32,
    ) -> Option<Payload> {
        if !matches!(self.phase, Phase::Unknown | Phase::Ready | Phase::Failed)
            && self
                .progress_at
                .is_some_and(|at| at.elapsed() >= Duration::from_secs(5))
        {
            self.send_failed();
        }
        if !valid_mask(mask) || channels(layout).is_none() || mask & (1 << layout) == 0 {
            return None;
        }
        if !matches!(self.phase, Phase::Unknown | Phase::Ready | Phase::Failed) {
            return None;
        }
        if self.phase == Phase::Failed
            && self.failures >= 3
            && self.last_offer == Some((mask, layout))
        {
            return None;
        }
        if self.phase == Phase::Ready && self.last_offer == Some((mask, layout)) {
            return None;
        }
        let request_id = self.next_request;
        self.next_request = self.next_request.checked_add(1)?;
        let offer = Payload {
            op: OFFER,
            endpoint,
            generation,
            session_id,
            supported_mask: mask,
            layout,
            request_id,
            epoch: 0,
        };
        if !offer.valid() {
            return None;
        }
        self.allowed = mask;
        if self.last_offer != Some((mask, layout)) {
            self.failures = 0;
        }
        self.last_offer = Some((mask, layout));
        self.phase = Phase::Offered;
        self.progress_at = Some(Instant::now());
        Some(offer)
    }

    pub fn receive(&mut self, message: Payload) -> Effect {
        if !message.valid() || message.epoch == 0 {
            return Effect::Ignore;
        }
        match message.op {
            PREPARE
                if message.epoch > self.high_epoch && self.allowed & (1 << message.layout) != 0 =>
            {
                self.high_epoch = message.epoch;
                self.pending = Some(message);
                self.phase = Phase::Preparing;
                self.progress_at = Some(Instant::now());
                Effect::Quiesce(message)
            }
            COMMITTED
                if matches!(
                    self.phase,
                    Phase::Quiesced | Phase::Committed | Phase::Accepting
                ) && self
                    .pending
                    .is_some_and(|pending| pending.same_format(&message)) =>
            {
                self.current = Some(message);
                self.phase = Phase::Committed;
                self.progress_at = Some(Instant::now());
                Effect::Accept(message)
            }
            READY
                if self.phase == Phase::Accepting
                    && self
                        .pending
                        .is_some_and(|pending| pending.same_format(&message)) =>
            {
                self.current = Some(message);
                self.pending = None;
                self.phase = Phase::Ready;
                self.failures = 0;
                self.last_offer = Some((message.supported_mask, message.layout));
                Effect::Publish(message)
            }
            ABORTED
                if self
                    .pending
                    .is_some_and(|pending| pending.same_transaction(&message)) =>
            {
                self.phase = Phase::Failed;
                self.failures = self.failures.saturating_add(1);
                self.pending = None;
                Effect::Failed
            }
            _ => Effect::Ignore,
        }
    }

    pub fn acknowledgement(&mut self, message: Payload, op: u32) -> Option<Payload> {
        let phase = match op {
            QUIESCED => Phase::Preparing,
            ACCEPTED => Phase::Committed,
            _ => return None,
        };
        let retry = match op {
            QUIESCED => Phase::Quiesced,
            _ => Phase::Accepting,
        };
        if !matches!(self.phase, p if p == phase || p == retry)
            || !self
                .pending
                .is_some_and(|pending| pending.same_format(&message))
        {
            return None;
        }
        self.phase = retry;
        self.progress_at = Some(Instant::now());
        Some(Payload { op, ..message })
    }

    pub fn send_failed(&mut self) {
        self.phase = Phase::Failed;
        self.pending = None;
        self.failures = self.failures.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hal_format_wire_payload_matches_the_fixed_64_byte_message_tail() {
        assert_eq!(std::mem::size_of::<Payload>(), 40);
        assert_eq!(std::mem::offset_of!(Payload, session_id), 24);
        assert_eq!(std::mem::offset_of!(Payload, request_id), 32);
    }
    #[test]
    fn hal_format_requires_quiesce_commit_and_accept_before_publication() {
        let mut state = State::default();
        let offer = state.offer(0, 5, 9, 15, 3).unwrap();
        let prepare = Payload {
            op: PREPARE,
            epoch: 1,
            ..offer
        };
        assert_eq!(state.receive(prepare), Effect::Quiesce(prepare));
        assert_eq!(
            state.receive(Payload {
                op: READY,
                ..prepare
            }),
            Effect::Ignore
        );
        let quiesced = state.acknowledgement(prepare, QUIESCED).unwrap();
        let committed = Payload {
            op: COMMITTED,
            ..quiesced
        };
        assert_eq!(state.receive(committed), Effect::Accept(committed));
        assert_eq!(
            state.receive(Payload {
                op: READY,
                ..committed
            }),
            Effect::Ignore
        );
        let accepted = state.acknowledgement(committed, ACCEPTED).unwrap();
        let ready = Payload {
            op: READY,
            ..accepted
        };
        assert_eq!(state.receive(ready), Effect::Publish(ready));
        assert_eq!(state.receive(prepare), Effect::Ignore);
    }
    #[test]
    fn hal_format_rejects_stale_epochs_wrong_transactions_and_microphones() {
        let mut state = State::default();
        let offer = state.offer(2, 7, 11, 3, 1).unwrap();
        let pending = Payload {
            op: PREPARE,
            epoch: 2,
            ..offer
        };
        assert!(matches!(state.receive(pending), Effect::Quiesce(_)));
        assert_eq!(
            state.receive(Payload {
                op: PREPARE,
                epoch: 1,
                ..pending
            }),
            Effect::Ignore
        );
        state.acknowledgement(pending, QUIESCED).unwrap();
        assert_eq!(
            state.receive(Payload {
                op: COMMITTED,
                request_id: 999,
                ..pending
            }),
            Effect::Ignore
        );
        assert!(!Payload {
            endpoint: 3,
            ..pending
        }
        .valid());
        assert_eq!(
            state.receive(Payload {
                op: ABORTED,
                ..pending
            }),
            Effect::Failed
        );
        assert!(state.offer(2, 7, 11, 1, 0).is_some());
    }

    #[test]
    fn hal_format_failed_offers_are_bounded_and_never_send_a_driver_epoch() {
        let mut state = State::default();
        for _ in 0..3 {
            assert_eq!(state.offer(0, 5, 9, 15, 0).unwrap().epoch, 0);
            state.send_failed();
        }
        assert!(state.offer(0, 5, 9, 15, 0).is_none());
        assert!(state.offer(0, 5, 9, 1, 0).is_some());
    }
}
