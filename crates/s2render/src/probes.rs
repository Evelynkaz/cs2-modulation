//! Light probe volumes (`env_combined_light_probe_volume` entities) and per-vertex `_LPV` baking
//! for probe-lit primitives (`s6f3a4_lighting.md` change item 2).
//!
//! Ground truth: `Renderer/Renderer/World/WorldLoader.cs:919-1038` (entity fields, `bounds` is
//! `box_mins`/`box_maxs` directly, no origin folded in), `Renderer/Renderer/Scene.cs:2130-2213`
//! (binding: handshake first, else the first volume -- sorted level desc, then
//! `AtlasSize.LengthSquared()` asc -- whose world AABB contains the bounds centre, else the last
//! (global) volume), `Renderer/Renderer/SceneEnvironment/SceneLightProbe.cs:132-171`
//! (`WorldToLocalVolumeNormalized`, atlas scale/offset), `Renderer/Shaders/common/
//! lighting.lpv.slang:36-153` (`CalculateProbeSampleCoords`, `CalculateProbeIndirectCoords`,
//! `SampleProbeAmbientCube`, `SampleProbeDirectLightShadows`); cross-checked against `REPORT.md`
//! §2 on real Mirage data. One deliberate deviation from the reference (REPORT.md §2, "clamp to
//! the sub-volume, not VRF's whole-atlas half-texel"): the reference's border clamp
//! (`BorderMin`/`BorderMax`) is a half-texel from the *whole atlas* edge, which lets trilinear
//! filtering bleed a half-texel into a neighbouring volume's own cells; this bakes with the clamp
//! against each volume's *own* `AtlasSize`/`AtlasOffset` sub-rectangle instead.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use s2fmt::entities::{Entity, EntityLump, entity_transform};

use crate::entity::entity_num;
use crate::source::{Sources, compiled_path};

/// Row-major 3x3 inverse via the adjugate, `None` for a singular (zero-determinant) matrix.
fn invert3(l: [[f32; 3]; 3]) -> Option<[[f32; 3]; 3]> {
    let det = l[0][0] * (l[1][1] * l[2][2] - l[1][2] * l[2][1])
        - l[0][1] * (l[1][0] * l[2][2] - l[1][2] * l[2][0])
        + l[0][2] * (l[1][0] * l[2][1] - l[1][1] * l[2][0]);
    if det.abs() < 1e-12 {
        return None;
    }
    let d = 1.0 / det;
    Some([
        [
            (l[1][1] * l[2][2] - l[1][2] * l[2][1]) * d,
            (l[0][2] * l[2][1] - l[0][1] * l[2][2]) * d,
            (l[0][1] * l[1][2] - l[0][2] * l[1][1]) * d,
        ],
        [
            (l[1][2] * l[2][0] - l[1][0] * l[2][2]) * d,
            (l[0][0] * l[2][2] - l[0][2] * l[2][0]) * d,
            (l[0][2] * l[1][0] - l[0][0] * l[1][2]) * d,
        ],
        [
            (l[1][0] * l[2][1] - l[1][1] * l[2][0]) * d,
            (l[0][1] * l[2][0] - l[0][0] * l[2][1]) * d,
            (l[0][0] * l[1][1] - l[0][1] * l[1][0]) * d,
        ],
    ])
}

/// Inverts a row-major 3x4 affine placement (`docs/FORMATS.md` §6.6 convention: `v' = L*v + t`),
/// `None` if its linear part is singular.
pub fn invert_affine(t: &[[f32; 4]; 3]) -> Option<[[f32; 4]; 3]> {
    let l = [
        [t[0][0], t[0][1], t[0][2]],
        [t[1][0], t[1][1], t[1][2]],
        [t[2][0], t[2][1], t[2][2]],
    ];
    let inv_l = invert3(l)?;
    let c = [t[0][3], t[1][3], t[2][3]];
    let mut out = [[0f32; 4]; 3];
    for (i, row) in inv_l.iter().enumerate() {
        out[i][0] = row[0];
        out[i][1] = row[1];
        out[i][2] = row[2];
        out[i][3] = -(row[0] * c[0] + row[1] * c[1] + row[2] * c[2]);
    }
    Some(out)
}

/// Applies a row-major 3x4 affine placement to a point.
pub fn apply_affine(t: &[[f32; 4]; 3], p: [f32; 3]) -> [f32; 3] {
    [
        t[0][0] * p[0] + t[0][1] * p[1] + t[0][2] * p[2] + t[0][3],
        t[1][0] * p[0] + t[1][1] * p[1] + t[1][2] * p[2] + t[1][3],
        t[2][0] * p[0] + t[2][1] * p[1] + t[2][2] * p[2] + t[2][3],
    ]
}

