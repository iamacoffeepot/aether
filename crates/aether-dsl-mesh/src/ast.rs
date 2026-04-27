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
    /// One angular slice of a [`Node::Lathe`] revolved into a closed
    /// solid: a wedge bounded by two radial walls (at the slice's start
    /// and end angles) and the lathe's outer surface restricted to the
    /// slice's angular range. Internal-only — emitted by the wedge-
    /// decomposition rewrite in [`crate::simplify`] when a lathe's
    /// profile is axis-closed (first and last profile points have
    /// `x == 0`). Not parseable from DSL source.
    ///
    /// `segment_index` ranges over `0..segments`; the slice covers
    /// `[segment_index · 2π/segments, (segment_index+1) · 2π/segments]`.
    LatheSegment {
        profile: Vec<[f32; 2]>,
        segments: u32,
        segment_index: u32,
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

    // Boolean (CSG) operators per ADR-0054. The mesher routes these
    // through `crate::csg`; the v1 algorithm is BSP-CSG with internal
    // fixed-point predicates.
    /// N-ary union: result is the union of all children's solid regions.
    /// Requires at least two children (one-child union is a parse error).
    Union {
        children: Vec<Node>,
    },
    /// N-ary intersection: result is the intersection of all children's
    /// solid regions. Requires at least two children. Empty-result is a
    /// valid empty mesh, not an error.
    Intersection {
        children: Vec<Node>,
    },
    /// First child minus the union of the remaining children. Requires
    /// `base` plus at least one subtractor.
    Difference {
        base: std::boxed::Box<Node>,
        subtract: Vec<Node>,
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

    /// Component index of this axis (`X = 0, Y = 1, Z = 2`). Useful as
    /// the bridge between the typed enum and the index-keyed APIs in
    /// `aether_math` (e.g. `Aabb::mirror`).
    pub fn index(self) -> usize {
        match self {
            Axis::X => 0,
            Axis::Y => 1,
            Axis::Z => 2,
        }
    }
}
