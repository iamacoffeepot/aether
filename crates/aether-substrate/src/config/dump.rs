//! The `--print-config` discovery dump (ADR-0090 §4): walk the composed
//! members' confique `Meta`s plus the residual hand-registered knob records
//! into a stable plaintext table of key, live value, source label, default,
//! and doc.

use std::env;
use std::fmt::Write as _;

use confique::meta::{Expr, Field, FieldKind, LeafKind, Meta};

use super::known_keys::KnobRecord;

/// Render a `confique::meta::Expr` default as a plain string (matching
/// how it would be typed in env). Best-effort for the discovery dump;
/// composite defaults (`Array` / `Map`) render in a compact debug
/// shape since they have no single env representation.
fn render_expr(expr: &Expr) -> String {
    match expr {
        Expr::Str(s) => (*s).to_owned(),
        Expr::Float(fl) => fl.to_string(),
        Expr::Integer(i) => i.to_string(),
        Expr::Bool(b) => b.to_string(),
        Expr::Array(items) => {
            let inner: Vec<String> = items.iter().map(render_expr).collect();
            format!("[{}]", inner.join(","))
        }
        Expr::Map(_) => "<map>".to_owned(),
        // `Expr` is `#[non_exhaustive]` — any future variant renders
        // as a placeholder rather than failing the dump.
        _ => "<expr>".to_owned(),
    }
}

/// One resolved row in the [`dump_config`] table.
struct DumpRow {
    key: String,
    value: String,
    source: &'static str,
    default: String,
    doc: String,
}

/// Resolve one confique leaf's discovery row: read the live env value
/// (the value the running config would resolve to) and label its
/// source as `env` (set) or `default` (unset).
fn leaf_row(env_key: &str, leaf: &LeafKind, doc: &[&'static str]) -> DumpRow {
    let default = match leaf {
        LeafKind::Required { default: Some(expr) } => render_expr(expr),
        LeafKind::Required { default: None } | LeafKind::Optional => String::new(),
    };
    // The sanctioned ADR-0090 config machinery: this is the central env-read
    // the whole derive-`Config` system funnels through (the `#[config(env =
    // ...)]` discovery dump), the one place capabilities *should* configure
    // through rather than reading env themselves.
    #[allow(clippy::disallowed_methods)]
    let (value, source) = env::var(env_key).map_or_else(|_| (default.clone(), "default"), |v| (v, "env"));
    DumpRow { key: env_key.to_owned(), value, source, default, doc: doc.join(" ").trim().to_owned() }
}

/// Walk one `Meta` into `rows`, resolving every leaf's discovery row
/// (recursing `Nested`). Iterative work-stack, same shape as
/// [`collect_meta_env_keys`](super::known_keys::collect_meta_env_keys).
fn collect_meta_rows(meta: &'static Meta, rows: &mut Vec<DumpRow>) {
    let mut stack: Vec<&'static Meta> = vec![meta];
    while let Some(m) = stack.pop() {
        for field in m.fields {
            let Field { doc, kind, .. } = field;
            match kind {
                FieldKind::Leaf { env: Some(key), kind: leaf } => rows.push(leaf_row(key, leaf, doc)),
                FieldKind::Leaf { env: None, .. } => {}
                FieldKind::Nested { meta } => stack.push(meta),
            }
        }
    }
}

/// Render the `--config` discovery dump (ADR-0090 §4): walk the same
/// `Meta`-slice + `KnobRecord`-slice registry e1 assembles and e2
/// reads, printing every knob with its live source-resolved value,
/// source label (`env` / `default`), default, and doc. Confique knobs
/// come from the `Meta` walk (the single source of truth — no second
/// hand-maintained list); hand-registered knobs render their
/// `KnobRecord` directly (`source` = `env` when the var is set, else
/// `unregistered-default` since their default lives only in the
/// record). Output is a stable plaintext table.
#[must_use]
pub fn dump_config(metas: &[&'static Meta], records: &[KnobRecord]) -> String {
    let mut rows: Vec<DumpRow> = Vec::new();
    for meta in metas {
        collect_meta_rows(meta, &mut rows);
    }
    for record in records {
        let default = record.default.unwrap_or("").to_owned();
        // The sanctioned ADR-0090 config machinery: resolving a hand-registered
        // knob's live source for the `--config` discovery dump, the central
        // config-read path, not a cap reading its own env.
        #[allow(clippy::disallowed_methods)]
        let (value, source) = env::var(record.env_key).map_or_else(|_| (default.clone(), "default"), |v| (v, "env"));
        rows.push(DumpRow { key: record.env_key.to_owned(), value, source, default, doc: record.doc.to_owned() });
    }
    rows.sort_by(|a, b| a.key.cmp(&b.key));

    let key_w = rows.iter().map(|r| r.key.len()).max().unwrap_or(3).max(3);
    let val_w = rows.iter().map(|r| r.value.len()).max().unwrap_or(5).max(5);
    let src_w = 7; // "default" is the widest source label
    let def_w = rows.iter().map(|r| r.default.len()).max().unwrap_or(7).max(7);

    let mut out = String::new();
    let (k, v, s, d, doc) = ("KEY", "VALUE", "SOURCE", "DEFAULT", "DOC");
    let _ = writeln!(out, "{k:<key_w$}  {v:<val_w$}  {s:<src_w$}  {d:<def_w$}  {doc}");
    for r in &rows {
        let (key, value, source, default, doc) = (&r.key, &r.value, r.source, &r.default, &r.doc);
        let _ = writeln!(out, "{key:<key_w$}  {value:<val_w$}  {source:<src_w$}  {default:<def_w$}  {doc}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_fixtures::{FIXTURE_KNOBS, fixture_meta};

    #[test]
    fn dump_config_renders_meta_keys_defaults_and_docs() {
        let dump = dump_config(&[fixture_meta()], FIXTURE_KNOBS);
        // Confique knob from the Meta walk: key + default + a header.
        assert!(dump.contains("AETHER_TEST_COUNT"));
        assert!(dump.contains('7')); // the count default
        assert!(dump.contains("KEY"));
        assert!(dump.contains("SOURCE"));
        // Hand-registered knob rendered directly.
        assert!(dump.contains("AETHER_FIXTURE_KNOB"));
        assert!(dump.contains("a hand-registered fixture knob"));
    }

    #[test]
    fn dump_config_labels_env_set_value_as_env_source() {
        // SAFETY: single-threaded test; unique key set then removed.
        unsafe { env::set_var("AETHER_FIXTURE_KNOB", "99") };
        let dump = dump_config(&[], FIXTURE_KNOBS);
        // SAFETY: same scope.
        unsafe { env::remove_var("AETHER_FIXTURE_KNOB") };
        let row = dump.lines().find(|l| l.contains("AETHER_FIXTURE_KNOB")).expect("knob row present");
        assert!(row.contains("99"), "value should be the env override: {row}");
        assert!(row.contains("env"), "source should be env: {row}");
    }
}