/// Applies only the linear (rotation*scale) part -- for normals, which don't translate.
pub fn apply_linear(t: &[[f32; 4]; 3], v: [f32; 3]) -> [f32; 3] {
    [
        t[0][0] * v[0] + t[0][1] * v[1] + t[0][2] * v[2],
        t[1][0] * v[0] + t[1][1] * v[1] + t[1][2] * v[2],
        t[2][0] * v[0] + t[2][1] * v[1] + t[2][2] * v[2],
    ]
}

/// One `env_combined_light_probe_volume` entity, atlas-backed (lighting version 8.2 only --
/// `WorldLoader.cs:1004-1008`'s "`entity.ContainsKey("light_probe_atlas_x")`" check; a volume
/// without atlas fields is skipped by the caller).
#[derive(Debug, Clone)]
pub struct ProbeVolume {
    pub handshake: i64,
    /// `indoor_outdoor_level`; sort key, higher = more specific (`Scene.cs:2180`).
    pub level: i64,
    pub box_mins: [f32; 3],
    pub box_maxs: [f32; 3],
    /// Inverse of the entity's own `origin`/`angles`/`scales` transform.
    world_to_local: [[f32; 4]; 3],
    /// World-space AABB of the (possibly rotated) box, for the AABB-centre binding fallback
    /// (`Scene.cs:2223-2239`'s `BoundingBox.Contains`).
    world_aabb_min: [f32; 3],
    world_aabb_max: [f32; 3],
    /// `light_probe_size_x/y/z`: this volume's footprint in the atlas, in texels.
    pub atlas_size: [f32; 3],
    /// `light_probe_atlas_x/y/z`: this volume's texel offset into the atlas.
    pub atlas_offset: [f32; 3],
}

impl ProbeVolume {
    fn contains_world_point(&self, p: [f32; 3]) -> bool {
        (0..3).all(|i| p[i] >= self.world_aabb_min[i] && p[i] <= self.world_aabb_max[i])
    }

    /// `local` in `WorldLoader.cs`/`SceneLightProbe.cs` terms: `(worldToLocal(P) - box_mins) /
    /// (box_maxs - box_mins)`, clamped to `[0,1]` (the shader's own `saturate`, `lighting.lpv.
    /// slang:89`).
    fn local(&self, world_pos: [f32; 3]) -> [f32; 3] {
        let p = apply_affine(&self.world_to_local, world_pos);
        let mut out = [0f32; 3];
        for i in 0..3 {
            let size = (self.box_maxs[i] - self.box_mins[i]).max(1e-6);
            out[i] = ((p[i] - self.box_mins[i]) / size).clamp(0.0, 1.0);
        }
        out
    }
}

fn entity_num_vec3(e: &Entity, x: &str, y: &str, z: &str) -> Option<[f32; 3]> {
    Some([
        entity_num(e.get(x))? as f32,
        entity_num(e.get(y))? as f32,
        entity_num(e.get(z))? as f32,
    ])
}

/// Parses one entity into a [`ProbeVolume`], `None` if it isn't a valid atlas-backed volume.
fn parse_volume(e: &Entity) -> Option<ProbeVolume> {
    if !e
        .classname()
        .eq_ignore_ascii_case("env_combined_light_probe_volume")
    {
        return None;
    }
    let box_mins = e.get_vec3("box_mins")?;
    let box_maxs = e.get_vec3("box_maxs")?;
    let atlas_size = entity_num_vec3(
        e,
        "light_probe_size_x",
        "light_probe_size_y",
        "light_probe_size_z",
    )?;
    let atlas_offset = entity_num_vec3(
        e,
        "light_probe_atlas_x",
        "light_probe_atlas_y",
        "light_probe_atlas_z",
    )?;
    let handshake = entity_num(e.get("handshake")).unwrap_or(0.0) as i64;
    let level = entity_num(e.get("indoor_outdoor_level")).unwrap_or(0.0) as i64;

    let transform = entity_transform(e.origin(), e.angles(), e.scales());
    let world_to_local = invert_affine(&transform)?;

    let mut world_min = [f32::INFINITY; 3];
    let mut world_max = [f32::NEG_INFINITY; 3];
    for cx in [box_mins[0], box_maxs[0]] {
        for cy in [box_mins[1], box_maxs[1]] {
            for cz in [box_mins[2], box_maxs[2]] {
                let w = apply_affine(&transform, [cx, cy, cz]);
                for i in 0..3 {
                    world_min[i] = world_min[i].min(w[i]);
                    world_max[i] = world_max[i].max(w[i]);
                }
            }
        }
    }

    Some(ProbeVolume {
        handshake,
        level,
        box_mins,
        box_maxs,
        world_to_local,
        world_aabb_min: world_min,
        world_aabb_max: world_max,
        atlas_size,
        atlas_offset,
    })
}

