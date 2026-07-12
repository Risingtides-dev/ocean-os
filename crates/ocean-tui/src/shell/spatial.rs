//! 3D camera, projection, and rendering primitives for the project graph.
//!
//! NOTICE — attribution (upstream correction, spec-graph3d.md): the mathematics
//! in this module is ported from **aclfe/inertia**
//! (<https://github.com/aclfe/inertia>), dual-licensed MIT OR Apache-2.0. It is
//! re-implemented in ocean-tui's style with a local, dependency-free `f64`
//! [`Vec3`] (instead of `nalgebra`) and adapted to the graph's unit-radius world
//! volume. Algorithms ported, each cited to its upstream source:
//!
//! - World → camera transform: dot-product basis change
//!   (`src/render/camera.rs`, `View::transform`).
//! - Perspective projection to NDC: focal / near divide
//!   (`src/render/projection.rs`, `Projection::project`).
//! - 2:1 terminal-cell aspect correction: [`cell_aspect`]
//!   (`src/render/mod.rs`, `Layout3D::cell_aspect = 2.0 * height / width`).
//! - Orbit / pan / zoom camera: spherical orbit with a polar clamp, dolly with a
//!   distance clamp, basis-relative pan (`src/render/camera.rs`, `Camera`).

use ratatui::layout::Rect;

// ───────────────────────── Vec3 ─────────────────────────

/// Minimal `f64` 3-vector (replaces `nalgebra::Vector3<f64>`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vec3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Vec3 {
    pub const ZERO: Vec3 = Vec3 {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };

    pub const fn new(x: f64, y: f64, z: f64) -> Self {
        Vec3 { x, y, z }
    }

    pub fn dot(self, o: Vec3) -> f64 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }

    pub fn cross(self, o: Vec3) -> Vec3 {
        Vec3::new(
            self.y * o.z - self.z * o.y,
            self.z * o.x - self.x * o.z,
            self.x * o.y - self.y * o.x,
        )
    }

    pub fn length(self) -> f64 {
        self.dot(self).sqrt()
    }

    /// Zero-safe normalization: the zero (or ~zero) vector stays zero, so the
    /// camera basis never produces NaNs from a degenerate forward direction.
    pub fn normalize(self) -> Vec3 {
        let l = self.length();
        if l < 1e-9 {
            Vec3::ZERO
        } else {
            Vec3::new(self.x / l, self.y / l, self.z / l)
        }
    }
}

impl std::ops::Add for Vec3 {
    type Output = Vec3;
    fn add(self, o: Vec3) -> Vec3 {
        Vec3::new(self.x + o.x, self.y + o.y, self.z + o.z)
    }
}
impl std::ops::Sub for Vec3 {
    type Output = Vec3;
    fn sub(self, o: Vec3) -> Vec3 {
        Vec3::new(self.x - o.x, self.y - o.y, self.z - o.z)
    }
}
impl std::ops::Neg for Vec3 {
    type Output = Vec3;
    fn neg(self) -> Vec3 {
        Vec3::new(-self.x, -self.y, -self.z)
    }
}
impl std::ops::Mul<f64> for Vec3 {
    type Output = Vec3;
    fn mul(self, s: f64) -> Vec3 {
        Vec3::new(self.x * s, self.y * s, self.z * s)
    }
}

const WORLD_UP: Vec3 = Vec3::new(0.0, 1.0, 0.0);
/// Polar clamp: the camera can never look straight along WORLD_UP, so
/// `forward × WORLD_UP` stays non-degenerate (mirrors aclfe/inertia).
const MIN_PHI: f64 = 0.05;
const MAX_PHI: f64 = std::f64::consts::PI - 0.05;
/// Distance bounds tightened from inertia's `[2.0, 60.0]` to frame the graph's
/// normalized unit-radius volume instead of a physics world.
const MIN_DISTANCE: f64 = 1.5;
const MAX_DISTANCE: f64 = 14.0;

/// Default vertical field of view, degrees (matches aclfe/inertia's default).
pub const FOV_Y_DEG: f64 = 60.0;

