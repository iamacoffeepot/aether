//! The two marker-attribute grammars.
//!
//! Both are validated to the same standard the capability applies at
//! registration — the name grammar, the description and title byte bounds, the
//! hint contradictions — so a tool that would be refused at runtime is refused
//! at compile time instead, with a span. The duplication of those bounds is
//! deliberate and one-directional: the registry stays the authority because a
//! hand-built registration never passes through this macro.

use std::collections::BTreeSet;

use proc_macro2::Span;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{Attribute, Expr, ExprLit, Ident, Lit, LitStr, Meta, Token, Type};

use crate::model::{Hints, Metadata};

/// Longest accepted tool name, matching the capability's own ceiling.
const NAME_MAXIMUM_BYTES: usize = 64;
/// Bytes a description may carry.
const DESCRIPTION_MAXIMUM_BYTES: usize = 4_096;
/// Bytes a title may carry.
const TITLE_MAXIMUM_BYTES: usize = 256;

/// Every bare flag the attribute accepts, in the order the protocol names them.
const FLAGS: [&str; 6] = ["read_only", "destructive", "non_destructive", "idempotent", "open_world", "closed_world"];

/// Parse `#[mcp::tool(name = …, description = …, …)]`.
pub fn tool_metadata(attribute: &Attribute) -> syn::Result<Metadata> {
    let entries = attribute.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;

    let mut name: Option<LitStr> = None;
    let mut title: Option<LitStr> = None;
    let mut description: Option<LitStr> = None;
    let mut flags: BTreeSet<String> = BTreeSet::new();

    for entry in &entries {
        match entry {
            Meta::NameValue(pair) => {
                let key = ident_of(pair.path.get_ident())?;
                let value = string_literal(&pair.value)?;
                let slot = match key.to_string().as_str() {
                    "name" => &mut name,
                    "title" => &mut title,
                    "description" => &mut description,
                    other => return Err(unknown_entry(key.span(), other)),
                };
                if slot.replace(value).is_some() {
                    return Err(syn::Error::new(key.span(), format!("`{key}` is stated twice")));
                }
            }
            Meta::Path(path) => {
                let flag = ident_of(path.get_ident())?;
                if !FLAGS.contains(&flag.to_string().as_str()) {
                    return Err(unknown_entry(flag.span(), &flag.to_string()));
                }
                if !flags.insert(flag.to_string()) {
                    return Err(syn::Error::new(flag.span(), format!("`{flag}` is stated twice")));
                }
            }
            Meta::List(list) => {
                return Err(syn::Error::new(
                    list.span(),
                    "#[mcp::tool] takes `key = \"value\"` entries and bare flags",
                ));
            }
        }
    }

    let span = attribute.span();
    let name = name.ok_or_else(|| syn::Error::new(span, "#[mcp::tool] requires a literal `name`"))?;
    let description =
        description.ok_or_else(|| syn::Error::new(span, "#[mcp::tool] requires a literal `description`"))?;

    Ok(Metadata {
        name: validated_name(&name)?,
        name_span: name.span(),
        title: validated_title(title)?,
        description: bounded(description, "description", 1, DESCRIPTION_MAXIMUM_BYTES)?,
        hints: resolve_hints(&flags, span)?,
    })
}

fn unknown_entry(span: Span, found: &str) -> syn::Error {
    syn::Error::new(
        span,
        format!(
            "`{found}` is not a #[mcp::tool] entry; accepted are `name`, `title`, `description`, and the flags {}",
            FLAGS.join(", "),
        ),
    )
}

fn ident_of(candidate: Option<&Ident>) -> syn::Result<Ident> {
    candidate.cloned().ok_or_else(|| syn::Error::new(Span::call_site(), "#[mcp::tool] entries are plain identifiers"))
}

fn string_literal(expr: &Expr) -> syn::Result<LitStr> {
    match expr {
        Expr::Lit(ExprLit { lit: Lit::Str(text), .. }) => Ok(text.clone()),
        other => Err(syn::Error::new(other.span(), "#[mcp::tool] values must be string literals")),
    }
}

