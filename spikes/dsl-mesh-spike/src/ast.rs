//! Typed mesh AST for the v1 vocabulary committed by ADR-0026, plus
//! the v2 additions the spike commits to ahead of an ADR amendment:
//! `torus` (handles, rings) and `sweep` (curved tubes — spouts).
//!
//! Sweep-along-path is on ADR-0026's parked v2 list; torus is not in
//! the ADR yet and would land as a v2 vocabulary extension. Both are
//! gated behind the spike — promotion to a real crate should sync
//! the ADR first.

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

    // v2 vocabulary (spike-committed; ADR amendment pending)
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
