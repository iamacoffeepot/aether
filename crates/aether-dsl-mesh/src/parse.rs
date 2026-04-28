//! Parse Lisp s-expression text into a typed mesh AST.
//!
//! Per ADR-0026, the format is Lisp-syntactic *data*, not a programmable Lisp.
//! Parsing produces a static tree; nothing is evaluated.

use lexpr::Value;

use crate::ast::{Axis, Node};
use aether_math::Vec3;

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("lexpr read error: {0}")]
    Read(#[from] lexpr::parse::Error),
    #[error("expected a list, got {0}")]
    NotAList(String),
    #[error("expected a proper list (nil-terminated), got dotted pair")]
    NotProperList,
    #[error("empty list at node position")]
    EmptyList,
    #[error("expected head symbol, got {0}")]
    ExpectedSymbol(String),
    #[error("unknown node head: {0}")]
    UnknownNode(String),
    #[error("expected number, got {0}")]
    ExpectedNumber(String),
    #[error("expected integer, got {0}")]
    ExpectedInteger(String),
    #[error("expected boolean (#t / #f / true / false), got {0}")]
    ExpectedBool(String),
    #[error("expected symbol, got {0}")]
    ExpectedSymbolValue(String),
    #[error("{node}: wrong number of positional arguments — expected {expected}, got {got}")]
    WrongArity {
        node: &'static str,
        expected: usize,
        got: usize,
    },
    #[error("{node}: missing required keyword :{keyword}")]
    MissingKeyword {
        node: &'static str,
        keyword: &'static str,
    },
    #[error("{node}: unknown keyword :{keyword}")]
    UnknownKeyword { node: &'static str, keyword: String },
    #[error("{node}: expected vector of length {expected}, got {got}")]
    WrongVectorLength {
        node: &'static str,
        expected: usize,
        got: usize,
    },
    #[error("axis must be one of x, y, z, got {0}")]
    InvalidAxis(String),
    #[error("profile point must be (x y), got list of length {0}")]
    InvalidProfilePoint(usize),
    #[error("sweep: :scales length ({scales_len}) must equal path length ({path_len})")]
    SweepScalesLengthMismatch { scales_len: usize, path_len: usize },
    #[error("{node}: requires at least {min} children, got {got}")]
    CsgArityTooFew {
        node: &'static str,
        min: usize,
        got: usize,
    },
    #[error("trailing top-level form after first node: {trailing}")]
    TrailingInput { trailing: String },
}

pub fn parse(text: &str) -> Result<Node, ParseError> {
    let opts = lexpr::parse::Options::default()
        .with_keyword_syntax(lexpr::parse::KeywordSyntax::ColonPrefix);
    let mut parser = lexpr::Parser::from_str_custom(text, opts);
    let value = parser
        .next_value()?
        .ok_or_else(|| ParseError::NotAList("empty input".into()))?;
    let node = parse_node(&value)?;
    if let Some(extra) = parser.next_value()? {
        return Err(ParseError::TrailingInput {
            trailing: format!("{extra}"),
        });
    }
    Ok(node)
}

fn parse_node(value: &Value) -> Result<Node, ParseError> {
    let items = list_to_vec(value)?;
    let (head, rest) = items.split_first().ok_or(ParseError::EmptyList)?;
    let head_sym = head
        .as_symbol()
        .ok_or_else(|| ParseError::ExpectedSymbol(format!("{head}")))?;

    let (positional, keywords) = split_args(rest)?;

    match head_sym {
        "box" => parse_box(&positional, &keywords),
        "cylinder" => parse_cylinder(&positional, &keywords),
        "cone" => parse_cone(&positional, &keywords),
        "wedge" => parse_wedge(&positional, &keywords),
        "sphere" => parse_sphere(&positional, &keywords),
        "lathe" => parse_lathe(&positional, &keywords),
        "extrude" => parse_extrude(&positional, &keywords),
        "torus" => parse_torus(&positional, &keywords),
        "sweep" => parse_sweep(&positional, &keywords),
        "composition" => parse_composition(&positional, &keywords),
        "translate" => parse_translate(&positional, &keywords),
        "rotate" => parse_rotate(&positional, &keywords),
        "scale" => parse_scale(&positional, &keywords),
        "mirror" => parse_mirror(&positional, &keywords),
        "array" => parse_array(&positional, &keywords),
        "union" => parse_union(&positional, &keywords),
        "intersection" => parse_intersection(&positional, &keywords),
        "difference" => parse_difference(&positional, &keywords),
        other => Err(ParseError::UnknownNode(other.to_string())),
    }
}

fn list_to_vec(value: &Value) -> Result<Vec<&Value>, ParseError> {
    let mut out = Vec::new();
    let mut current = value;
    loop {
        match current {
            Value::Cons(c) => {
                out.push(c.car());
                current = c.cdr();
            }
            Value::Nil | Value::Null => return Ok(out),
            other => {
                if out.is_empty() {
                    return Err(ParseError::NotAList(format!("{other}")));
                } else {
                    return Err(ParseError::NotProperList);
                }
            }
        }
    }
}

type SplitArgs<'a> = (Vec<&'a Value>, Vec<(String, &'a Value)>);

fn split_args<'a>(args: &[&'a Value]) -> Result<SplitArgs<'a>, ParseError> {
    let mut positional = Vec::new();
    let mut keywords = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if let Some(kw) = args[i].as_keyword() {
            let val = args.get(i + 1).ok_or(ParseError::MissingKeyword {
                node: "<unknown>",
                keyword: "<value-after-keyword>",
            })?;
            keywords.push((kw.to_string(), *val));
            i += 2;
        } else {
            positional.push(args[i]);
            i += 1;
        }
    }
    Ok((positional, keywords))
}

