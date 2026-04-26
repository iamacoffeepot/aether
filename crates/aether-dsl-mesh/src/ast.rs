//! Typed mesh AST for the v1 vocabulary defined by ADR-0026 and
//! formalized in ADR-0051. All variants here have a parser arm in
//! `parse.rs` and a mesher arm in `mesh.rs`.

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

    /// Donut-shaped surface around the Y axis. `major_radius` is the
    /// distance from the torus center to the center of the tube;
    /// `minor_radius` is the tube's radius. `major_segments` divides
    /// the big loop, `minor_segments` divides the tube cross-section.
    Torus {
        major_radius: f32,
        minor_radius: f32,
        major_segments: u32,
        minor_segments: u32,
        color: u32,
    },
    /// Sweep a 2D `profile` polygon along a 3D polyline `path`. At each
    /// path waypoint the profile is oriented perpendicular to the local
    /// tangent (parallel-transport-ish — see mesher for exact framing).
    /// Adjacent profile rings are stitched into quads (triangulated).
    ///
    /// Optional `scales` is a per-waypoint scalar multiplier applied to
    /// the profile (length must match `path` length). `None` is
    /// equivalent to all-ones — uniform tube along the path. Use this
    /// to taper a swept tube toward its tip.
    Sweep {
        profile: Vec<[f32; 2]>,
        path: Vec<[f32; 3]>,
        scales: Option<Vec<f32>>,
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