/// Every `env_combined_light_probe_volume` across `lumps`, sorted level desc then
/// `atlas_size.length_squared()` asc (`Scene.cs:2178-2183`) -- the last entry is the fallback
/// ("global") volume. `skipped` counts entities that looked like a probe volume but were missing
/// a required field (malformed input; not fatal).
pub fn collect_volumes(lumps: &[EntityLump]) -> (Vec<ProbeVolume>, u32) {
    let mut volumes = Vec::new();
    let mut skipped = 0u32;
    for lump in lumps {
        for e in &lump.entities {
            if !e
                .classname()
                .eq_ignore_ascii_case("env_combined_light_probe_volume")
            {
                continue;
            }
            match parse_volume(e) {
                Some(v) => volumes.push(v),
                None => skipped += 1,
            }
        }
    }
    volumes.sort_by(|a, b| {
        b.level.cmp(&a.level).then_with(|| {
            let len2 = |v: &ProbeVolume| {
                v.atlas_size[0] * v.atlas_size[0]
                    + v.atlas_size[1] * v.atlas_size[1]
                    + v.atlas_size[2] * v.atlas_size[2]
            };
            len2(a).total_cmp(&len2(b))
        })
    });
    (volumes, skipped)
}

/// The first atlas-backed `lightprobetexture`/`lightprobetexture_dlshd` pair found (every volume
/// on a map shares one atlas -- REPORT.md §2, verified on Mirage's 58 volumes).
pub fn atlas_texture_paths(lumps: &[EntityLump]) -> Option<(String, String)> {
    for lump in lumps {
        for e in &lump.entities {
            if !e
                .classname()
                .eq_ignore_ascii_case("env_combined_light_probe_volume")
            {
                continue;
            }
            let irr = e.get_str("lightprobetexture").filter(|s| !s.is_empty());
            let dlshd = e
                .get_str("lightprobetexture_dlshd")
                .filter(|s| !s.is_empty());
            if let (Some(irr), Some(dlshd)) = (irr, dlshd) {
                return Some((irr.to_string(), dlshd.to_string()));
            }
        }
    }
    None
}

/// Chooses the volume a scene node binds to (`Scene.cs:2150-2213`): a nonzero `handshake` that
/// resolves wins outright; otherwise the first (highest-priority) volume whose world AABB
/// contains `fallback_point` (the node's own bounds centre); otherwise the last (global) volume.
/// `volumes` must already be [`collect_volumes`]-sorted.
pub fn bind_volume(
    volumes: &[ProbeVolume],
    handshake: i64,
    fallback_point: [f32; 3],
) -> Option<&ProbeVolume> {
    if handshake != 0
        && let Some(v) = volumes.iter().find(|v| v.handshake == handshake)
    {
        return Some(v);
    }
    volumes
        .iter()
        .find(|v| v.contains_world_point(fallback_point))
        .or_else(|| volumes.last())
}

/// Cap on how many decoded slices [`SliceAtlas`] keeps at once (§12's fix: an unbounded cache
/// made peak memory grow with map size without bound -- 377->1,020 MB on Mirage, 1,041->4,029 MB
/// on de_cache, REPORT.md-measured). Measured on de_cache (its atlas packs several dozen probe
/// volumes side by side, the worst case for thrashing): `64` bounds memory tightly (1,586 MB peak)
/// but is an 11x slowdown (41s -> 458s) from constant eviction/re-decode; `128` -> 1,738 MB / 220s;
/// `256` -> 2,034 MB / 74s, close to the unbounded original's own wall time while still capping
/// memory at roughly half of its unbounded 4,029 MB -- the chosen trade-off.
const SLICE_CACHE_CAP: usize = 256;

/// Decoded per-z-slice float data for one raw (still block-compressed on disk) 3D mip, decoded
/// lazily and cached with an LRU eviction cap (§12): the general path for a probe atlas of any
/// size (`lib.rs`'s own design note, "map exporter will read large probe-volume atlases SLICE BY
/// SLICE, never whole").
pub struct SliceAtlas {
    raw: s2tex::RawMip,
    /// Channels actually kept per texel (§12's "store RGB only"): `3` for the irradiance atlas,
    /// whose alpha nothing reads; `4` for the dlshd atlas, since an arbitrary `bakedShadowChannel`
    /// (§1, `0..=3`) can select any of its four channels.
    channels: usize,
    cache: RefCell<HashMap<u32, Rc<Vec<f32>>>>,
    /// Access order, oldest first, for the LRU eviction `cache` enforces at [`SLICE_CACHE_CAP`].
    order: RefCell<VecDeque<u32>>,
}

