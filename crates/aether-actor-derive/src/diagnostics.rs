use syn::{Attribute, Expr, ExprLit, Lit, Meta};

pub fn extract_agent_doc(attrs: &[Attribute]) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        let Meta::NameValue(nv) = &attr.meta else {
            continue;
        };
        let Expr::Lit(ExprLit { lit: Lit::Str(s), .. }) = &nv.value else {
            continue;
        };
        lines.push(s.value());
    }
    if lines.is_empty() {
        return None;
    }
    let full = lines.join("\n");
    let full_trimmed = full.trim();
    if full_trimmed.is_empty() {
        return None;
    }

    // Scan for a `# Agent` heading (conventional rustdoc section
    // heading, top-level `#` followed by space). Capture everything
    // until the next top-level heading or end-of-doc.
    let mut in_agent = false;
    let mut found_agent = false;
    let mut agent_lines: Vec<&str> = Vec::new();
    for line in full.lines() {
        let trimmed = line.trim_start();
        let starts_h1 = trimmed.starts_with("# ") && !trimmed.starts_with("## ");
        if starts_h1 {
            if in_agent {
                // A new top-level heading ends the Agent section.
                break;
            }
            let heading = trimmed.trim_start_matches('#').trim();
            if heading.eq_ignore_ascii_case("Agent") {
                in_agent = true;
                found_agent = true;
                continue;
            }
            continue;
        }
        if in_agent {
            agent_lines.push(line);
        }
    }

    if found_agent {
        let s = agent_lines.join("\n").trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    } else {
        Some(full_trimmed.to_string())
    }
}