fn expect_arity(
    node: &'static str,
    positional: &[&Value],
    expected: usize,
) -> Result<(), ParseError> {
    if positional.len() != expected {
        Err(ParseError::WrongArity {
            node,
            expected,
            got: positional.len(),
        })
    } else {
        Ok(())
    }
}

fn require_color(node: &'static str, keywords: &[(String, &Value)]) -> Result<u32, ParseError> {
    for (k, v) in keywords {
        if k == "color" {
            return as_u32(v);
        }
    }
    Err(ParseError::MissingKeyword {
        node,
        keyword: "color",
    })
}

fn check_no_extra_keywords(
    node: &'static str,
    keywords: &[(String, &Value)],
    allowed: &[&str],
) -> Result<(), ParseError> {
    for (k, _) in keywords {
        if !allowed.contains(&k.as_str()) {
            return Err(ParseError::UnknownKeyword {
                node,
                keyword: k.clone(),
            });
        }
    }
    Ok(())
}

fn as_f32(value: &Value) -> Result<f32, ParseError> {
    value
        .as_f64()
        .map(|n| n as f32)
        .ok_or_else(|| ParseError::ExpectedNumber(format!("{value}")))
}

fn as_u32(value: &Value) -> Result<u32, ParseError> {
    let n = value
        .as_u64()
        .ok_or_else(|| ParseError::ExpectedInteger(format!("{value}")))?;
    u32::try_from(n).map_err(|_| ParseError::ExpectedInteger(format!("{value} (out of u32 range)")))
}

fn as_bool(value: &Value) -> Result<bool, ParseError> {
    // Accept the lexpr-native `Bool` (Scheme `#t` / `#f`) plus the
    // bare symbols `true` / `false` so the DSL stays human-friendly.
    if let Some(b) = value.as_bool() {
        return Ok(b);
    }
    if let Some(sym) = value.as_symbol() {
        match sym {
            "true" => return Ok(true),
            "false" => return Ok(false),
            _ => {}
        }
    }
    Err(ParseError::ExpectedBool(format!("{value}")))
}

fn as_vec3(node: &'static str, value: &Value) -> Result<Vec3, ParseError> {
    let items = list_to_vec(value)?;
    if items.len() != 3 {
        return Err(ParseError::WrongVectorLength {
            node,
            expected: 3,
            got: items.len(),
        });
    }
    Ok(Vec3::new(
        as_f32(items[0])?,
        as_f32(items[1])?,
        as_f32(items[2])?,
    ))
}

fn as_profile(value: &Value) -> Result<Vec<[f32; 2]>, ParseError> {
    let points = list_to_vec(value)?;
    let mut out = Vec::with_capacity(points.len());
    for p in points {
        let coords = list_to_vec(p)?;
        if coords.len() != 2 {
            return Err(ParseError::InvalidProfilePoint(coords.len()));
        }
        out.push([as_f32(coords[0])?, as_f32(coords[1])?]);
    }
    Ok(out)
}

fn parse_box(positional: &[&Value], keywords: &[(String, &Value)]) -> Result<Node, ParseError> {
    expect_arity("box", positional, 3)?;
    check_no_extra_keywords("box", keywords, &["color"])?;
    Ok(Node::Box {
        x: as_f32(positional[0])?,
        y: as_f32(positional[1])?,
        z: as_f32(positional[2])?,
        color: require_color("box", keywords)?,
    })
}

fn parse_cylinder(
    positional: &[&Value],
    keywords: &[(String, &Value)],
) -> Result<Node, ParseError> {
    expect_arity("cylinder", positional, 3)?;
    check_no_extra_keywords("cylinder", keywords, &["color"])?;
    Ok(Node::Cylinder {
        radius: as_f32(positional[0])?,
        height: as_f32(positional[1])?,
        segments: as_u32(positional[2])?,
        color: require_color("cylinder", keywords)?,
    })
}

