//! The lanes a widget host sends its children through (ADR-0231 §3).
//!
//! A host keeps children of sixteen types in one table, so it cannot hold each
//! one as the typed [`InlineChild<C>`] its spawn returned. It holds instead one
//! [`ProtocolRef`] per *lane* the child covers: a protocol grouping the kinds
//! the host routes to it that the same set of widget types handles. Each lane
//! is narrowed from the spawn result by [`InlineChild::narrow`], which compiles
//! only where the child type covers the lane, so a send through a lane is a
//! kind the child declares, and a child without a lane is skipped rather than
//! mailed a kind it would warn-drop.
//!
//! | Lane | Kinds | Covered by |
//! |---|---|---|
//! | [`WidgetSlot`] | `Collect` | every spawnable widget |
//! | [`WidgetStyled`] | `WidgetFrame`, `SetTheme` | every one but the compositing `Widget` |
//! | [`WidgetHover`] | `HoverGained`, `HoverLost` | the label and the `WidgetDefaults` adopters |
//! | [`WidgetControl`] | `FocusGained`, `FocusLost`, `MouseButton`, `MouseButtonRelease`, `Key` | the `WidgetDefaults` adopters |
//! | [`WidgetMotion`] | `MouseMove` | the adopters but button, radio, and toggle |
//! | [`WidgetWheel`] | `MouseWheel` | the virtual list and the scroll container |
//! | [`WidgetKeyRelease`] | `KeyRelease` | button, dropdown, and toggle |
//! | [`WidgetTextEntry`] | `TextInput`, `ImePreedit`, `Modifiers` | text field, text area, and numeric |
//!
//! A host's routing tables — focus, hover, composite slots — stay keyed by the
//! erased [`WidgetLanes::key`]: they compare, monitor, and purge by identity
//! and never send (ADR-0231 §4). A routing decision returns a key, and the host
//! looks up that key's lanes to send.

use aether_actor::{Addressable, ErasedActorRef, InlineChild, ProtocolRef, protocol};
use aether_kinds::{
    ImePreedit, Key, KeyRelease, Modifiers, MouseButton, MouseButtonRelease, MouseMove, MouseWheel, TextInput,
};

use crate::set::{
    ButtonWidget, DropdownWidget, ImageWidget, LabelWidget, MenuBarWidget, NumericWidget, RadioGroupWidget,
    SegmentedWidget, SliderWidget, TabStripWidget, TextAreaWidget, TextFieldWidget, ToggleWidget, VirtualListWidget,
};
use crate::theme::SetTheme;
use crate::{Collect, FocusGained, FocusLost, HoverGained, HoverLost, ScrollWidget, Widget, WidgetFrame};

/// The frame poll every widget answers with its draw list.
#[protocol]
pub trait WidgetSlot {
    fn collect(mail: Collect);
}

/// The layout rect and theme a host pushes down.
#[protocol]
pub trait WidgetStyled {
    fn frame(mail: WidgetFrame);
    fn set_theme(mail: SetTheme);
}

/// Hover edges.
#[protocol]
pub trait WidgetHover {
    fn hover_gained(mail: HoverGained);
    fn hover_lost(mail: HoverLost);
}

/// Focus edges plus the press, release, and key input a focusable control
/// reads.
#[protocol]
pub trait WidgetControl {
    fn focus_gained(mail: FocusGained);
    fn focus_lost(mail: FocusLost);
    fn mouse_button(mail: MouseButton);
    fn mouse_button_release(mail: MouseButtonRelease);
    fn key(mail: Key);
}

/// Pointer motion.
#[protocol]
pub trait WidgetMotion {
    fn mouse_move(mail: MouseMove);
}

/// The wheel, for a widget that scrolls itself.
#[protocol]
pub trait WidgetWheel {
    fn mouse_wheel(mail: MouseWheel);
}

/// Key release, for a control that activates on it.
#[protocol]
pub trait WidgetKeyRelease {
    fn key_release(mail: KeyRelease);
}

