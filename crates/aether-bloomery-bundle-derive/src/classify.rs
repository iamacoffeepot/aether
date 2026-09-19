//! Classification of the generator's exports into program and reactor roles.
//!
//! One pass over `exports` in order matches each type to its `actors`
//! envelope and collects the programs and reactors it names. Every misuse is
//! a spanned compile error: the reserved root namespace, a duplicate program
//! `NAME`, a non-literal or duplicate reactor `NAMESPACE`, a module with
//! neither role, and a `boot` / `default` that names a program or reactor.
//! The returned [`Roles`] holds at least one role, so "neither role" cannot
//! reach expansion.

use proc_macro2::Span;
use quote::quote;
use syn::Type;

use aether_bloomery_kinds::BUNDLE_NAMESPACE;

use crate::input::{GenerateInput, NamespaceTok, ProgramMeta, Tag};

pub enum Roles {
    Programs(Vec<ProgramEntry>),
    Reactors(Vec<ReactorEntry>),
    Both { programs: Vec<ProgramEntry>, reactors: Vec<ReactorEntry> },
}

pub struct ProgramEntry {
    pub ty: Type,
    pub meta: ProgramMeta,
}

pub struct ReactorEntry {
    pub ty: Type,
    pub namespace: String,
}

impl Roles {
    pub fn contains(&self, ty: &Type) -> bool {
        self.is_program(ty) || self.is_reactor(ty)
    }

    fn is_program(&self, ty: &Type) -> bool {
        match self {
            Self::Programs(programs) | Self::Both { programs, .. } => {
                type_in(ty, programs.iter().map(|entry| &entry.ty))
            }
            Self::Reactors(_) => false,
        }
    }

    fn is_reactor(&self, ty: &Type) -> bool {
        match self {
            Self::Reactors(reactors) | Self::Both { reactors, .. } => {
                type_in(ty, reactors.iter().map(|entry| &entry.ty))
            }
            Self::Programs(_) => false,
        }
    }
}

/// Classify the input's exports into the roles the root must serve.
///
/// # Errors
///
/// A spanned error for the reserved root namespace, a duplicate program
/// `NAME`, a non-literal or duplicate reactor `NAMESPACE`, a module with
/// neither role, or a `boot` / `default` that names a program or reactor.
pub fn classify(input: &GenerateInput) -> syn::Result<Roles> {
    for entry in &input.actors {
        if let NamespaceTok::Lit(namespace) = &entry.namespace
            && namespace == BUNDLE_NAMESPACE
        {
            return Err(syn::Error::new_spanned(
                &entry.ty,
                format!("NAMESPACE `{namespace}` is reserved for the generated bundle root"),
            ));
        }
    }

    let mut programs = Vec::new();
    let mut reactors = Vec::new();
    let mut seen_names = Vec::new();
    let mut seen_namespaces = Vec::new();
    for export_ty in &input.exports {
        let Some(entry) = input.actors.iter().find(|entry| types_eq(export_ty, &entry.ty)) else {
            continue;
        };
        match &entry.tag {
            Tag::Ordinary => {}
            Tag::Program(meta) => {
                let name = meta.name.value();
                if seen_names.iter().any(|existing| existing == &name) {
                    return Err(syn::Error::new_spanned(&entry.ty, format!("duplicate program NAME `{name}`")));
                }
                seen_names.push(name);
                programs.push(ProgramEntry { ty: entry.ty.clone(), meta: (**meta).clone() });
            }
            Tag::Reactor => {
                let NamespaceTok::Lit(namespace) = &entry.namespace else {
                    return Err(syn::Error::new_spanned(&entry.ty, "reactor NAMESPACE must be a string literal"));
                };
                if seen_namespaces.iter().any(|existing| existing == namespace) {
                    return Err(syn::Error::new_spanned(
                        &entry.ty,
                        format!("duplicate reactor NAMESPACE `{namespace}`"),
                    ));
                }
                seen_namespaces.push(namespace.clone());
                reactors.push(ReactorEntry { ty: entry.ty.clone(), namespace: namespace.clone() });
            }
        }
    }
    let roles = match (programs.is_empty(), reactors.is_empty()) {
        (true, true) => {
            return Err(syn::Error::new(
                Span::call_site(),
                "bundle found no #[program] or #[reactor] exports in this export! set",
            ));
        }
        (false, true) => Roles::Programs(programs),
        (true, false) => Roles::Reactors(reactors),
        (false, false) => Roles::Both { programs, reactors },
    };

    if let Some(boot) = &input.boot {
        if roles.is_program(boot) {
            return Err(syn::Error::new_spanned(boot, "export! boot type cannot be a program"));
        }
        if roles.is_reactor(boot) {
            return Err(syn::Error::new_spanned(boot, "export! boot type cannot be a reactor"));
        }
    }
    if let Some(default) = &input.default {
        let kind = if roles.is_program(default) {
            Some("program")
        } else if roles.is_reactor(default) {
            Some("reactor")
        } else {
            None
        };
        if let Some(kind) = kind {
            let mixed = input.exports.iter().any(|ty| !roles.contains(ty));
            return Err(syn::Error::new_spanned(
                default,
                if mixed {
                    format!(
                        "export! default cannot be a {kind} in a mixed module; name an ordinary actor or omit default"
                    )
                } else {
                    format!(
                        "export! default cannot be a {kind}; a bundle-only module exports the generated bundle root"
                    )
                },
            ));
        }
    }
    Ok(roles)
}

fn type_in<'a>(needle: &Type, haystack: impl IntoIterator<Item = &'a Type>) -> bool {
    haystack.into_iter().any(|ty| types_eq(needle, ty))
}

fn types_eq(left: &Type, right: &Type) -> bool {
    quote!(#left).to_string() == quote!(#right).to_string()
}