/// Spherical orbit camera around a `target`. Ported from aclfe/inertia
/// `src/render/camera.rs::Camera`.
#[derive(Clone, Copy, Debug)]
pub struct Camera {
    pub theta: f64,
    pub phi: f64,
    pub distance: f64,
    pub target: Vec3,
}

impl Default for Camera {
    fn default() -> Self {
        Self::new()
    }
}

impl Camera {
    /// Default camera frames the unit-radius graph volume with margin: the
    /// normalized constellation (max Euclidean radius = 1) projects to roughly
    /// the inner half of NDC at this distance for common pane aspect ratios.
    pub fn new() -> Self {
        Self {
            theta: std::f64::consts::FRAC_PI_4,
            phi: std::f64::consts::FRAC_PI_3,
            distance: 4.5,
            target: Vec3::ZERO,
        }
    }

    /// Camera position in world space.
    pub fn eye(&self) -> Vec3 {
        self.target
            + Vec3::new(
                self.phi.sin() * self.theta.cos(),
                self.phi.cos(),
                self.phi.sin() * self.theta.sin(),
            ) * self.distance
    }

    /// Orbit by `dtheta` (yaw) and `dphi` (pitch). Pitch is clamped so the
    /// camera can never flip through either pole.
    pub fn orbit(&mut self, dtheta: f64, dphi: f64) {
        self.theta += dtheta;
        self.phi = (self.phi + dphi).clamp(MIN_PHI, MAX_PHI);
    }

    /// Dolly in/out by `factor`, clamped to safe framing bounds.
    pub fn zoom(&mut self, factor: f64) {
        self.distance = (self.distance * factor).clamp(MIN_DISTANCE, MAX_DISTANCE);
    }

    /// Pan the target along the current camera basis (`dright`, `dup`).
    pub fn pan(&mut self, dright: f64, dup: f64) {
        let (right, up, _) = self.basis();
        self.target = self.target + right * dright + up * dup;
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Camera basis `(right, up, forward)`. Robust when `forward` is parallel
    /// to `WORLD_UP` (falls back to the X axis to rebuild `right`), so a
    /// degenerate configuration cannot produce NaNs.
    pub fn basis(&self) -> (Vec3, Vec3, Vec3) {
        let forward = (self.target - self.eye()).normalize();
        let mut right = forward.cross(WORLD_UP);
        if right.length() < 1e-6 {
            right = forward.cross(Vec3::new(1.0, 0.0, 0.0));
        }
        let right = right.normalize();
        let up = right.cross(forward).normalize();
        (right, up, forward)
    }

    pub fn view(&self) -> View {
        let (right, up, forward) = self.basis();
        View {
            eye: self.eye(),
            right,
            up,
            forward,
        }
    }
}

/// World → camera-space transform. Ported from `camera.rs::View`.
#[derive(Clone, Copy, Debug)]
pub struct View {
    pub eye: Vec3,
    pub right: Vec3,
    pub up: Vec3,
    pub forward: Vec3,
}

impl View {
    /// Express `world` in right/up/forward camera coordinates.
    pub fn transform(&self, world: Vec3) -> Vec3 {
        let rel = world - self.eye;
        Vec3::new(rel.dot(self.right), rel.dot(self.up), rel.dot(self.forward))
    }

    /// Unconsumed accessor: the renderer reads the basis through
    /// `Camera::view()` + [`View::transform`], and the field itself is `pub`.
    /// Kept as landed API surface.
    #[allow(dead_code)]
    pub fn right(&self) -> Vec3 {
        self.right
    }
}

/// Perspective projection to normalized device coordinates. Ported from
/// `src/render/projection.rs::Projection`.
#[derive(Clone, Copy, Debug)]
pub struct Projection {
    pub focal: f64,
    pub near: f64,
}

impl Projection {
    pub fn new(fov_y_degrees: f64) -> Self {
        let fov_y = fov_y_degrees.to_radians();
        Self {
            focal: 1.0 / (fov_y / 2.0).tan(),
            near: 0.1,
        }
    }