impl SliceAtlas {
    pub fn load(bytes: &[u8], channels: usize) -> Result<SliceAtlas, s2tex::TexError> {
        let raw = s2tex::decode_raw_mip_bytes(bytes, 0)?;
        Ok(SliceAtlas {
            raw,
            channels,
            cache: RefCell::new(HashMap::new()),
            order: RefCell::new(VecDeque::new()),
        })
    }

    pub fn width(&self) -> u32 {
        self.raw.width
    }
    pub fn height(&self) -> u32 {
        self.raw.height
    }
    pub fn depth(&self) -> u32 {
        self.raw.depth
    }

    fn touch(&self, z: u32) {
        let mut order = self.order.borrow_mut();
        if let Some(pos) = order.iter().position(|&x| x == z) {
            order.remove(pos);
        }
        order.push_back(z);
    }

    /// Decoded floats for slice `z` (`self.channels` per texel, §12), cached with LRU eviction.
    /// Branches on the source format: `decode_hdr_slice` for the float-HDR formats (the irradiance
    /// atlas' BC6H), `decode_raw_mip_slice_ldr` for everything else (the dlshd atlas' BC7, an
    /// ordinary `[0,1]` value, not HDR -- REPORT.md §2).
    fn slice(&self, z: u32) -> Option<Rc<Vec<f32>>> {
        if let Some(v) = self.cache.borrow().get(&z) {
            let v = v.clone();
            self.touch(z);
            return Some(v);
        }
        let rgba: Vec<f32> = if self.raw.format.is_high_dynamic_range() {
            s2tex::decode_hdr_slice(&self.raw, z).ok()?.rgba
        } else {
            let decoded = s2tex::decode_raw_mip_slice_ldr(&self.raw, z).ok()?;
            decoded.rgba.iter().map(|&b| f32::from(b) / 255.0).collect()
        };
        let stored = if self.channels >= 4 {
            rgba
        } else {
            rgba.as_chunks::<4>()
                .0
                .iter()
                .flat_map(|px| px[..self.channels].iter().copied())
                .collect()
        };
        let rc = Rc::new(stored);
        self.cache.borrow_mut().insert(z, rc.clone());
        self.touch(z);
        let mut order = self.order.borrow_mut();
        while order.len() > SLICE_CACHE_CAP {
            if let Some(evict) = order.pop_front() {
                self.cache.borrow_mut().remove(&evict);
            }
        }
        Some(rc)
    }

    fn texel(&self, x: u32, y: u32, z: u32) -> Option<[f32; 4]> {
        let slice = self.slice(z)?;
        let idx = ((y as usize) * (self.raw.width as usize) + x as usize) * self.channels;
        let s = slice.get(idx..idx + self.channels)?;
        let mut out = [0f32; 4];
        out[..self.channels].copy_from_slice(s);
        Some(out)
    }
}

/// Clamps a continuous texel coordinate to a sub-range `[lo, hi]` (inclusive), returning the two
/// texel indices to lerp between and the fractional weight -- REPORT.md §2's sub-volume clamp
/// (both `i0` and `i1` stay inside `[lo, hi]`, so trilinear filtering never reads a neighbouring
/// volume's cell).
fn clamp_pair(v: f32, lo: u32, hi: u32) -> (u32, u32, f32) {
    if hi <= lo {
        return (lo, lo, 0.0);
    }
    let v = v.clamp(lo as f32, hi as f32);
    let i0 = (v.floor() as u32).min(hi);
    let i1 = (i0 + 1).min(hi);
    let frac = if i1 > i0 { v - i0 as f32 } else { 0.0 };
    (i0, i1, frac)
}

/// Trilinear sample at continuous full-atlas texel coordinates, `x`/`y` clamped to `[x_lo,x_hi]`/
/// `[y_lo,y_hi]` and `z` to `[z_lo,z_hi]` (the sub-volume's own texel range on each axis).
#[allow(clippy::too_many_arguments)]
fn sample_trilinear(
    atlas: &SliceAtlas,
    x: f32,
    x_lo: u32,
    x_hi: u32,
    y: f32,
    y_lo: u32,
    y_hi: u32,
    z: f32,
    z_lo: u32,
    z_hi: u32,
) -> Option<[f32; 4]> {
    let (x0, x1, fx) = clamp_pair(x, x_lo, x_hi);
    let (y0, y1, fy) = clamp_pair(y, y_lo, y_hi);
    let (z0, z1, fz) = clamp_pair(z, z_lo, z_hi);

    let mut corners = [[0f32; 4]; 8];
    for (i, &(cx, cy, cz)) in [
        (x0, y0, z0),
        (x1, y0, z0),
        (x0, y1, z0),
        (x1, y1, z0),
        (x0, y0, z1),
        (x1, y0, z1),
        (x0, y1, z1),
        (x1, y1, z1),
    ]
    .iter()
    .enumerate()
    {
        corners[i] = atlas.texel(cx, cy, cz)?;
    }

    let lerp4 = |a: [f32; 4], b: [f32; 4], t: f32| {
        let mut out = [0f32; 4];
        for c in 0..4 {
            out[c] = a[c] + (b[c] - a[c]) * t;
        }
        out
    };
    let x00 = lerp4(corners[0], corners[1], fx);
    let x10 = lerp4(corners[2], corners[3], fx);
    let x01 = lerp4(corners[4], corners[5], fx);
    let x11 = lerp4(corners[6], corners[7], fx);
    let y0 = lerp4(x00, x10, fy);
    let y1 = lerp4(x01, x11, fy);
    Some(lerp4(y0, y1, fz))
}