/// Committed text, IME composition, and the modifier chord a text editor
/// caches.
#[protocol]
pub trait WidgetTextEntry {
    fn text_input(mail: TextInput);
    fn ime_preedit(mail: ImePreedit);
    fn modifiers(mail: Modifiers);
}

/// One spawned child's lanes: its [`WidgetSlot`], which every widget covers,
/// and each other lane its type covers. Built only by [`WidgetLaneSet`], from
/// one spawn result, so every lane proves the same child.
#[derive(Clone, Copy)]
pub struct WidgetLanes {
    pub(crate) slot: ProtocolRef<WidgetSlot>,
    pub(crate) styled: Option<ProtocolRef<WidgetStyled>>,
    pub(crate) hover: Option<ProtocolRef<WidgetHover>>,
    pub(crate) control: Option<ProtocolRef<WidgetControl>>,
    pub(crate) motion: Option<ProtocolRef<WidgetMotion>>,
    pub(crate) wheel: Option<ProtocolRef<WidgetWheel>>,
    pub(crate) key_release: Option<ProtocolRef<WidgetKeyRelease>>,
    pub(crate) text_entry: Option<ProtocolRef<WidgetTextEntry>>,
}

impl WidgetLanes {
    /// A child covering the slot lane alone.
    const fn slot_only(slot: ProtocolRef<WidgetSlot>) -> Self {
        Self {
            slot,
            styled: None,
            hover: None,
            control: None,
            motion: None,
            wheel: None,
            key_release: None,
            text_entry: None,
        }
    }

    /// The child's identity: the key a routing table compares, monitors, and
    /// purges by, and that `ctx.sender()` is compared against.
    #[must_use]
    pub const fn key(&self) -> ErasedActorRef {
        self.slot.erase()
    }
}

mod sealed {
    /// The seal: only this crate's spawnable widget types name their lanes.
    pub trait Sealed {}
}

/// A widget type a host can spawn and hold by its lanes: the lanes the type
/// covers, narrowed from its spawn result.
///
/// Each impl names its lanes, and [`InlineChild::narrow`] refuses any the type
/// does not cover, so the claims are the compiler's. Sealed, so no impl can
/// assemble lanes from more than one child. A widget type added to a host
/// implements it here, beside its siblings.
pub trait WidgetLaneSet: Addressable + Sized + sealed::Sealed {
    /// The lanes `child` covers.
    fn lanes(child: InlineChild<Self>) -> WidgetLanes;
}

/// Implement [`WidgetLaneSet`] for a widget type from the lanes it covers
/// beyond the slot, each narrowed from the one spawn result.
macro_rules! lane_set {
    ($widget:ty $(: $($lane:ident),+)?) => {
        impl sealed::Sealed for $widget {}

        impl WidgetLaneSet for $widget {
            fn lanes(child: InlineChild<Self>) -> WidgetLanes {
                WidgetLanes { $($($lane: Some(child.narrow()),)+)? ..WidgetLanes::slot_only(child.narrow()) }
            }
        }
    };
}

lane_set!(Widget);
lane_set!(ImageWidget: styled);
lane_set!(ScrollWidget: styled, wheel);
lane_set!(LabelWidget: styled, hover);
lane_set!(RadioGroupWidget: styled, hover, control);
lane_set!(ButtonWidget: styled, hover, control, key_release);
lane_set!(ToggleWidget: styled, hover, control, key_release);
lane_set!(SliderWidget: styled, hover, control, motion);
lane_set!(SegmentedWidget: styled, hover, control, motion);
lane_set!(TabStripWidget: styled, hover, control, motion);
lane_set!(MenuBarWidget: styled, hover, control, motion);
lane_set!(DropdownWidget: styled, hover, control, motion, key_release);
lane_set!(VirtualListWidget: styled, hover, control, motion, wheel);
lane_set!(TextFieldWidget: styled, hover, control, motion, text_entry);
lane_set!(TextAreaWidget: styled, hover, control, motion, text_entry);
lane_set!(NumericWidget: styled, hover, control, motion, text_entry);
