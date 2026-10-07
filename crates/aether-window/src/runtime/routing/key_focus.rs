//! Key focus state (ADR-0248 §9): each window's slot, and the record of each
//! key that is down at it. Both are a [`Reach`]: everyone, or one holder.
//!
//! Every function here changes or reads the state and sends no mail; the
//! caller sends the notices a change calls for. The state has no lock: it is
//! reached only through the window manager's own state, inside one of its
//! handlers or host turns.
//!
//! The table is keyed by the erased window path, because the events that
//! read it (`Key`, `WindowFocus`, `WindowClosed`, an injected event) name
//! their window as one. A take and a release name theirs as an
//! `ActorPath<WindowInstance>`, which is erased here, at the lookup, and a
//! held slot keeps the typed path so the notice a close sends carries it.

use std::collections::HashMap;
use std::mem;

use aether_actor::{ActorPath, ErasedActorRef, ProtocolRef};
use aether_data::ErasedActorPath;

use crate::{KeyFocusHolder, KeyFocusScope, WindowInstance};

/// Which of a window's key subscribers its key focus slot admits. It is the
/// whole state of a slot, and each key that is down keeps a copy of the one it
/// was pressed under.
#[derive(Clone, Default)]
pub(in crate::runtime) enum Reach {
    /// Nobody holds the slot: every key subscriber is admitted.
    #[default]
    Everyone,
    /// One actor holds the slot: it is admitted, and under a subtree scope so
    /// is every actor beneath it in lineage.
    ///
    /// `path` is the holder's canonical path, an owned copy, so the record of
    /// a key that is down still says who it admits after its holder closes.
    /// `holder` is the proof the holder's notices are sent through while the
    /// slot is held; a key record's copy of it is never sent through.
    /// `window` is the window the slot belongs to, the typed path its take
    /// named, which the notice a close sends carries; a key record's copy of
    /// it is never read.
    Held {
        window: ActorPath<WindowInstance>,
        holder: ProtocolRef<KeyFocusHolder>,
        path: ErasedActorPath,
        scope: KeyFocusScope,
    },
}

impl Reach {
    /// Whether the actor at `subscriber` is inside this reach.
    pub(in crate::runtime) fn admits(&self, subscriber: &ErasedActorPath) -> bool {
        match self {
            Self::Everyone => true,
            Self::Held { path, scope, .. } => {
                let is_holder = subscriber == path;
                let reaches_subtree = *scope == KeyFocusScope::Subtree;
                let beneath_holder = reaches_subtree && beneath(subscriber, path);

                is_holder || beneath_holder
            }
        }
    }

    /// Whether `actor` holds the slot.
    fn held_by(&self, actor: ErasedActorRef) -> bool {
        match self {
            Self::Everyone => false,
            Self::Held { holder, .. } => holder.erase() == actor,
        }
    }

    /// The actor holding the slot: `None` when nobody holds it.
    fn holder(&self) -> Option<ProtocolRef<KeyFocusHolder>> {
        match self {
            Self::Everyone => None,
            Self::Held { holder, .. } => Some(*holder),
        }
    }

    /// The hold the slot is under: `None` when nobody holds it.
    fn into_hold(self) -> Option<Hold> {
        match self {
            Self::Everyone => None,
            Self::Held { window, holder, .. } => Some(Hold { window, holder }),
        }
    }
}

/// The hold a closing window's slot was under: its holder, and the window as
/// the holder's take named it.
pub(in crate::runtime) struct Hold {
    pub(in crate::runtime) window: ActorPath<WindowInstance>,
    pub(in crate::runtime) holder: ProtocolRef<KeyFocusHolder>,
}

/// Whether `path` names an actor beneath `ancestor`: `ancestor`'s path, then
/// a step separator, then more. The separator is what keeps `pan` from being
/// read as an ancestor of `panel`.
fn beneath(path: &ErasedActorPath, ancestor: &ErasedActorPath) -> bool {
    path.as_str().strip_prefix(ancestor.as_str()).is_some_and(|rest| rest.starts_with('/'))
}

/// One window's key focus slot and the keys down at it. Each key that is down
/// records the reach it was pressed under, and its repeats and its release
/// are routed by that record.
#[derive(Default)]
struct WindowKeys {
    slot: Reach,
    down: HashMap<u32, Reach>,
}

impl WindowKeys {
    fn is_idle(&self) -> bool {
        let unheld = matches!(self.slot, Reach::Everyone);

        unheld && self.down.is_empty()
    }

