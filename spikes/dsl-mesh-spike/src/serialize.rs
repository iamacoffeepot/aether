//! Serialize a typed mesh AST back to Lisp s-expression text.
//!
//! Round-trip property: `parse(serialize(ast)) == ast`. Whitespace and
//! formatting are not preserved — the serializer emits a canonical form.

use lexpr::{Cons, Number, Value};

use crate::ast::Node;

pub fn serialize(node: &Node) -> String {
    let value = node_to_value(node);
    let opts = lexpr::print::Options::default()
        .with_keyword_syntax(lexpr::parse::KeywordSyntax::ColonPrefix);
    let mut buf = Vec::new();
    lexpr::print::to_writer_custom(&mut buf, &value, opts).expect("writing to Vec never fails");
    String::from_utf8(buf).expect("lexpr emits utf-8")
}

pub fn node_to_value(node: &Node) -> Value {
    match node {
        Node::Box { x, y, z, color } => list([
            sym("box"),
            num(*x),
            num(*y),
            num(*z),
            kw("color"),
            uint(*color),
        ]),
        Node::Cylinder {
            radius,
            height,
            segments,
            color,
        } => list([
            sym("cylinder"),
            num(*radius),
            num(*height),
            uint(*segments),
            kw("color"),
            uint(*color),
        ]),
        Node::Cone {
            radius,
            height,
            segments,
            color,
        } => list([
            sym("cone"),
            num(*radius),
            num(*height),
            uint(*segments),
            kw("color"),
            uint(*color),
        ]),
        Node::Wedge { x, y, z, color } => list([
            sym("wedge"),
            num(*x),
            num(*y),
            num(*z),
            kw("color"),
            uint(*color),
        ]),
        Node::Sphere {
            radius,
            subdivisions,
            color,
        } => list([
            sym("sphere"),
            num(*radius),
            uint(*subdivisions),
            kw("color"),
            uint(*color),
        ]),
        Node::Lathe {
            profile,
            segments,
            color,
        } => list([
            sym("lathe"),
            profile_to_value(profile),
            uint(*segments),
            kw("color"),
            uint(*color),
        ]),
        Node::Extrude {
            profile,
            depth,
            color,
        } => list([
            sym("extrude"),
            profile_to_value(profile),
            num(*depth),
            kw("color"),
            uint(*color),
        ]),
        Node::Composition(children) => {
            let mut items = vec![sym("composition")];
            items.extend(children.iter().map(node_to_value));
            list(items)
        }
        Node::Translate { offset, child } => list([
            sym("translate"),
            vec3_to_value(*offset),
            node_to_value(child),
        ]),
        Node::Rotate { axis, angle, child } => list([
            sym("rotate"),
            vec3_to_value(*axis),
            num(*angle),
            node_to_value(child),
        ]),
        Node::Scale { factor, child } => {
            list([sym("scale"), vec3_to_value(*factor), node_to_value(child)])
        }
        Node::Mirror { axis, child } => {
            list([sym("mirror"), sym(axis.as_symbol()), node_to_value(child)])
        }
        Node::Array {
            count,
            spacing,
            child,
        } => list([
            sym("array"),
            uint(*count),
            vec3_to_value(*spacing),
            node_to_value(child),
        ]),
    }
}

fn list<I: IntoIterator<Item = Value>>(items: I) -> Value {
    let items: Vec<Value> = items.into_iter().collect();
    let mut tail = Value::Null;
    for item in items.into_iter().rev() {
        tail = Value::Cons(Cons::new(item, tail));
    }
    tail
}

fn sym(s: &str) -> Value {
    Value::Symbol(s.into())
}

fn kw(s: &str) -> Value {
    Value::Keyword(s.into())
}

fn num(f: f32) -> Value {
    Value::Number(Number::from_f64(f as f64).expect("non-finite float in AST"))
}

fn uint(n: u32) -> Value {
    Value::Number(Number::from(n))
}

fn vec3_to_value(v: [f32; 3]) -> Value {
    list([num(v[0]), num(v[1]), num(v[2])])
}

fn profile_to_value(p: &[[f32; 2]]) -> Value {
    list(p.iter().map(|pt| list([num(pt[0]), num(pt[1])])))
}