fn parse_cone(positional: &[&Value], keywords: &[(String, &Value)]) -> Result<Node, ParseError> {
    expect_arity("cone", positional, 3)?;
    check_no_extra_keywords("cone", keywords, &["color"])?;
    Ok(Node::Cone {
        radius: as_f32(positional[0])?,
        height: as_f32(positional[1])?,
        segments: as_u32(positional[2])?,
        color: require_color("cone", keywords)?,
    })
}

fn parse_wedge(positional: &[&Value], keywords: &[(String, &Value)]) -> Result<Node, ParseError> {
    expect_arity("wedge", positional, 3)?;
    check_no_extra_keywords("wedge", keywords, &["color"])?;
    Ok(Node::Wedge {
        x: as_f32(positional[0])?,
        y: as_f32(positional[1])?,
        z: as_f32(positional[2])?,
        color: require_color("wedge", keywords)?,
    })
}

fn parse_sphere(positional: &[&Value], keywords: &[(String, &Value)]) -> Result<Node, ParseError> {
    expect_arity("sphere", positional, 2)?;
    check_no_extra_keywords("sphere", keywords, &["color"])?;
    Ok(Node::Sphere {
        radius: as_f32(positional[0])?,
        subdivisions: as_u32(positional[1])?,
        color: require_color("sphere", keywords)?,
    })
}

fn parse_lathe(positional: &[&Value], keywords: &[(String, &Value)]) -> Result<Node, ParseError> {
    expect_arity("lathe", positional, 2)?;
    check_no_extra_keywords("lathe", keywords, &["color"])?;
    Ok(Node::Lathe {
        profile: as_profile(positional[0])?,
        segments: as_u32(positional[1])?,
        color: require_color("lathe", keywords)?,
    })
}

fn parse_extrude(positional: &[&Value], keywords: &[(String, &Value)]) -> Result<Node, ParseError> {
    expect_arity("extrude", positional, 2)?;
    check_no_extra_keywords("extrude", keywords, &["color"])?;
    Ok(Node::Extrude {
        profile: as_profile(positional[0])?,
        depth: as_f32(positional[1])?,
        color: require_color("extrude", keywords)?,
    })
}

fn parse_torus(positional: &[&Value], keywords: &[(String, &Value)]) -> Result<Node, ParseError> {
    expect_arity("torus", positional, 4)?;
    check_no_extra_keywords("torus", keywords, &["color"])?;
    Ok(Node::Torus {
        major_radius: as_f32(positional[0])?,
        minor_radius: as_f32(positional[1])?,
        major_segments: as_u32(positional[2])?,
        minor_segments: as_u32(positional[3])?,
        color: require_color("torus", keywords)?,
    })
}

fn parse_sweep(positional: &[&Value], keywords: &[(String, &Value)]) -> Result<Node, ParseError> {
    expect_arity("sweep", positional, 2)?;
    check_no_extra_keywords("sweep", keywords, &["color", "scales", "open"])?;
    let scales = keywords
        .iter()
        .find(|(k, _)| k == "scales")
        .map(|(_, v)| as_scalar_list(v))
        .transpose()?;
    let path = as_path(positional[1])?;
    if let Some(s) = scales.as_ref()
        && s.len() != path.len()
    {
        return Err(ParseError::SweepScalesLengthMismatch {
            scales_len: s.len(),
            path_len: path.len(),
        });
    }
    let open = keywords
        .iter()
        .find(|(k, _)| k == "open")
        .map(|(_, v)| as_bool(v))
        .transpose()?
        .unwrap_or(false);
    Ok(Node::Sweep {
        profile: as_profile(positional[0])?,
        path,
        scales,
        open,
        color: require_color("sweep", keywords)?,
    })
}

fn as_scalar_list(value: &Value) -> Result<Vec<f32>, ParseError> {
    let items = list_to_vec(value)?;
    items.iter().map(|v| as_f32(v)).collect()
}

fn as_path(value: &Value) -> Result<Vec<Vec3>, ParseError> {
    let points = list_to_vec(value)?;
    let mut out = Vec::with_capacity(points.len());
    for p in points {
        out.push(as_vec3("sweep path waypoint", p)?);
    }
    Ok(out)
}

fn parse_composition(
    positional: &[&Value],
    keywords: &[(String, &Value)],
) -> Result<Node, ParseError> {
    check_no_extra_keywords("composition", keywords, &[])?;
    let children = positional
        .iter()
        .map(|v| parse_node(v))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Node::Composition(children))
}

