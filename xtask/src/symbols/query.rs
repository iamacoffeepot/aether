//! Name-substring and underscore-folded similarity search.

use serde::Serialize;

use crate::symbols::table::{Symbol, Table};

/// Bounded find result: `matched` is the full hit count, `matches` is the printed prefix.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FindResult {
    pub query: String,
    pub matched: usize,
    pub shown: usize,
    pub matches: Vec<Symbol>,
}

pub fn find(table: &Table, query: &str, limit: usize) -> FindResult {
    let mut scored: Vec<(Score, &Symbol)> = table
        .symbols
        .iter()
        .filter(|symbol| is_match(&symbol.name, query))
        .map(|symbol| (score(&symbol.name, query), symbol))
        .collect();
    scored.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(right.1)));
    let hit_count = scored.len();
    let cap = if limit == 0 {
        hit_count
    } else {
        limit.min(hit_count)
    };
    let hits: Vec<Symbol> = scored.into_iter().take(cap).map(|(_, symbol)| symbol.clone()).collect();
    FindResult { query: query.to_string(), matched: hit_count, shown: hits.len(), matches: hits }
}

fn is_match(name: &str, query: &str) -> bool {
    if query.is_empty() {
        return false;
    }
    let name_l = name.to_lowercase();
    let query_l = query.to_lowercase();
    if name_l.contains(&query_l) {
        return true;
    }
    let name_n = normalize(name);
    let query_n = normalize(query);
    !query_n.is_empty() && name_n.contains(&query_n)
}

/// Lowercase, drop underscores — `to_hex` and `digest_hex` share the `hex` token.
fn normalize(name: &str) -> String {
    name.chars().filter(|&ch| ch != '_').flat_map(char::to_lowercase).collect()
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Score {
    exact: u8,
    position: usize,
    length: usize,
}

fn score(name: &str, query: &str) -> Score {
    let name_n = normalize(name);
    let query_n = normalize(query);
    let exact = u8::from(name_n != query_n);
    let position = name_n.find(&query_n).unwrap_or(usize::MAX);
    Score { exact, position, length: name_n.len() }
}

#[cfg(test)]
mod tests {
    use super::find;
    use crate::symbols::table::{Signature, Symbol, SymbolKind, Table};

    fn symbol(crate_name: &str, name: &str) -> Symbol {
        Symbol {
            crate_name: crate_name.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Fn,
            module: format!("{crate_name}::tests"),
            visibility: "private".to_string(),
            signature: Signature { arity: 0, inputs: Vec::new(), output: "()".to_string() },
            doc: None,
            test: true,
            path: format!("{crate_name}/src/lib.rs"),
        }
    }

    fn census_table() -> Table {
        Table::new(vec![
            symbol("aether_bloomery", "digest"),
            symbol("aether_bloomery_console", "digest"),
            symbol("aether_bloomery_git", "digest_hex"),
            symbol("aether_bloomery_git", "to_hex"),
            symbol("aether_chassis", "to_hex"),
            symbol("aether_data", "encode_hex"),
            symbol("unrelated", "scratch_dir"),
        ])
    }

    #[test]
    fn find_digest_surfaces_the_fixture_family() {
        // The duplication census's `fn digest` cluster is the query a lane
        // issues before writing another one. Dropping substring match would
        // hide the family; dropping the unrelated helper proves the filter.
        let found = find(&census_table(), "digest", 64);
        let names: Vec<&str> = found.matches.iter().map(|symbol| symbol.name.as_str()).collect();
        assert!(names.contains(&"digest"), "exact digest helpers: {names:?}");
        assert!(names.contains(&"digest_hex"), "folded neighbor digest_hex: {names:?}");
        assert!(!names.contains(&"scratch_dir"));
        assert!(found.matches.iter().any(|symbol| symbol.crate_name == "aether_bloomery"));
        assert!(found.matches.iter().any(|symbol| symbol.crate_name == "aether_bloomery_console"));
    }

    #[test]
    fn find_hex_surfaces_the_encoder_family_across_crates() {
        let found = find(&census_table(), "hex", 64);
        let names: Vec<&str> = found.matches.iter().map(|symbol| symbol.name.as_str()).collect();
        assert!(names.contains(&"to_hex"), "{names:?}");
        assert!(names.contains(&"digest_hex"), "{names:?}");
        assert!(names.contains(&"encode_hex"), "{names:?}");
        let crates: Vec<&str> = found.matches.iter().map(|symbol| symbol.crate_name.as_str()).collect();
        assert!(crates.contains(&"aether_bloomery_git"));
        assert!(crates.contains(&"aether_chassis"));
        assert!(crates.contains(&"aether_data"));
    }

    #[test]
    fn find_folds_case_and_underscores() {
        let found = find(&census_table(), "ToHex", 64);
        assert!(found.matches.iter().any(|symbol| symbol.name == "to_hex"));
    }

    #[test]
    fn find_caps_rows_and_reports_the_full_match_count() {
        // A lane reads this in one call: blowing past the cap hides nothing
        // (matched still counts) but must not dump every `new` in the tree.
        let found = find(&census_table(), "hex", 1);
        assert_eq!(found.shown, 1);
        assert!(found.matched > 1, "cap is on shown, not on the search");
    }
}
