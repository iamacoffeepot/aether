//! Human-readable rendering of `GET /view`.

use anyhow::{Context, Result};
use serde_json::Value;

/// Render the live view: the two heads a successor can seal on, then each
/// bloom's status and supersession link.
pub(super) fn render(view: &Value) -> Result<String> {
    let mainline = view.get("mainline").and_then(Value::as_str).context("view is missing mainline")?;
    let observed = view.get("observed").and_then(Value::as_str).context("view is missing observed")?;
    let mut lines = vec![format!("mainline  {mainline}"), format!("observed  {observed}"), String::new()];

    let blooms = view.get("blooms").and_then(Value::as_array).context("view is missing blooms")?;
    if blooms.is_empty() {
        lines.push("(no blooms)".to_owned());
        return Ok(lines.join("\n"));
    }

    let mut rows: Vec<&Value> = blooms.iter().collect();
    rows.sort_by_key(|bloom| bloom.get("id").and_then(Value::as_str).unwrap_or(""));
    for bloom in rows {
        lines.push(render_bloom(bloom)?);
    }
    Ok(lines.join("\n"))
}

fn render_bloom(bloom: &Value) -> Result<String> {
    let id = bloom.get("id").and_then(Value::as_str).context("bloom is missing id")?;
    let status = bloom.get("status").and_then(Value::as_str).unwrap_or("unknown");
    let mut line = bloom
        .get("superseded_by")
        .and_then(Value::as_str)
        .map_or_else(|| format!("{id}  {status}"), |successor| format!("{id}  {status}  →  {successor}"));

    if let Some(members) = bloom.get("members").and_then(Value::as_array)
        && !members.is_empty()
    {
        let names: Vec<&str> =
            members.iter().filter_map(|member| member.get("workpiece").and_then(Value::as_str)).collect();
        if !names.is_empty() {
            line.push_str("  [");
            line.push_str(&names.join(", "));
            line.push(']');
        }
    }
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::render;
    use serde_json::json;

    #[test]
    fn render_names_status_and_the_supersession_link() {
        let view = json!({
            "mainline": "aa".repeat(32),
            "observed": "bb".repeat(32),
            "blooms": [
                {
                    "id": "cc".repeat(32),
                    "status": "Superseded",
                    "superseded_by": "dd".repeat(32),
                    "members": [{ "workpiece": "wp-1" }],
                },
                {
                    "id": "dd".repeat(32),
                    "status": "Sealed",
                    "superseded_by": null,
                    "members": [{ "workpiece": "wp-1" }],
                },
            ],
        });

        let rendered = render(&view).expect("view renders");
        assert!(rendered.contains("mainline  "), "the heads a successor can seal on are named");
        assert!(rendered.contains("observed  "), "the default successor base is named");
        assert!(
            rendered.contains(&format!("{}  Superseded  →  {}", "cc".repeat(32), "dd".repeat(32))),
            "a superseded bloom names its successor: {rendered}"
        );
        assert!(rendered.contains(&format!("{}  Sealed", "dd".repeat(32))), "the live bloom is listed: {rendered}");
        assert!(rendered.contains("[wp-1]"), "membership is readable: {rendered}");
    }

    #[test]
    fn empty_view_says_so() {
        let view = json!({
            "mainline": "aa".repeat(32),
            "observed": "bb".repeat(32),
            "blooms": [],
        });
        assert!(render(&view).expect("empty view renders").contains("(no blooms)"));
    }
}
