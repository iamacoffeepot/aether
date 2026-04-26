//! Typed mesh AST for the v1 vocabulary committed by ADR-0026.

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    // Primitives
    Box {
        x: f32,
        y: f32,
        z: f32,
        color: u32,
    },
    Cylinder {
        radius: f32,
        height: f32,
        segments: u32,
        color: u32,
    },
    Cone {
        radius: f32,
        height: f32,
        segments: u32,
        color: u32,
    },
    Wedge {
        x: f32,
        y: f32,
        z: f32,
        color: u32,
    },
    Sphere {
        radius: f32,
        subdivisions: u32,
        color: u32,
    },

    // Profile operations
    Lathe {
        profile: Vec<[f32; 2]>,
        segments: u32,
        color: u32,
    },
    Extrude {
        profile: Vec<[f32; 2]>,
        depth: f32,
        color: u32,
    },

    // Structural
    Composition(Vec<Node>),
    Translate {
        offset: [f32; 3],
        child: std::boxed::Box<Node>,
    },
    Rotate {
        axis: [f32; 3],
        angle: f32,
        child: std::boxed::Box<Node>,
    },
    Scale {
        factor: [f32; 3],
        child: std::boxed::Box<Node>,
    },
    Mirror {
        axis: Axis,
        child: std::boxed::Box<Node>,
    },
    Array {
        count: u32,
        spacing: [f32; 3],
        child: std::boxed::Box<Node>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    X,
    Y,
    Z,
}

impl Axis {
    pub fn as_symbol(self) -> &'static str {
        match self {
            Axis::X => "x",
            Axis::Y => "y",
            Axis::Z => "z",
        }
    }

    pub fn from_symbol(sym: &str) -> Option<Self> {
        match sym {
            "x" => Some(Axis::X),
            "y" => Some(Axis::Y),
            "z" => Some(Axis::Z),
            _ => None,
        }
    }
}
