// Source-engine camera maths, shared by the free-fly 3D view and the first-person view
// (`s6f3b_viewer3d.md` F3b-1b/F3b-1c). Game units (inches), Z-up, degrees in - everything here is
// a pure function; no DOM, no three.js state.

// `sim/src/throw.rs`'s `STAND_EYE_HEIGHT`/`CROUCH_EYE_HEIGHT` - eyes above the feet.
export const STAND_EYE_HEIGHT = 64.06;
export const CROUCH_EYE_HEIGHT = 46.04;

// Player collision hull height (`s6f3b_viewer3d.md`: "капсула 72 стоя / 54 присев").
export const STAND_HULL_HEIGHT = 72;
export const CROUCH_HULL_HEIGHT = 54;
const PLAYER_RADIUS = 16;

// CS2's `fov_desired` is a horizontal FOV specified at a 4:3 reference aspect ratio, then the
// engine holds the VERTICAL FOV fixed and widens the horizontal one for wider aspects ("Vert-"
// scaling) - so the vertical FOV is the aspect-independent quantity a `PerspectiveCamera` (whose
// own `fov` parameter is vertical) should be given. At `fov 90` (4:3): tan(vfov/2) =
// tan(45deg)/(4/3) = 0.75, so vfov = 2*atan(0.75) = 73.7398 deg (matches the spec's "73.74° при
// любом соотношении сторон", and 2*atan(0.75*16/9... ) below matches its "106.26° по горизонтали
// на 16:9" cross-check).
export const VERTICAL_FOV_DEG = (2 * Math.atan(0.75) * 180) / Math.PI;

/**
 * Source's `AngleVectors` (roll = 0): forward/right/up unit vectors for a `pitch`/`yaw` pair in
 * degrees, with Source's convention that positive pitch looks DOWN (`forward.z = -sin(pitch)`).
 * Verified against the spec's own unit check: pitch -69.6, yaw -125.5 must give forward =
 * (cos(p)*cos(y), cos(p)*sin(y), -sin(p)).
 */
export function sourceBasis(pitchDeg, yawDeg) {
  const p = (pitchDeg * Math.PI) / 180;
  const y = (yawDeg * Math.PI) / 180;
  const cp = Math.cos(p);
  const sp = Math.sin(p);
  const cy = Math.cos(y);
  const sy = Math.sin(y);
  return {
    forward: { x: cp * cy, y: cp * sy, z: -sp },
    // roll = 0 case of AngleVectors's `right`/`up`.
    right: { x: sy, y: -cy, z: 0 },
    up: { x: sp * cy, y: sp * sy, z: cp },
  };
}

export function eyeHeight(crouched) {
  return crouched ? CROUCH_EYE_HEIGHT : STAND_EYE_HEIGHT;
}

export function hullHeight(crouched) {
  return crouched ? CROUCH_HULL_HEIGHT : STAND_HULL_HEIGHT;
}

export const PLAYER_CAPSULE_RADIUS = PLAYER_RADIUS;