/// Both atlas textures (`s6f3a4_lighting.md` change item 2), loaded once for the whole export.
pub struct ProbeAtlasTextures {
    pub irradiance: SliceAtlas,
    pub dlshd: SliceAtlas,
}

impl ProbeAtlasTextures {
    pub fn load(
        sources: &Sources,
        irr_path: &str,
        dlshd_path: &str,
    ) -> Result<ProbeAtlasTextures, String> {
        let irr_compiled = compiled_path(irr_path);
        let irr_bytes = sources
            .read(&irr_compiled)
            .ok_or_else(|| format!("{irr_compiled}: not found"))?;
        // §12: the irradiance atlas keeps RGB only (its alpha is never read); the dlshd atlas
        // keeps all 4 channels, since an arbitrary `bakedShadowChannel` (§1) can select any of
        // them.
        let irradiance = SliceAtlas::load(&irr_bytes, 3)
            .map_err(|e| format!("{irr_compiled}: failed to read: {e}"))?;
        let dlshd_compiled = compiled_path(dlshd_path);
        let dlshd_bytes = sources
            .read(&dlshd_compiled)
            .ok_or_else(|| format!("{dlshd_compiled}: not found"))?;
        let dlshd = SliceAtlas::load(&dlshd_bytes, 4)
            .map_err(|e| format!("{dlshd_compiled}: failed to read: {e}"))?;
        Ok(ProbeAtlasTextures { irradiance, dlshd })
    }

    /// Samples the ambient cube + sun visibility at `world_pos`/`world_normal` within `volume`
    /// (`lighting.lpv.slang:105-153`, with REPORT.md §2's sub-volume clamp instead of the whole-
    /// atlas border). `shadow_channel` is the sun's baked shadow channel (§1, `export.rs`'s
    /// `find_baked_shadow_channel`): `Some(ch)` samples `1 - dlshd[ch]`, `None` (no baked sun
    /// shadow on this map) always returns visibility `1.0` without touching the dlshd atlas.
    /// `None` on a decode failure (reported by the caller, not fatal).
    pub fn sample(
        &self,
        volume: &ProbeVolume,
        world_pos: [f32; 3],
        world_normal: [f32; 3],
        shadow_channel: Option<usize>,
    ) -> Option<([f32; 3], f32)> {
        let local = volume.local(world_pos);

        let atlas_w = self.irradiance.width();
        let atlas_h = self.irradiance.height();
        let block_depth = self.dlshd.depth(); // 64 on Mirage: one block per cube face / the whole dlshd texture

        let size = volume.atlas_size;
        let offset = volume.atlas_offset;

        let tex_x = local[0] * size[0] - 0.5 + offset[0];
        let tex_y = local[1] * size[1] - 0.5 + offset[1];
        let tex_z_in_block = local[2] * size[2] - 0.5 + offset[2];

        let x_lo = offset[0].round() as u32;
        let x_hi = (offset[0] + size[0] - 1.0).max(offset[0]).round() as u32;
        let y_lo = offset[1].round() as u32;
        let y_hi = (offset[1] + size[1] - 1.0).max(offset[1]).round() as u32;
        let z_lo = offset[2].round() as u32;
        let z_hi = (offset[2] + size[2] - 1.0).max(offset[2]).round() as u32;
        let x_hi = x_hi.min(atlas_w.saturating_sub(1));
        let y_hi = y_hi.min(atlas_h.saturating_sub(1));
        let z_hi = z_hi.min(block_depth.saturating_sub(1));

        let visibility = match shadow_channel {
            Some(ch) => {
                let vis_sample = sample_trilinear(
                    &self.dlshd,
                    tex_x,
                    x_lo,
                    x_hi,
                    tex_y,
                    y_lo,
                    y_hi,
                    tex_z_in_block,
                    z_lo,
                    z_hi,
                )?;
                (1.0 - vis_sample[ch.min(3)]).clamp(0.0, 1.0)
            }
            None => 1.0,
        };

        // Ambient cube (lighting.lpv.slang:136-153): up to 3 faces, weighted by the squared
        // normal component, one per axis, +face when the component is positive else -face.
        let mut irradiance = [0f32; 3];
        for (axis, &n) in world_normal.iter().enumerate() {
            let weight = n * n;
            if weight <= 0.0 {
                continue;
            }
            let face = if n > 0.0 {
                axis as u32
            } else {
                axis as u32 + 3
            };
            let z_base = face * block_depth;
            let sample = sample_trilinear(
                &self.irradiance,
                tex_x,
                x_lo,
                x_hi,
                tex_y,
                y_lo,
                y_hi,
                tex_z_in_block + z_base as f32,
                z_lo + z_base,
                z_hi + z_base,
            )?;
            for c in 0..3 {
                irradiance[c] += sample[c] * weight;
            }
        }

        Some((irradiance, visibility))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use s2fmt::entities::EntityValue;

    const IDENTITY: [[f32; 4]; 3] = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
    ];

