//! Key focus state (ADR-0248 §9): each window's slot, and the record of each
//! key that is down at it.
//!
//! Every function here changes or reads the state and sends no mail; the
//! caller sends the notices a change calls for. The state has no lock: it is
//! reached only through the window manager's own state, inside one of its
//! handlers or host turns.

use std::collections::HashMap;

use aether_actor::{ErasedActorRef, ProtocolRef};
use aether_data::ErasedActorPath;

use crate::{KeyFocusHolder, KeyFocusScope};

/// The key subscribers a held slot admits: its holder, named by canonical
/// path, and how far beneath it the hold reaches.
///
/// It owns its copy of the path, so the record of a key that is down stays
/// good after its holder closes.
#[derive(Clone)]
pub(in crate::runtime) struct Reach {
    holder: ErasedActorPath,
    scope: KeyFocusScope,
}

impl Reach {
    /// Whether the actor at `subscriber` is inside this reach: the holder
    /// itself, or under a subtree scope any actor beneath it in lineage.
    pub(in crate::runtime) fn admits(&self, subscriber: &ErasedActorPath) -> bool {
        let is_holder = subscriber == &self.holder;
        let reaches_subtree = self.scope == KeyFocusScope::Subtree;
        let beneath_holder = reaches_subtree && beneath(subscriber, &self.holder);

        is_holder || beneath_holder
    }
}

/// Whether `path` names an actor beneath `ancestor`: `ancestor`'s path, then
/// a step separator, then more. The separator is what keeps `pan` from being
/// read as an ancestor of `panel`.
fn beneath(path: &ErasedActorPath, ancestor: &ErasedActorPath) -> bool {
    path.as_str().strip_prefix(ancestor.as_str()).is_some_and(|rest| rest.starts_with('/'))
}

/// A held slot: the proof its holder's notices are sent through, and what it
/// admits.
struct Slot {
    holder: ProtocolRef<KeyFocusHolder>,
    reach: Reach,
}

/// One window's key focus slot and the keys down at it. Each key that is down
/// records the reach it was pressed under, `None` for a press under an empty
/// slot, and its repeats and its release are routed by that record.
#[derive(Default)]
struct WindowKeys {
    slot: Option<Slot>,
    down: HashMap<u32, Option<Reach>>,
}

impl WindowKeys {
    fn reach(&self) -> Option<&Reach> {
        self.slot.as_ref().map(|slot| &slot.reach)
    }

    fn held_by(&self, actor: ErasedActorRef) -> bool {
        self.slot.as_ref().is_some_and(|slot| slot.holder.erase() == actor)
    }

    fn is_idle(&self) -> bool {
        self.slot.is_none() && self.down.is_empty()
    }

    /// Record `code` going down under the reach as it is now. A code already
    /// down keeps the record its press made.
    fn press(&mut self, code: u32) {
        self.down.entry(code).or_insert_with(|| self.slot.as_ref().map(|slot| slot.reach.clone()));
    }
}

/// What a take changed.
pub(in crate::runtime) enum Take {
    /// The window's holder took again: its scope is the new one and nobody is
    /// told.
    Kept,
    /// The sender is the window's new holder, in place of `replaced` when the
    /// slot was held.
    Gained { replaced: Option<ProtocolRef<KeyFocusHolder>> },
}

/// Every window's key focus slot and key records, keyed by window path.
#[derive(Default)]
pub(in crate::runtime) struct KeyFocus {
    windows: HashMap<ErasedActorPath, WindowKeys>,
}

impl KeyFocus {
    /// Give `window`'s slot to `holder`, the actor at `path`, with `scope`.
    /// The window need not be open.
    pub(in crate::runtime) fn take(
        &mut self,
        window: &ErasedActorPath,
        holder: ProtocolRef<KeyFocusHolder>,
        path: ErasedActorPath,
        scope: KeyFocusScope,
    ) -> Take {
        let slot = Slot { holder, reach: Reach { holder: path, scope } };
        let previous = self.windows.entry(window.clone()).or_default().slot.replace(slot).map(|slot| slot.holder);

        match previous {
            Some(previous) if previous.erase() == holder.erase() => Take::Kept,
            replaced => Take::Gained { replaced },
        }
    }

    /// Empty `window`'s slot when `sender` holds it, answering whether it
    /// did. The window's key records are left as they are.
    pub(in crate::runtime) fn release(&mut self, window: &ErasedActorPath, sender: ErasedActorRef) -> bool {
        let Some(keys) = self.windows.get_mut(window) else {
            return false;
        };
        if !keys.held_by(sender) {
            return false;
        }
        keys.slot = None;
        if keys.is_idle() {
            self.windows.remove(window);
        }
        true
    }

    /// Remove `window`'s slot and key records, answering the holder it had.
    pub(in crate::runtime) fn close(&mut self, window: &ErasedActorPath) -> Option<ProtocolRef<KeyFocusHolder>> {
        self.windows.remove(window).and_then(|keys| keys.slot).map(|slot| slot.holder)
    }

    /// Empty every slot `departed` holds. Key records are left as they are:
    /// a key pressed under its hold still routes its repeats and its release
    /// by that record.
    pub(in crate::runtime) fn forget(&mut self, departed: ErasedActorRef) {
        for keys in self.windows.values_mut() {
            if keys.held_by(departed) {
                keys.slot = None;
            }
        }
        self.windows.retain(|_, keys| !keys.is_idle());
    }

    /// Record `code` going down at `window` under the slot as it is now,
    /// unless it is already down.
    pub(in crate::runtime) fn press(&mut self, window: &ErasedActorPath, code: u32) {
        if let Some(keys) = self.windows.get_mut(window) {
            keys.press(code);
            return;
        }
        self.windows.insert(window.clone(), WindowKeys { slot: None, down: HashMap::from([(code, None)]) });
    }

    /// The reach recorded for `code` while it is down at `window`: `None`
    /// when it was pressed under an empty slot.
    pub(in crate::runtime) fn pressed_under(&self, window: &ErasedActorPath, code: u32) -> Option<&Reach> {
        self.windows.get(window).and_then(|keys| keys.down.get(&code)).and_then(Option::as_ref)
    }

    /// Take the record of `code` going up at `window`, answering the reach it
    /// was pressed under, or the slot's reach as it is now when no press was
    /// recorded.
    pub(in crate::runtime) fn lift(&mut self, window: &ErasedActorPath, code: u32) -> Option<Reach> {
        let keys = self.windows.get_mut(window)?;

        keys.down.remove(&code).unwrap_or_else(|| keys.reach().cloned())
    }

    /// The reach of `window`'s slot as it is now: `None` when it is empty.
    pub(in crate::runtime) fn reach(&self, window: &ErasedActorPath) -> Option<&Reach> {
        self.windows.get(window).and_then(WindowKeys::reach)
    }

    /// Forget every key down at `window`, keeping its slot: the window lost
    /// operating-system focus, so those releases never arrive.
    pub(in crate::runtime) fn forget_keys(&mut self, window: &ErasedActorPath) {
        if let Some(keys) = self.windows.get_mut(window) {
            keys.down.clear();
        }
    }
}