fn parse_translate(
    positional: &[&Value],
    keywords: &[(String, &Value)],
) -> Result<Node, ParseError> {
    expect_arity("translate", positional, 2)?;
    check_no_extra_keywords("translate", keywords, &[])?;
    Ok(Node::Translate {
        offset: as_vec3("translate", positional[0])?,
        child: std::boxed::Box::new(parse_node(positional[1])?),
    })
}

fn parse_rotate(positional: &[&Value], keywords: &[(String, &Value)]) -> Result<Node, ParseError> {
    expect_arity("rotate", positional, 3)?;
    check_no_extra_keywords("rotate", keywords, &[])?;
    Ok(Node::Rotate {
        axis: as_vec3("rotate", positional[0])?,
        angle: as_f32(positional[1])?,
        child: std::boxed::Box::new(parse_node(positional[2])?),
    })
}

fn parse_scale(positional: &[&Value], keywords: &[(String, &Value)]) -> Result<Node, ParseError> {
    expect_arity("scale", positional, 2)?;
    check_no_extra_keywords("scale", keywords, &[])?;
    Ok(Node::Scale {
        factor: as_vec3("scale", positional[0])?,
        child: std::boxed::Box::new(parse_node(positional[1])?),
    })
}

fn parse_mirror(positional: &[&Value], keywords: &[(String, &Value)]) -> Result<Node, ParseError> {
    expect_arity("mirror", positional, 2)?;
    check_no_extra_keywords("mirror", keywords, &[])?;
    let axis_sym = positional[0]
        .as_symbol()
        .ok_or_else(|| ParseError::ExpectedSymbolValue(format!("{}", positional[0])))?;
    let axis =
        Axis::from_symbol(axis_sym).ok_or_else(|| ParseError::InvalidAxis(axis_sym.to_string()))?;
    Ok(Node::Mirror {
        axis,
        child: std::boxed::Box::new(parse_node(positional[1])?),
    })
}

fn parse_array(positional: &[&Value], keywords: &[(String, &Value)]) -> Result<Node, ParseError> {
    expect_arity("array", positional, 3)?;
    check_no_extra_keywords("array", keywords, &[])?;
    Ok(Node::Array {
        count: as_u32(positional[0])?,
        spacing: as_vec3("array", positional[1])?,
        child: std::boxed::Box::new(parse_node(positional[2])?),
    })
}

fn parse_union(positional: &[&Value], keywords: &[(String, &Value)]) -> Result<Node, ParseError> {
    check_no_extra_keywords("union", keywords, &[])?;
    if positional.len() < 2 {
        return Err(ParseError::CsgArityTooFew {
            node: "union",
            min: 2,
            got: positional.len(),
        });
    }
    let children = positional
        .iter()
        .map(|v| parse_node(v))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Node::Union { children })
}

fn parse_intersection(
    positional: &[&Value],
    keywords: &[(String, &Value)],
) -> Result<Node, ParseError> {
    check_no_extra_keywords("intersection", keywords, &[])?;
    if positional.len() < 2 {
        return Err(ParseError::CsgArityTooFew {
            node: "intersection",
            min: 2,
            got: positional.len(),
        });
    }
    let children = positional
        .iter()
        .map(|v| parse_node(v))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Node::Intersection { children })
}

fn parse_difference(
    positional: &[&Value],
    keywords: &[(String, &Value)],
) -> Result<Node, ParseError> {
    check_no_extra_keywords("difference", keywords, &[])?;
    if positional.len() < 2 {
        return Err(ParseError::CsgArityTooFew {
            node: "difference",
            min: 2,
            got: positional.len(),
        });
    }
    let base = parse_node(positional[0])?;
    let subtract = positional[1..]
        .iter()
        .map(|v| parse_node(v))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Node::Difference {
        base: std::boxed::Box::new(base),
        subtract,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `as_u32` accepts values up to `u32::MAX`. Pinned by issue #362
    /// to keep the boundary case from regressing if the conversion is
    /// ever rewritten.
    #[test]
    fn as_u32_accepts_u32_max() {
        let dsl = format!("(box 1 1 1 :color {})", u32::MAX);
        parse(&dsl).expect("u32::MAX must parse successfully");
    }

    /// Values one past `u32::MAX` previously wrapped silently
    /// (`n as u32` is a wrapping cast); per issue #362 they must now
    /// surface a parse error rather than producing a different mesh.
    #[test]
    fn as_u32_rejects_values_above_u32_max() {
        let dsl = format!("(box 1 1 1 :color {})", u32::MAX as u64 + 1);
        let err = parse(&dsl).expect_err("values above u32::MAX must error");
        match err {
            ParseError::ExpectedInteger(msg) => {
                assert!(
                    msg.contains("out of u32 range"),
                    "expected range diagnostic, got: {msg}"
                );
            }
            other => panic!("expected ExpectedInteger, got {other:?}"),
        }
    }
}