    /// Record `code` going down under the slot as it is now. A code already
    /// down keeps the record its press made.
    fn press(&mut self, code: u32) {
        self.down.entry(code).or_insert_with(|| self.slot.clone());
    }

    /// The reach a key event for `code` is routed by: the record its press
    /// made, or the slot as it is now when no press is recorded.
    fn routed_by(&self, code: u32) -> &Reach {
        self.down.get(&code).unwrap_or(&self.slot)
    }
}

/// What a take changed.
pub(in crate::runtime) enum Take {
    /// The window's holder took again: its scope is the new one and nobody is
    /// told.
    Kept,
    /// The sender is the window's new holder. `replaced` is the holder it
    /// displaced: `None` when nobody held the slot.
    Gained { replaced: Option<ProtocolRef<KeyFocusHolder>> },
}

/// Every window's key focus slot and key records, keyed by the erased window
/// path. A window with no entry has a slot nobody holds and no key down.
#[derive(Default)]
pub(in crate::runtime) struct KeyFocus {
    windows: HashMap<ErasedActorPath, WindowKeys>,
}

impl KeyFocus {
    /// Give `window`'s slot to `holder`, the actor at `path`, with `scope`.
    /// The window need not be open: its type says the path names a window,
    /// and nothing about whether one stands there.
    pub(in crate::runtime) fn take(
        &mut self,
        window: &ActorPath<WindowInstance>,
        holder: ProtocolRef<KeyFocusHolder>,
        path: ErasedActorPath,
        scope: KeyFocusScope,
    ) -> Take {
        let keys = self.windows.entry(window.as_erased().clone()).or_default();
        let held = Reach::Held { window: window.clone(), holder, path, scope };
        let previous = mem::replace(&mut keys.slot, held);

        if previous.held_by(holder.erase()) {
            return Take::Kept;
        }
        Take::Gained { replaced: previous.holder() }
    }

    /// Empty `window`'s slot when `sender` holds it, answering whether it
    /// did. The window's key records are left as they are.
    pub(in crate::runtime) fn release(&mut self, window: &ActorPath<WindowInstance>, sender: ErasedActorRef) -> bool {
        let window = window.as_erased();
        let Some(keys) = self.windows.get_mut(window) else {
            return false;
        };
        if !keys.slot.held_by(sender) {
            return false;
        }
        keys.slot = Reach::Everyone;
        if keys.is_idle() {
            self.windows.remove(window);
        }
        true
    }

    /// Remove `window`'s slot and key records, answering the hold it was
    /// under: `None` when nobody held it.
    pub(in crate::runtime) fn close(&mut self, window: &ErasedActorPath) -> Option<Hold> {
        self.windows.remove(window).and_then(|keys| keys.slot.into_hold())
    }

    /// Empty every slot `departed` holds. Key records are left as they are:
    /// a key pressed under its hold still routes its repeats and its release
    /// by that record.
    pub(in crate::runtime) fn forget(&mut self, departed: ErasedActorRef) {
        for keys in self.windows.values_mut() {
            if keys.slot.held_by(departed) {
                keys.slot = Reach::Everyone;
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
        let down = HashMap::from([(code, Reach::Everyone)]);

        self.windows.insert(window.clone(), WindowKeys { slot: Reach::Everyone, down });
    }

    /// The reach a key event for `code` at `window` is routed by: the record
    /// its press made, or the slot as it is now when no press is recorded.
    pub(in crate::runtime) fn pressed_under(&self, window: &ErasedActorPath, code: u32) -> &Reach {
        self.windows.get(window).map_or(&Reach::Everyone, |keys| keys.routed_by(code))
    }

    /// Take the record of `code` going up at `window`, answering the reach it
    /// was pressed under, or the slot as it is now when no press was
    /// recorded.
    pub(in crate::runtime) fn lift(&mut self, window: &ErasedActorPath, code: u32) -> Reach {
        let Some(keys) = self.windows.get_mut(window) else {
            return Reach::Everyone;
        };

        keys.down.remove(&code).unwrap_or_else(|| keys.slot.clone())
    }

    /// The reach of `window`'s slot as it is now.
    pub(in crate::runtime) fn reach(&self, window: &ErasedActorPath) -> &Reach {
        self.windows.get(window).map_or(&Reach::Everyone, |keys| &keys.slot)
    }

    /// Forget every key down at `window`, keeping its slot: the window lost
    /// operating-system focus, so those releases never arrive.
    pub(in crate::runtime) fn forget_keys(&mut self, window: &ErasedActorPath) {
        if let Some(keys) = self.windows.get_mut(window) {
            keys.down.clear();
        }
    }
}
