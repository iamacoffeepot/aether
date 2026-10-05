//! The items a called turn adds to its conversation once every call has its
//! output.

use aether_data::{Ref, Utf8Text};

use crate::input::{Reasoning, Role, ToolCall, TurnItem};
use crate::session::record::CallAnswer;

/// What a turn that asked for `calls` adds to the conversation: its
/// `reasoning` items in reply order, then its `text` as an assistant message
/// when the text is not empty, then every call, then every output in
/// `outputs`' order.
///
/// The next turn and a turn-limit rest both append exactly these items, so a
/// session resting at its limit holds what its next turn would have sent.
pub fn replay(
    reasoning: &[Reasoning],
    text: Ref<Utf8Text>,
    calls: &[ToolCall],
    outputs: &[CallAnswer],
) -> Vec<TurnItem> {
    let said = (text != Ref::of_text("")).then(|| TurnItem::message(Role::Assistant, text));
    reasoning
        .iter()
        .cloned()
        .map(TurnItem::Reasoning)
        .chain(said)
        .chain(calls.iter().cloned().map(TurnItem::Call))
        .chain(outputs.iter().map(CallAnswer::item))
        .collect()
}
