//! Set-difference of two tables: symbols the working tree introduces.

use std::collections::BTreeSet;

use serde::Serialize;

use crate::symbols::table::{Symbol, Table};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DiffResult {
    pub introduced: Vec<Symbol>,
}

pub fn diff(base: &Table, current: &Table) -> DiffResult {
    let base_ids: BTreeSet<_> = base.symbols.iter().map(Symbol::identity).collect();
    let introduced = current.symbols.iter().filter(|symbol| !base_ids.contains(&symbol.identity())).cloned().collect();
    DiffResult { introduced }
}

#[cfg(test)]
mod tests {
    use super::diff;
    use crate::symbols::extract::extract_source;
    use crate::symbols::table::{SymbolKind, Table};

    fn table(source: &str) -> Table {
        Table::new(extract_source("demo", "src/lib.rs", source, false).expect("parse fixture"))
    }

    #[test]
    fn diff_reports_exactly_the_added_function() {
        // The verify gate consumes this: a one-function edit must surface that
        // symbol and nothing the base already named, including helpers that
        // merely moved path.
        let base = table(
            r"
            fn digest(byte: u8) -> u8 { byte }
            fn to_hex() {}
        ",
        );
        let current = table(
            r"
            fn digest(byte: u8) -> u8 { byte }
            fn to_hex() {}
            fn scratch_dir() {}
        ",
        );
        let result = diff(&base, &current);
        let names: Vec<&str> = result.introduced.iter().map(|symbol| symbol.name.as_str()).collect();
        assert_eq!(names, vec!["scratch_dir"]);
        assert_eq!(result.introduced[0].kind, SymbolKind::Fn);
    }

    #[test]
    fn diff_ignores_path_only_churn_for_the_same_identity() {
        let mut base = table("fn digest() {}");
        let current = table("fn digest() {}");
        base.symbols[0].path = "old/lib.rs".to_string();
        let result = diff(&base, &current);
        assert!(result.introduced.is_empty(), "same crate/module/name/kind is not introduced: {:?}", result.introduced);
    }
}