    fn synthetic_volume(
        handshake: i64,
        level: i64,
        aabb_min: [f32; 3],
        aabb_max: [f32; 3],
        atlas_size: [f32; 3],
        atlas_offset: [f32; 3],
    ) -> ProbeVolume {
        ProbeVolume {
            handshake,
            level,
            box_mins: [0.0, 0.0, 0.0],
            box_maxs: [1.0, 1.0, 1.0],
            world_to_local: IDENTITY,
            world_aabb_min: aabb_min,
            world_aabb_max: aabb_max,
            atlas_size,
            atlas_offset,
        }
    }

    #[test]
    fn bind_volume_handshake_match_wins_over_aabb_order() {
        // Listed first and contains the query point, but has no matching handshake -- must lose
        // to the handshake match even though it would otherwise win the AABB-contains search.
        let global = synthetic_volume(0, -2, [-1e6; 3], [1e6; 3], [4.0; 3], [0.0; 3]);
        // Its AABB does *not* contain the query point at all.
        let specific = synthetic_volume(
            42,
            0,
            [100.0, 100.0, 100.0],
            [200.0, 200.0, 200.0],
            [4.0; 3],
            [0.0; 3],
        );
        let volumes = vec![global, specific];
        let bound = bind_volume(&volumes, 42, [0.0, 0.0, 0.0]).expect("handshake match");
        assert_eq!(bound.handshake, 42);
    }

    #[test]
    fn bind_volume_prefers_the_first_containing_volume_in_sorted_order() {
        // Both AABBs contain the query point; `collect_volumes`-sorted order (level desc) has
        // already put the higher-level (more specific) volume first, and `bind_volume` must
        // return that one, not the later, lower-priority match.
        let higher_priority = synthetic_volume(
            0,
            5,
            [-10.0, -10.0, -10.0],
            [10.0, 10.0, 10.0],
            [4.0; 3],
            [0.0; 3],
        );
        let lower_priority = synthetic_volume(
            0,
            0,
            [-100.0, -100.0, -100.0],
            [100.0, 100.0, 100.0],
            [4.0; 3],
            [0.0; 3],
        );
        let volumes = vec![higher_priority, lower_priority];
        let bound = bind_volume(&volumes, 0, [1.0, 1.0, 1.0]).expect("aabb match");
        assert_eq!(bound.level, 5);
    }

    #[test]
    fn bind_volume_falls_back_to_the_last_volume_when_nothing_contains_the_point() {
        let a = synthetic_volume(
            0,
            0,
            [-1.0, -1.0, -1.0],
            [1.0, 1.0, 1.0],
            [4.0; 3],
            [0.0; 3],
        );
        let global = synthetic_volume(
            0,
            -2,
            [-2.0, -2.0, -2.0],
            [2.0, 2.0, 2.0],
            [4.0; 3],
            [0.0; 3],
        );
        let volumes = vec![a, global];
        let bound =
            bind_volume(&volumes, 0, [1000.0, 1000.0, 1000.0]).expect("fallback to the last");
        assert_eq!(
            bound.level, -2,
            "the last (global) volume must be the fallback"
        );
    }

    fn probe_volume_entity(handshake: i64, level: i64, size: [f32; 3]) -> Entity {
        Entity {
            properties: vec![
                (
                    "classname".into(),
                    EntityValue::String("env_combined_light_probe_volume".into()),
                ),
                ("box_mins".into(), EntityValue::Vector([-1.0, -1.0, -1.0])),
                ("box_maxs".into(), EntityValue::Vector([1.0, 1.0, 1.0])),
                ("handshake".into(), EntityValue::Int(handshake)),
                ("indoor_outdoor_level".into(), EntityValue::Int(level)),
                (
                    "light_probe_size_x".into(),
                    EntityValue::Float(size[0] as f64),
                ),
                (
                    "light_probe_size_y".into(),
                    EntityValue::Float(size[1] as f64),
                ),
                (
                    "light_probe_size_z".into(),
                    EntityValue::Float(size[2] as f64),
                ),
                ("light_probe_atlas_x".into(), EntityValue::Float(0.0)),
                ("light_probe_atlas_y".into(), EntityValue::Float(0.0)),
                ("light_probe_atlas_z".into(), EntityValue::Float(0.0)),
            ],
            connections: Vec::new(),
        }
    }