    /// Perspective divide. `None` for points at/behind the near plane — these
    /// are rejected by the renderer rather than drawn.
    pub fn project(&self, view: Vec3) -> Option<(f64, f64)> {
        if view.z <= self.near {
            return None;
        }
        Some((self.focal * view.x / view.z, self.focal * view.y / view.z))
    }
}

impl Default for Projection {
    fn default() -> Self {
        Self::new(FOV_Y_DEG)
    }
}

/// 2:1 terminal-cell aspect correction: the canvas Y extent so braille cells
/// keep their intended aspect ratio. Ported from `src/render/mod.rs`
/// (`Layout3D::cell_aspect = 2.0 * height / width`).
pub fn cell_aspect(area: Rect) -> f64 {
    if area.width == 0 {
        1.0
    } else {
        2.0 * area.height.max(1) as f64 / area.width.max(1) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn vec3_cross_dot_normalize() {
        let i = Vec3::new(1.0, 0.0, 0.0);
        let j = Vec3::new(0.0, 1.0, 0.0);
        let k = Vec3::new(0.0, 0.0, 1.0);
        assert!(approx(i.cross(j).dot(k), 1.0));
        assert!(approx(Vec3::new(0.0, 5.0, 0.0).normalize().length(), 1.0));
        // zero-safe normalization never yields NaNs
        assert_eq!(Vec3::ZERO.normalize(), Vec3::ZERO);
    }

    #[test]
    fn view_axis_projects_to_ndc_origin_with_positive_depth() {
        let cam = Camera::new(); // target == origin
        let view = cam.view();
        let v = view.transform(Vec3::ZERO); // the target point
        assert!(approx(v.x, 0.0));
        assert!(approx(v.y, 0.0));
        assert!(approx(v.z, cam.distance)); // positive depth == distance
        let (px, py) = Projection::default()
            .project(v)
            .expect("target is in front of the near plane");
        assert!(approx(px, 0.0));
        assert!(approx(py, 0.0));
    }

    #[test]
    fn behind_and_near_plane_rejected_and_perspective_scales() {
        let p = Projection::default();
        assert!(p.project(Vec3::new(0.0, 0.0, -1.0)).is_none()); // behind
        assert!(p.project(Vec3::new(1.0, 1.0, 0.0)).is_none()); // at plane
                                                                // halving the depth halves the NDC extent (perspective divide)
        let near = p.project(Vec3::new(2.0, 0.0, 1.0)).unwrap().0;
        let far = p.project(Vec3::new(2.0, 0.0, 2.0)).unwrap().0;
        assert!(approx(near, 2.0 * far));
    }

    #[test]
    fn orbit_clamps_phi_to_poles() {
        let mut c = Camera::new();
        c.orbit(0.0, -100.0); // try to pitch past the north pole
        assert!(c.phi >= MIN_PHI);
        c.orbit(0.0, 100.0); // try to pitch past the south pole
        assert!(c.phi <= MAX_PHI);
    }

    #[test]
    fn zoom_clamps_distance() {
        let mut c = Camera::new();
        c.zoom(1e6);
        assert_eq!(c.distance, MAX_DISTANCE);
        c.zoom(1e-6);
        assert_eq!(c.distance, MIN_DISTANCE);
    }

    #[test]
    fn pan_moves_target_along_basis() {
        let mut c = Camera::new();
        let before = c.target;
        c.pan(1.0, 0.0);
        assert!(!approx((c.target - before).length(), 0.0));
    }

    #[test]
    fn cell_aspect_is_two_to_one() {
        assert!(approx(cell_aspect(Rect::new(0, 0, 10, 5)), 1.0)); // 2*5/10
        assert!(approx(cell_aspect(Rect::new(0, 0, 20, 5)), 0.5)); // wide
        assert!(approx(cell_aspect(Rect::new(0, 0, 0, 5)), 1.0)); // zero-width guard
    }
}
