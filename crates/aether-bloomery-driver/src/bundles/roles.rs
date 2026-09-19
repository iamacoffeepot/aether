//! A bundle's declared roles: programs, reactors, or both (ADR-0226 decision 2).
//!
//! The roles are valid by construction: "no roles" cannot be represented,
//! and a declared program role always carries at least one declaration.
//! [`DeclaredRoles::new`] is the only constructor, and only the section
//! reader calls it.

use aether_bloomery_kinds::{Program, ProgramName, ReactorDeclaration};

/// A bundle's program declarations: never empty.
#[derive(Debug, Clone)]
pub struct Programs(Vec<Program>);

impl Programs {
    /// The declaration named `name`, if the bundle declares it.
    pub fn find(&self, name: &ProgramName) -> Option<&Program> {
        self.0.iter().find(|declared| declared.name == *name)
    }
}

/// The roles one bundle declares: at least one.
#[derive(Debug, Clone)]
pub enum DeclaredRoles {
    /// Program declarations only.
    Programs(Programs),
    /// At least one reactor declaration; the driver never reads them.
    Reactors,
    /// Program declarations plus at least one reactor declaration.
    Both(Programs),
}

impl DeclaredRoles {
    /// Classify decoded declarations. `None` when the bundle declares neither role.
    pub(super) fn new(programs: Vec<Program>, reactors: &[ReactorDeclaration]) -> Option<Self> {
        match (programs.is_empty(), reactors.is_empty()) {
            (true, true) => None,
            (false, true) => Some(Self::Programs(Programs(programs))),
            (true, false) => Some(Self::Reactors),
            (false, false) => Some(Self::Both(Programs(programs))),
        }
    }

    /// The declared programs, when the program role is declared.
    pub fn programs(&self) -> Option<&Programs> {
        match self {
            Self::Programs(programs) | Self::Both(programs) => Some(programs),
            Self::Reactors => None,
        }
    }

    /// Whether the reactor role is declared.
    pub fn declares_reactors(&self) -> bool {
        match self {
            Self::Programs(_) => false,
            Self::Reactors | Self::Both(_) => true,
        }
    }
}