fn bounded(value: LitStr, role: &str, low: usize, high: usize) -> syn::Result<LitStr> {
    let bytes = value.value().len();
    if bytes < low || bytes > high {
        return Err(syn::Error::new(value.span(), format!("a tool {role} must be {low} through {high} UTF-8 bytes")));
    }
    Ok(value)
}

fn validated_title(title: Option<LitStr>) -> syn::Result<Option<LitStr>> {
    title.map(|value| bounded(value, "title", 1, TITLE_MAXIMUM_BYTES)).transpose()
}

/// Enforce `^[a-z][a-z0-9_]{0,63}$`.
///
/// The grammar is what lets the minted kind name paste the tool name verbatim,
/// so a character outside it would produce a kind name nothing can address.
fn validated_name(literal: &LitStr) -> syn::Result<String> {
    let name = literal.value();
    let refuse = || {
        Err(syn::Error::new(
            literal.span(),
            format!(
                "tool name `{name}` is not the accepted grammar: a lowercase letter followed by up to {} more \
                 lowercase letters, digits, or underscores",
                NAME_MAXIMUM_BYTES - 1,
            ),
        ))
    };

    let mut characters = name.chars();
    let leads = characters.next().is_some_and(|first| first.is_ascii_lowercase());
    let tail_ok = characters.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if name.len() > NAME_MAXIMUM_BYTES || !leads || !tail_ok {
        return refuse();
    }
    Ok(name)
}

/// Fold the bare flags into the four wire hints, refusing a pair that states
/// two things at once.
fn resolve_hints(flags: &BTreeSet<String>, span: Span) -> syn::Result<Hints> {
    let stated = |flag: &str| flags.contains(flag);
    let contradiction = |one: &str, two: &str| {
        syn::Error::new(span, format!("`{one}` and `{two}` contradict each other; state at most one"))
    };

    if stated("read_only") && stated("destructive") {
        return Err(contradiction("read_only", "destructive"));
    }
    if stated("destructive") && stated("non_destructive") {
        return Err(contradiction("destructive", "non_destructive"));
    }
    if stated("open_world") && stated("closed_world") {
        return Err(contradiction("open_world", "closed_world"));
    }

    let read_only = stated("read_only");
    Ok(Hints {
        read_only,
        // A read-only tool cannot be destructive, so the flag selects both
        // rather than leaving the protocol's conservative default standing.
        destructive: !read_only && !stated("non_destructive"),
        idempotent: stated("idempotent"),
        open_world: !stated("closed_world"),
    })
}

/// Parsed `#[mcp::reply(ReplyKind, tool = method, map = mapper)]`.
pub struct ReplyMarker {
    pub kind: Type,
    pub tool: Ident,
    /// Present when the marker rides above a retained handler and names a
    /// separate `fn(&ReplyKind) -> Result<Output, ToolError>`; absent when the
    /// annotated method is itself the mapping.
    pub map: Option<Ident>,
}

impl Parse for ReplyMarker {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let kind: Type = input.parse()?;
        let mut tool: Option<Ident> = None;
        let mut map: Option<Ident> = None;

        while !input.is_empty() {
            input.parse::<Token![,]>()?;
            if input.is_empty() {
                break;
            }
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            let value: Ident = input.parse()?;
            let slot = match key.to_string().as_str() {
                "tool" => &mut tool,
                "map" => &mut map,
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!("`{other}` is not a #[mcp::reply] entry; accepted are `tool` and `map`"),
                    ));
                }
            };
            if slot.replace(value).is_some() {
                return Err(syn::Error::new(key.span(), format!("`{key}` is stated twice")));
            }
        }

        let tool = tool.ok_or_else(|| {
            input.error("#[mcp::reply] requires `tool = <method>` naming the #[mcp::tool] method it answers for")
        })?;
        Ok(Self { kind, tool, map })
    }
}
