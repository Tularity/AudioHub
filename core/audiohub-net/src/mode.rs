//! The machine-wide operating mode (plan §13, frozen 2026-08-03).
//!
//! Three modes, **mutually exclusive**, and the exclusion is the whole point:
//!
//! | mode | others may use this machine | this machine may use others |
//! |---|---|---|
//! | [`Mode::Share`] | ✅ | ❌ |
//! | [`Mode::A`]     | ❌ | ✅ (driverless system capture) |
//! | [`Mode::B`]     | ❌ | ✅ (one virtual device pair per peer) |
//!
//! ## Why the exclusion exists (plan §13 "失效形态")
//!
//! Before §13 a machine was a provider AND a consumer at once. Let X share its
//! *default* microphone with Y while X is in mode B with "AudioHub – Z 麦克风"
//! selected as its default input: Y then receives **Z's** microphone, and X is
//! a relay it never agreed to be. Add "Z is using X" and the graph closes into
//! a cycle whose latency grows without bound until some stage saturates — the
//! same failure class this project spent §7.6 curing, reached through topology
//! instead of through jitter.
//!
//! The exclusion is enforced **locally, on the machine being asked** (see
//! `audiohubd`'s `refuse_being_used` / `refuse_using_others`). It never depends
//! on a peer behaving: a peer's advertised mode is a courtesy that lets the UI
//! grey an entry out *before* the user tries, not a security boundary.
//!
//! ## Why this type lives in `audiohub-net` and not `audiohub-ipc`
//!
//! It has to be nameable on the wire (`SessionMsg::ModeState`) and in the local
//! IPC contract. `audiohub-ipc` already depends on this crate, so a type here
//! can be re-exported upward; the reverse dependency does not exist and must
//! not be created for one enum.

use serde::{Deserialize, Serialize};

/// Wire/JSON spellings. These are the strings the frontend compares against, so
/// they are part of the frozen contract — `Mode` is serialised as exactly one
/// of them, and nothing constructs the strings by hand anywhere else.
pub const MODE_SHARE: &str = "share";
pub const MODE_A: &str = "a";
pub const MODE_B: &str = "b";

/// The machine-wide mode. See the module docs for the exclusion table.
///
/// A real enum rather than the `String` this used to be: every place that
/// branches on the mode now fails to compile when a variant is added, which is
/// how the third mode is kept from being silently handled as "not B". The
/// previous shape (`consumer_mode: String`, compared with `== MODE_B`) folds
/// `Share` and `A` into one branch by construction, and every one of those
/// branches is a place where a Share-mode machine would have behaved like a
/// mode-A consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// The only mode in which other machines may take this machine's microphone
    /// or play into its default output. Needs no driver and no capture
    /// permission, which is why it is the default (see `StoredSettings`).
    Share,
    /// Driverless consumer: this machine's system audio is captured and sent to
    /// a peer's output; taking a peer's microphone needs a third-party virtual
    /// sound card (plan §7.1).
    A,
    /// Driver consumer: every paired peer becomes a pair of real system audio
    /// devices here, and the system's device selection *is* the peer selection.
    B,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Share => MODE_SHARE,
            Mode::A => MODE_A,
            Mode::B => MODE_B,
        }
    }

    /// `None` for anything this build does not define. Callers decide what an
    /// unknown mode means; nothing here guesses one, because the two plausible
    /// guesses ("share" and "a") sit on opposite sides of the exclusion.
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            MODE_SHARE => Some(Mode::Share),
            MODE_A => Some(Mode::A),
            MODE_B => Some(Mode::B),
            _ => None,
        }
    }

    /// May other machines take audio from / play audio into this one?
    ///
    /// Exactly one mode says yes. Written as a `match` with no `_` arm so a
    /// fourth mode cannot be added without answering this question for it.
    pub fn serves_peers(self) -> bool {
        match self {
            Mode::Share => true,
            Mode::A | Mode::B => false,
        }
    }

    /// May this machine take audio from / play audio into other machines?
    pub fn consumes_peers(self) -> bool {
        match self {
            Mode::Share => false,
            Mode::A | Mode::B => true,
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exclusion table of plan §13, asserted as a table rather than as three
    /// separate facts: the property that matters is that **no mode does both**,
    /// and that is a statement about the whole set.
    #[test]
    fn no_mode_both_serves_and_consumes() {
        for m in [Mode::Share, Mode::A, Mode::B] {
            assert!(
                !(m.serves_peers() && m.consumes_peers()),
                "{m} both serves and consumes — that is the relay/loop hazard plan §13 exists to \
                 remove"
            );
            assert!(
                m.serves_peers() || m.consumes_peers(),
                "{m} does neither, which would make it an unreachable mode"
            );
        }
    }

    /// Exactly one mode may be used by others. If a second one ever can, the
    /// "which machine is the provider" question stops having one answer.
    #[test]
    fn exactly_one_mode_serves_peers() {
        let serving: Vec<Mode> = [Mode::Share, Mode::A, Mode::B]
            .into_iter()
            .filter(|m| m.serves_peers())
            .collect();
        assert_eq!(serving, vec![Mode::Share]);
    }

    /// The JSON spellings are a contract with the frontend (`state/mode.ts`)
    /// and with `settings.json` on disk. Renaming a variant must not silently
    /// rename the string.
    #[test]
    fn the_wire_spellings_are_frozen() {
        assert_eq!(serde_json::to_string(&Mode::Share).unwrap(), r#""share""#);
        assert_eq!(serde_json::to_string(&Mode::A).unwrap(), r#""a""#);
        assert_eq!(serde_json::to_string(&Mode::B).unwrap(), r#""b""#);
        for m in [Mode::Share, Mode::A, Mode::B] {
            assert_eq!(Mode::parse(m.as_str()), Some(m));
            assert_eq!(
                serde_json::from_str::<Mode>(&format!("\"{}\"", m.as_str())).unwrap(),
                m
            );
        }
    }

    /// An unknown mode is `None`, never a default. The two candidate defaults
    /// sit on opposite sides of the exclusion, so guessing here would decide
    /// "can this machine be used" by accident.
    #[test]
    fn an_unknown_mode_is_not_guessed() {
        assert_eq!(Mode::parse("c"), None);
        assert_eq!(Mode::parse(""), None);
        assert_eq!(Mode::parse("SHARE"), None, "spellings are exact");
        assert!(serde_json::from_str::<Mode>(r#""provider""#).is_err());
    }
}
