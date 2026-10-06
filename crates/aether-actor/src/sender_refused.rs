//! What a dispatch arm reports when its handler's sender requirement refuses
//! the mail's sender (ADR-0231 §11).
//!
//! A handler that takes `sender: ProtocolRef<P>` runs only for a sender the
//! engine casts to `P` first. When the cast answers nothing, the arm on
//! either transport builds one [`SenderRefused`] from what it can read about
//! the sender, logs it, and answers with it: a request through its reply's
//! `From<PathRefused>`, a tell through the decode-refusal notice an opted-in
//! reply target hears. Both transports build it here, so their log lines and
//! answers read the same.

use core::fmt;

use aether_data::{ErasedActorPath, KindId, ReplyContract};

use crate::model::{CastTarget, RowSet};
use crate::{PathRefusal, PathRefused};

/// Why a sender did not pass a handler's sender requirement.
///
/// Not part of the public API: the `#[actor]` dispatch arms reach it through
/// the hidden ctx helpers, and a handler never sees one.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SenderRefused {
    /// The mail carries no actor sender, or its stamped position holds no
    /// route: a harness push, session mail, or a broadcast.
    NoSender,
    /// The sender's route is not `Live`, so it publishes no rows to check.
    NotLive {
        /// The sender.
        sender: ErasedActorPath,
    },
    /// The sender's published rows lack a row of the required protocol.
    Uncovered {
        /// The sender.
        sender: ErasedActorPath,
        /// The first row of the protocol the sender does not publish.
        kind: KindId,
        /// That kind's name.
        name: &'static str,
    },
}

impl SenderRefused {
    /// The refusal of a sender the cast to `P` did not admit: `sender` is its
    /// path, absent for mail with no sender, and `rows` what its route
    /// published while `Live`.
    ///
    /// The row it names is the first of `P`'s rows the sender does not
    /// publish with the row's exact reply. [`Subscriber<K>`](crate::Subscriber)
    /// also admits an unchecked row, so a sender it refused publishes neither
    /// and its one row is the one named.
    #[must_use]
    pub fn of<P: CastTarget>(sender: Option<ErasedActorPath>, rows: Option<&[(KindId, ReplyContract)]>) -> Self {
        let Some(sender) = sender else {
            return Self::NoSender;
        };
        let Some(rows) = rows else {
            return Self::NotLive { sender };
        };

        let required = <P::Rows as RowSet>::CONTRACTS.iter().zip(<P::Rows as RowSet>::KIND_NAMES);
        let lacked = required.clone().find(|(row, _)| !rows.contains(row)).or_else(|| required.clone().next());
        match lacked {
            Some(((kind, _), name)) => Self::Uncovered { sender, kind: *kind, name },
            None => Self::NotLive { sender },
        }
    }

    /// The typed-path refusal a request's reply is built from, naming the
    /// sender's path, or `None` for mail with no sender, which has no path to
    /// name and no one to answer.
    #[must_use]
    pub fn path_refused(&self) -> Option<PathRefused> {
        match self {
            Self::NoSender => None,
            Self::NotLive { sender } => Some(PathRefused { path: sender.clone(), reason: PathRefusal::NotLive }),
            Self::Uncovered { sender, kind, .. } => {
                Some(PathRefused { path: sender.clone(), reason: PathRefusal::Uncovered { kind: *kind } })
            }
        }
    }
}

impl fmt::Display for SenderRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSender => f.write_str("the mail has no actor sender, and the handler requires one"),
            Self::NotLive { sender } => write!(f, "the sender `{sender}` is not live"),
            Self::Uncovered { sender, name, .. } => {
                write!(f, "the sender `{sender}` has no handler for `{name}`, which the handler requires of its sender")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use aether_data::{ErasedActorPath, Kind, ReplyContract};
    use aether_kinds::{Ping, Pong, Tick};

    use super::SenderRefused;
    use crate::model::ProtocolCast;
    use crate::{PathRefusal, Protocol, Row, Silent, Subscriber};

    struct PingPong;

    impl Protocol for PingPong {
        type Rows = (Row<Ping, Pong>, Row<Pong, Silent>);
    }

    impl ProtocolCast for PingPong {}

    fn sender() -> ErasedActorPath {
        ErasedActorPath::new("test.sender").expect("a valid path")
    }

    // A refusal that named the first row of the protocol, or a row the sender
    // does publish, would send its author to add a handler they already have.
    #[test]
    fn the_refusal_names_the_first_row_the_sender_lacks() {
        let request = (Ping::ID, ReplyContract::One(Pong::ID));
        let refused = SenderRefused::of::<PingPong>(Some(sender()), Some(&[(Tick::ID, ReplyContract::None), request]));

        assert_eq!(refused, SenderRefused::Uncovered { sender: sender(), kind: Pong::ID, name: Pong::NAME });
        assert_eq!(
            refused.path_refused().map(|refused| refused.reason),
            Some(PathRefusal::Uncovered { kind: Pong::ID })
        );
    }

    // A row answered with another reply is a lacked row: the cast refused it,
    // so the refusal must name it and not the row after it.
    #[test]
    fn a_row_with_another_reply_is_the_row_named() {
        let rows = [(Ping::ID, ReplyContract::None), (Pong::ID, ReplyContract::None)];

        let refused = SenderRefused::of::<PingPong>(Some(sender()), Some(&rows));

        assert_eq!(refused, SenderRefused::Uncovered { sender: sender(), kind: Ping::ID, name: Ping::NAME });
    }

    // Mail with no sender has no path, so a request arm must find nothing to
    // answer and fall back to the notice; a refusal that made up a path would
    // answer a caller that does not exist.
    #[test]
    fn mail_with_no_sender_and_a_closed_sender_name_no_row() {
        assert_eq!(SenderRefused::of::<PingPong>(None, None).path_refused(), None);

        let closed = SenderRefused::of::<Subscriber<Tick>>(Some(sender()), None);

        assert_eq!(closed.path_refused().map(|refused| refused.reason), Some(PathRefusal::NotLive));
    }
}
