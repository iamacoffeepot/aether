//! The conversation a turn resends: a flat, ordered list of role-tagged texts.

use aether_bloomery_kinds::{Ref, Utf8Text};

/// Who spoke an item. System-style instructions are a leading `Developer` item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Storage)]
pub enum Role {
    Developer,
    User,
    Assistant,
}

/// One item of the conversation: its speaker and the cited text it said.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Storage)]
pub struct TurnItem {
    role: Role,
    text: Ref<Utf8Text>,
}

impl TurnItem {
    /// One item citing `text` as spoken by `role`.
    #[must_use]
    pub const fn new(role: Role, text: Ref<Utf8Text>) -> Self {
        Self { role, text }
    }

    /// Who spoke the item.
    #[must_use]
    pub const fn role(&self) -> Role {
        self.role
    }

    /// The cited text of the item.
    #[must_use]
    pub const fn text(&self) -> Ref<Utf8Text> {
        self.text
    }
}

/// Why [`TurnItems::new`] or decode refused a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnItemsError {
    /// The conversation had no items.
    Empty,
    /// More than [`TurnItems::MAX_ITEMS`] items.
    TooMany,
    /// The last item was not spoken by [`Role::User`].
    LastNotUser,
}

impl TurnItemsError {
    const fn reason(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooMany => "too-many",
            Self::LastNotUser => "last-not-user",
        }
    }
}

/// The whole conversation, in order: never empty, and it ends on a user item.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
pub struct TurnItems(Vec<TurnItem>);

impl TurnItems {
    /// Most items one turn may carry.
    pub const MAX_ITEMS: usize = 4096;

    /// Accept a conversation that ends on a user item.
    ///
    /// # Errors
    ///
    /// The [`TurnItemsError`] naming the rule the list broke.
    pub fn new(items: Vec<TurnItem>) -> Result<Self, TurnItemsError> {
        Self::check(&items)?;
        Ok(Self(items))
    }

    /// Every item in conversation order.
    #[must_use]
    pub fn as_slice(&self) -> &[TurnItem] {
        &self.0
    }

    fn check(items: &[TurnItem]) -> Result<(), TurnItemsError> {
        let Some(last) = items.last() else {
            return Err(TurnItemsError::Empty);
        };
        if items.len() > Self::MAX_ITEMS {
            return Err(TurnItemsError::TooMany);
        }
        if last.role != Role::User {
            return Err(TurnItemsError::LastNotUser);
        }
        Ok(())
    }
}

invariant_errors!(TurnItemsError);

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{Ref, Utf8Text};
    use aether_data::{Storage, StorageData};

    use super::{Role, TurnItem, TurnItems};
    use crate::input::{Endpoint, ModelName, OutputBudget, ReasoningEffort, TurnInput};

    #[test]
    fn a_stored_input_that_breaks_a_rule_refuses_on_decode() {
        // Catches a dropped `#[storage(validate)]`, which would let an invalid input in through the journal.
        let with_items = |items: TurnItems| TurnInput {
            endpoint: Endpoint::new("https://example.test/v1/responses").expect("endpoint"),
            model: ModelName::new("muse-spark-1.3").expect("model"),
            items,
            max_output_tokens: OutputBudget::new(64).expect("budget"),
            reasoning: ReasoningEffort::Low,
        };
        let stored = |input: TurnInput| TurnInput::encode_storage(&StorageData::from_value(input)).expect("encode");
        let decoded = |bytes: &[u8]| TurnInput::decode_storage(bytes).map(|data| data.value);

        let user = TurnItem::new(Role::User, Ref::<Utf8Text>::of_text("hello"));
        let valid = with_items(TurnItems::new(vec![user]).expect("one user item"));
        assert_eq!(decoded(&stored(valid.clone())).ok(), Some(valid), "a valid input decodes");

        assert!(decoded(&stored(with_items(TurnItems(Vec::new())))).is_err(), "an empty list refuses");
        let assistant_last = TurnItems(vec![user, TurnItem::new(Role::Assistant, Ref::of_text("hi"))]);
        assert!(decoded(&stored(with_items(assistant_last))).is_err(), "an assistant-last list refuses");
    }
}