    #[test]
    fn collect_volumes_sorts_by_level_desc_then_atlas_size_asc() {
        let lump = EntityLump {
            name: String::new(),
            child_lumps: Vec::new(),
            entities: vec![
                probe_volume_entity(1, 0, [10.0, 10.0, 10.0]), // level 0: last regardless of size
                probe_volume_entity(2, 5, [4.0, 4.0, 4.0]),    // level 5, larger atlas
                probe_volume_entity(3, 5, [2.0, 2.0, 2.0]),    // level 5, smaller atlas: first
            ],
        };
        let (volumes, skipped) = collect_volumes(std::slice::from_ref(&lump));
        assert_eq!(skipped, 0);
        assert_eq!(
            volumes.iter().map(|v| v.handshake).collect::<Vec<_>>(),
            vec![3, 2, 1]
        );
    }

    /// Builds a minimal (no extra-data) `vtex_c` resource wrapping a plain `Rgba8888` volume
    /// texture, `depth` slices of `width*height` texels each, in on-disk (z-major) order --
    /// enough for [`SliceAtlas::load`]/[`decode_raw_mip_bytes`] without any real BC compression,
    /// so a test can put a known, distinct color in each slice.
    fn synthetic_rgba8_volume(width: u16, height: u16, depth: u16, texels: &[[u8; 4]]) -> Vec<u8> {
        let mut header = Vec::new();
        header.extend_from_slice(&1u16.to_le_bytes()); // version
        header.extend_from_slice(&0u16.to_le_bytes()); // flags
        header.extend_from_slice(&[0u8; 16]); // reflectivity
        header.extend_from_slice(&width.to_le_bytes());
        header.extend_from_slice(&height.to_le_bytes());
        header.extend_from_slice(&depth.to_le_bytes());
        header.push(4); // format = Rgba8888
        header.push(1); // num_mip_levels
        header.extend_from_slice(&0u32.to_le_bytes()); // picmip0res
        header.extend_from_slice(&0u32.to_le_bytes()); // extraDataOffset
        header.extend_from_slice(&0u32.to_le_bytes()); // extraDataCount

        let mut out = Vec::new();
        out.extend_from_slice(&0u32.to_le_bytes()); // file_size, patched below
        out.extend_from_slice(&12u16.to_le_bytes()); // header_version
        out.extend_from_slice(&0u16.to_le_bytes()); // resource version
        out.extend_from_slice(&8u32.to_le_bytes()); // block_offset
        out.extend_from_slice(&1u32.to_le_bytes()); // block_count
        out.extend_from_slice(b"DATA");
        out.extend_from_slice(&8u32.to_le_bytes()); // rel_offset
        out.extend_from_slice(&(header.len() as u32).to_le_bytes());
        out.extend_from_slice(&header);
        for px in texels {
            out.extend_from_slice(px);
        }
        let file_size = out.len() as u32;
        out[0..4].copy_from_slice(&file_size.to_le_bytes());
        out
    }

    fn flat_texels(count: usize, color: [u8; 4]) -> Vec<[u8; 4]> {
        vec![color; count]
    }

