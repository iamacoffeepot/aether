//! Identifier folds for the minted siblings.
//!
//! Sibling type names carry the actor type so two actors in one module cannot
//! collide, and generated method names carry a reserved prefix so an authored
//! method cannot be shadowed by one. Both folds are total on the inputs that
//! reach them — a Rust identifier and a path segment — so neither can produce
//! an invalid identifier from valid input.

/// `list_commissions_tool` → `ListCommissionsTool`.
pub fn camel(snake: &str) -> String {
    snake
        .split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut characters = part.chars();
            let head: String = characters.by_ref().take(1).flat_map(char::to_uppercase).collect();
            head + characters.as_str()
        })
        .collect()
}

/// `ListCommissionsResult` → `list_commissions_result`.
///
/// A run of capitals stays one word, so `HttpServerResponse` folds to
/// `http_server_response` rather than `h_t_t_p_…`.
pub fn snake(camel: &str) -> String {
    let characters: Vec<char> = camel.chars().collect();
    let mut folded = String::with_capacity(camel.len() + 4);
    for (index, current) in characters.iter().enumerate() {
        let starts_word = current.is_uppercase()
            && index > 0
            && (!characters[index - 1].is_uppercase()
                || characters.get(index + 1).is_some_and(char::is_ascii_lowercase));
        if starts_word && !folded.ends_with('_') {
            folded.push('_');
        }
        folded.extend(current.to_lowercase());
    }
    folded
}

#[cfg(test)]
mod tests {
    use super::{camel, snake};

    // Tripwire: the minted request / value / boundary sibling names are built
    // from this fold. A regression that dropped or mangled a segment would let
    // two tools on one actor mint the same struct name, and the collision would
    // surface as a confusing duplicate-definition error far from its cause.
    #[test]
    fn camel_folds_every_snake_segment() {
        assert_eq!(camel("list_commissions_tool"), "ListCommissionsTool");
        assert_eq!(camel("echo"), "Echo");
        assert_eq!(camel("add_2_numbers"), "Add2Numbers");
    }

    // Tripwire: one generated handler per reply kind is the invariant the whole
    // composite-reply surface protects, and the handler's name is this fold of
    // the kind. A regression that folded two distinct kinds to one name would
    // silently merge two groups into one handler.
    #[test]
    fn snake_keeps_capital_runs_together() {
        assert_eq!(snake("ListCommissionsResult"), "list_commissions_result");
        assert_eq!(snake("HttpServerResponse"), "http_server_response");
        assert_eq!(snake("EchoResult"), "echo_result");
    }
}
