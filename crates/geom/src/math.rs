//! Minimal `f32` 3-vector and axis-aligned-box arithmetic, matching the
//! reference C# `System.Numerics.Vector3` operation order exactly: `dot(a,b)
//! = a.x*b.x + a.y*b.y + a.z*b.z` (no FMA), `normalize(v) = v /
//! sqrt(dot(v,v))`. No external math crate, by design (Stage 3 spec).

use std::ops::{Add, Div, Mul, Neg, Sub};

/// A `Copy` 3-component `f32` vector, standing in for `System.Numerics.
/// Vector3` in the ported code: every method here mirrors the reference's
/// operation order exactly (see the module doc), so callers get the same
/// rounding as the C# solver, not just the same mathematical result.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct V3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl V3 {
    /// `(0, 0, 0)`.
    pub const ZERO: V3 = V3::new(0.0, 0.0, 0.0);

    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        V3 { x, y, z }
    }

    /// `[x, y, z]` as a `V3`, e.g. from `CollisionMesh::vertices`.
    pub fn from_array(a: [f32; 3]) -> Self {
        V3::new(a[0], a[1], a[2])
    }

    pub fn to_array(self) -> [f32; 3] {
        [self.x, self.y, self.z]
    }

    /// `x*o.x + y*o.y + z*o.z`, left-to-right, no FMA - matches
    /// `Vector3.Dot`'s rounding exactly.
    pub fn dot(self, o: V3) -> f32 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }

    /// The standard 3D cross product, component order matching
    /// `Vector3.Cross`.
    pub fn cross(self, o: V3) -> V3 {
        V3::new(
            self.y * o.z - self.z * o.y,
            self.z * o.x - self.x * o.z,
            self.x * o.y - self.y * o.x,
        )
    }

    /// `dot(self, self)`.
    pub fn length_squared(self) -> f32 {
        self.dot(self)
    }

    /// `sqrt(length_squared())`.
    pub fn length(self) -> f32 {
        self.length_squared().sqrt()
    }

    /// `v / sqrt(dot(v, v))`, exactly the reference's `Vector3.Normalize`.
    pub fn normalize(self) -> V3 {
        self / self.length()
    }

    /// Componentwise minimum, matching `Vector3.Min`.
    pub fn min(self, o: V3) -> V3 {
        V3::new(self.x.min(o.x), self.y.min(o.y), self.z.min(o.z))
    }

    /// Componentwise maximum, matching `Vector3.Max`.
    pub fn max(self, o: V3) -> V3 {
        V3::new(self.x.max(o.x), self.y.max(o.y), self.z.max(o.z))
    }

    /// Componentwise absolute value.
    pub fn abs(self) -> V3 {
        V3::new(self.x.abs(), self.y.abs(), self.z.abs())
    }

    /// Whether all three components are finite (not NaN or +/-inf).
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite()
    }
}

impl Add for V3 {
    type Output = V3;
    fn add(self, o: V3) -> V3 {
        V3::new(self.x + o.x, self.y + o.y, self.z + o.z)
    }
}

impl Sub for V3 {
    type Output = V3;
    fn sub(self, o: V3) -> V3 {
        V3::new(self.x - o.x, self.y - o.y, self.z - o.z)
    }
}

impl Mul<f32> for V3 {
    type Output = V3;
    fn mul(self, s: f32) -> V3 {
        V3::new(self.x * s, self.y * s, self.z * s)
    }
}

impl Div<f32> for V3 {
    type Output = V3;
    fn div(self, s: f32) -> V3 {
        V3::new(self.x / s, self.y / s, self.z / s)
    }
}

impl Neg for V3 {
    type Output = V3;
    fn neg(self) -> V3 {
        V3::new(-self.x, -self.y, -self.z)
    }
}

/// An axis-aligned bounding box, closed on both ends (`min` and `max` are
/// themselves inside the box).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Aabb {
    pub min: V3,
    pub max: V3,
}

impl Aabb {
    /// The bounding box of three points (e.g. a triangle's vertices).
    pub fn from_points(a: V3, b: V3, c: V3) -> Self {
        Aabb {
            min: a.min(b).min(c),
            max: a.max(b).max(c),
        }
    }

    /// The smallest box containing both `self` and `o`.
    pub fn union(self, o: Aabb) -> Aabb {
        Aabb {
            min: self.min.min(o.min),
            max: self.max.max(o.max),
        }
    }

    /// Whether two boxes touch or overlap (closed-interval test, so flush
    /// contact counts as touching, matching `TriangleCollider.BoundsTouch`
    /// and the region-intersection check in the mesh region filters).
    pub fn intersects(self, o: Aabb) -> bool {
        self.max.x >= o.min.x
            && self.min.x <= o.max.x
            && self.max.y >= o.min.y
            && self.min.y <= o.max.y
            && self.max.z >= o.min.z
            && self.min.z <= o.max.z
    }
}