    /// `sample`'s ambient-cube weighting (REPORT.md §2, `lighting.lpv.slang:136-153`): a synthetic
    /// atlas with a distinct constant colour per cube face, one texel per volume so trilinear
    /// clamping can't blend anything in.
    #[test]
    fn sample_ambient_cube_picks_and_blends_faces_by_normal() {
        const PX: [u8; 4] = [255, 0, 0, 255]; // face 0, +X
        const PY: [u8; 4] = [0, 255, 0, 255]; // face 1, +Y ("f1")
        const PZ: [u8; 4] = [0, 0, 255, 255]; // face 2, +Z ("f2")
        const NX: [u8; 4] = [128, 64, 32, 255]; // face 3, -X
        let mut irr_texels = Vec::new();
        for face_color in [PX, PY, PZ, NX, [0, 0, 0, 255], [0, 0, 0, 255]] {
            irr_texels.push(face_color); // one texel per face slice (width=height=1)
        }
        let irr_bytes = synthetic_rgba8_volume(1, 1, 6, &irr_texels);
        // R=64, G=191: distinct per-channel values so `Some(0)` and `Some(1)` pick different
        // visibilities (`1 - dlshd[ch]`).
        let dlshd_bytes = synthetic_rgba8_volume(1, 1, 1, &flat_texels(1, [64, 191, 0, 0]));

        let atlas = ProbeAtlasTextures {
            irradiance: SliceAtlas::load(&irr_bytes, 3).expect("load irradiance"),
            dlshd: SliceAtlas::load(&dlshd_bytes, 4).expect("load dlshd"),
        };
        let volume = synthetic_volume(0, 0, [0.0; 3], [1.0; 3], [1.0, 1.0, 1.0], [0.0, 0.0, 0.0]);
        let world_pos = [0.5, 0.5, 0.5];

        let (e, vis) = atlas
            .sample(&volume, world_pos, [1.0, 0.0, 0.0], None)
            .expect("+X sample");
        assert!(
            (e[0] - 1.0).abs() < 1e-4 && e[1].abs() < 1e-4 && e[2].abs() < 1e-4,
            "{e:?}"
        );
        assert_eq!(vis, 1.0, "no baked shadow channel -> always visible");

        let (_, vis0) = atlas
            .sample(&volume, world_pos, [1.0, 0.0, 0.0], Some(0))
            .expect("+X sample, shadow channel 0");
        assert!((vis0 - (1.0 - 64.0 / 255.0)).abs() < 1e-4, "{vis0}");

        let (_, vis1) = atlas
            .sample(&volume, world_pos, [1.0, 0.0, 0.0], Some(1))
            .expect("+X sample, shadow channel 1");
        assert!((vis1 - (1.0 - 191.0 / 255.0)).abs() < 1e-4, "{vis1}");

        let (e, _) = atlas
            .sample(&volume, world_pos, [-1.0, 0.0, 0.0], None)
            .expect("-X sample");
        let want_nx = [128.0 / 255.0, 64.0 / 255.0, 32.0 / 255.0];
        for c in 0..3 {
            assert!((e[c] - want_nx[c]).abs() < 1e-4, "{e:?}");
        }

        let (e, _) = atlas
            .sample(&volume, world_pos, [0.0, 0.6, 0.8], None)
            .expect("mixed +Y/+Z sample");
        let want = [0.0, 0.36, 0.64]; // 0.6^2 * PY + 0.8^2 * PZ
        for c in 0..3 {
            assert!((e[c] - want[c]).abs() < 1e-4, "{e:?} want {want:?}");
        }
    }

    /// REPORT.md §2's deliberate deviation from the reference: the sub-volume clamp must not let
    /// trilinear filtering read a neighbouring volume's own cells, even right at the shared edge
    /// between them.
    #[test]
    fn sample_sub_volume_edge_does_not_read_the_neighbour() {
        const A_FAR_EDGE: [u8; 4] = [10, 0, 0, 255];
        const B_NEAR_EDGE: [u8; 4] = [250, 0, 0, 255];
        // 4-wide row: volume A owns x=[0,1], volume B owns x=[2,3]; only face 0 (+X, z=0) is
        // populated meaningfully, the other 5 faces are never read (a pure +X normal has zero
        // weight on every other axis).
        let mut irr_texels = vec![[0, 0, 0, 255]; 4]; // x=0 (A near edge, irrelevant)
        irr_texels[1] = A_FAR_EDGE; // x=1: A's own far edge
        irr_texels[2] = B_NEAR_EDGE; // x=2: B's near edge -- must never be read for a sample in A
        irr_texels[3] = [0, 0, 0, 255]; // x=3
        for _ in 1..6 {
            irr_texels.extend(vec![[0, 0, 0, 255]; 4]); // faces 1..5, unused
        }
        let irr_bytes = synthetic_rgba8_volume(4, 1, 6, &irr_texels);
        let dlshd_bytes = synthetic_rgba8_volume(4, 1, 1, &flat_texels(4, [0, 0, 0, 0]));

        let atlas = ProbeAtlasTextures {
            irradiance: SliceAtlas::load(&irr_bytes, 3).expect("load irradiance"),
            dlshd: SliceAtlas::load(&dlshd_bytes, 4).expect("load dlshd"),
        };
        let volume_a = synthetic_volume(0, 0, [0.0; 3], [1.0; 3], [2.0, 1.0, 1.0], [0.0, 0.0, 0.0]);
        // local[0] = 1.0: right at volume A's own box_maxs edge, which maps to the atlas' shared
        // boundary with volume B.
        let (e, _) = atlas
            .sample(&volume_a, [1.0, 0.5, 0.5], [1.0, 0.0, 0.0], None)
            .expect("edge sample");
        let want = [A_FAR_EDGE[0] as f32 / 255.0, 0.0, 0.0];
        assert!(
            (e[0] - want[0]).abs() < 1e-4,
            "edge sample must read A's own far texel ({want:?}), not blend toward B's near \
             texel ({:?}); got {e:?}",
            [B_NEAR_EDGE[0] as f32 / 255.0, 0.0, 0.0]
        );
    }
}
