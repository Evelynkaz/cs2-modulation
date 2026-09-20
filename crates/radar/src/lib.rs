//! 2D radar rendering: a top-down PNG slice of a map's collision mesh plus a small
//! `viewer-map.json` header, ported from `ViewerDataCommand.cs` (whole file cited throughout
//! this module as `ViewerDataCommand.cs:NN`).

use std::collections::HashMap;
use std::path::Path;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use extract::report::EntityRecord;
use geom::collider::{Collider, ColliderError};
use geom::filter;
use geom::grid::UniformGrid;
use geom::math::{Aabb, V3};
use geom::mesh::CollisionMesh;
use solver::nav_ground::{NAV_GAP_REACH, nav_ground_z_nearby};

/// Ground-height sampling cell, `ViewerDataCommand.cs:50`.
const NAV_CELL: f32 = 16.0;
/// Bucket size grouping nav areas near each `NAV_CELL` sample, so a sample only tests the
/// handful of areas that could actually reach it, `ViewerDataCommand.cs:55`.
const BUCKET_SIZE: f32 = 256.0;

#[derive(Debug, Error)]
pub enum RadarError {
    #[error(
        "no nav areas within reach of the requested region - check --region against the map's nav data"
    )]
    NoNavCoverage,
    #[error("failed to build radar collider: {0}")]
    Collider(#[from] ColliderError),
    #[error("failed to write PNG: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to encode PNG: {0}")]
    Encoding(#[from] png::EncodingError),
    #[error("bad --region: {0}")]
    BadRegion(String),
    #[error("bad --pixel-size: {0}")]
    BadPixelSize(String),
}

pub struct RadarOptions {
    /// World units per pixel; `ViewerDataCommand.cs:103` hardcodes 2.
    pub pixel_size: f32,
    /// `x0,y0,x1,y1`; `None` derives the region from nav coverage (see [`default_region`]).
    pub region: Option<[f32; 4]>,
}

impl Default for RadarOptions {
    fn default() -> Self {
        RadarOptions {
            pixel_size: 2.0,
            region: None,
        }
    }
}

#[derive(Debug)]
pub struct RadarImage {
    pub width: u32,
    pub height: u32,
    /// 4 bytes per pixel, row-major, row 0 = north edge (`ViewerDataCommand.cs:135-137`).
    pub rgba: Vec<u8>,
    pub region: [i32; 4],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewerMap {
    pub map: String,
    pub build: String,
    pub region: [i32; 4],
    pub image: String,
    #[serde(rename = "pixelSize")]
    pub pixel_size: f32,
    pub callouts: Vec<(String, i32, i32)>,
}

/// The AABB of every nav area's corners, expanded outward to a multiple of `pixel_size * 8`.
/// The reference command always takes `--region` from the caller and never derives one itself;
/// with no such precedent to port, this is our own choice, made here so the region always lines
/// up on a coarse pixel boundary (readable at any zoom) rather than an arbitrary one. Returns
/// `None` if there are no nav areas at all.
fn default_region(nav_areas: &[Vec<V3>], pixel_size: f32) -> Option<[f32; 4]> {
    let mut min = (f32::MAX, f32::MAX);
    let mut max = (f32::MIN, f32::MIN);
    let mut any = false;
    for area in nav_areas {
        for c in area {
            any = true;
            min.0 = min.0.min(c.x);
            min.1 = min.1.min(c.y);
            max.0 = max.0.max(c.x);
            max.1 = max.1.max(c.y);
        }
    }
    if !any {
        return None;
    }
    let step = pixel_size * 8.0;
    Some([
        (min.0 / step).floor() * step,
        (min.1 / step).floor() * step,
        (max.0 / step).ceil() * step,
        (max.1 / step).ceil() * step,
    ])
}

/// Buckets nav areas by `BUCKET_SIZE` over `region`, padded by [`NAV_GAP_REACH`] so a sample near
/// a bucket edge still sees areas whose gap-bridging reach could cover it (`ViewerDataCommand.cs:59-73`).
fn bucket_nav_areas(
    nav_areas: &[Vec<V3>],
    region: [f32; 4],
    bw: i64,
    bh: i64,
) -> Vec<Vec<Vec<V3>>> {
    let [x0, y0, ..] = region;
    let mut buckets = vec![Vec::new(); (bw * bh) as usize];
    for area in nav_areas {
        let mut amin = (f32::MAX, f32::MAX);
        let mut amax = (f32::MIN, f32::MIN);
        for c in area {
            amin.0 = amin.0.min(c.x);
            amin.1 = amin.1.min(c.y);
            amax.0 = amax.0.max(c.x);
            amax.1 = amax.1.max(c.y);
        }
        let bx0 = (((amin.0 - x0 - NAV_GAP_REACH) / BUCKET_SIZE) as i64).clamp(0, bw - 1);
        let bx1 = (((amax.0 - x0 + NAV_GAP_REACH) / BUCKET_SIZE) as i64).clamp(0, bw - 1);
        let by0 = (((amin.1 - y0 - NAV_GAP_REACH) / BUCKET_SIZE) as i64).clamp(0, bh - 1);
        let by1 = (((amax.1 - y0 + NAV_GAP_REACH) / BUCKET_SIZE) as i64).clamp(0, bh - 1);
        for by in by0..=by1 {
            for bx in bx0..=bx1 {
                buckets[(by * bw + bx) as usize].push(area.clone());
            }
        }
    }
    buckets
}

/// Ground height per `NAV_CELL` cell over `region`, lowest-wins on stacked areas
/// (`ViewerDataCommand.cs:74-88`, via [`nav_ground_z_nearby`]). Returns `(gw, gh, values)`.
fn build_nav_grid(nav_areas: &[Vec<V3>], region: [f32; 4]) -> (i64, i64, Vec<Option<f32>>) {
    let [x0, y0, x1, y1] = region;
    let gw = ((x1 - x0) / NAV_CELL).ceil() as i64;
    let gh = ((y1 - y0) / NAV_CELL).ceil() as i64;
    let bw = ((x1 - x0) / BUCKET_SIZE).ceil() as i64 + 1;
    let bh = ((y1 - y0) / BUCKET_SIZE).ceil() as i64 + 1;
    let buckets = bucket_nav_areas(nav_areas, region, bw, bh);

    let nav_z: Vec<Option<f32>> = (0..(gw * gh) as usize)
        .into_par_iter()
        .map(|i| {
            let gx = i as i64 % gw;
            let gy = i as i64 / gw;
            let wx = x0 + (gx as f32 + 0.5) * NAV_CELL;
            let wy = y0 + (gy as f32 + 0.5) * NAV_CELL;
            let bx = (((wx - x0) / BUCKET_SIZE) as i64).clamp(0, bw - 1);
            let by = (((wy - y0) / BUCKET_SIZE) as i64).clamp(0, bh - 1);
            let bucket = &buckets[(by * bw + bx) as usize];
            if bucket.is_empty() {
                None
            } else {
                nav_ground_z_nearby(bucket, wx, wy)
            }
        })
        .collect();
    (gw, gh, nav_z)
}

/// Renders `mesh` to a radar-style top-down slice over `nav_areas`' walkable ground
/// (`ViewerDataCommand.cs:15-259`).
pub fn render(
    mesh: &CollisionMesh,
    nav_areas: &[Vec<V3>],
    opts: &RadarOptions,
) -> Result<RadarImage, RadarError> {
    let region = match opts.region {
        Some(r) => r,
        None => default_region(nav_areas, opts.pixel_size).ok_or(RadarError::NoNavCoverage)?,
    };
    let [x0, y0, x1, y1] = region;
    let pixel_size = opts.pixel_size;

    if !pixel_size.is_finite() || pixel_size <= 0.0 {
        return Err(RadarError::BadPixelSize(format!(
            "pixel size must be a finite number greater than 0, got {pixel_size}"
        )));
    }
    if !x0.is_finite()
        || !y0.is_finite()
        || !x1.is_finite()
        || !y1.is_finite()
        || x1 <= x0
        || y1 <= y0
    {
        return Err(RadarError::BadRegion(format!(
            "region must have finite coordinates with x1 > x0 and y1 > y0, got {x0},{y0},{x1},{y1}"
        )));
    }
    let est_w = ((x1 as f64 - x0 as f64) / pixel_size as f64).ceil();
    let est_h = ((y1 as f64 - y0 as f64) / pixel_size as f64).ceil();
    if est_w < 1.0 || est_h < 1.0 || est_w * est_h > 64_000_000.0 {
        return Err(RadarError::BadRegion(format!(
            "region {x0},{y0},{x1},{y1} at pixel size {pixel_size} would render a {}x{} image, which is out of bounds",
            est_w as i64, est_h as i64
        )));
    }

    let (gw, gh, nav_z) = build_nav_grid(nav_areas, region);
    let nav_values: Vec<f32> = nav_z.iter().filter_map(|v| *v).collect();
    if nav_values.is_empty() {
        return Err(RadarError::NoNavCoverage);
    }
    let nav_lo = nav_values.iter().copied().fold(f32::INFINITY, f32::min);
    let nav_hi = nav_values.iter().copied().fold(f32::NEG_INFINITY, f32::max);

    let nav_cell_of = |wx: f32, wy: f32| -> Option<f32> {
        let gx = ((wx - x0) / NAV_CELL) as i64;
        let gy = ((wy - y0) / NAV_CELL) as i64;
        if gx >= 0 && gx < gw && gy >= 0 && gy < gh {
            nav_z[(gy * gw + gx) as usize]
        } else {
            None
        }
    };

    // Bounded around the walkable floor, not an absolute height - the nav mesh already says
    // where the playable band is (`ViewerDataCommand.cs:104-117`).
    let (mesh_min, mesh_max) = mesh
        .bounds()
        .map(|(lo, hi)| (V3::from_array(lo), V3::from_array(hi)))
        .unwrap_or((V3::ZERO, V3::ZERO));
    let z_lo = mesh_min.z.max(nav_lo - 256.0);
    let z_hi = mesh_max.z.min(nav_hi + 512.0);
    let mask = filter::names_mask(mesh, &["Default", "default", "EntitySolid"]);
    let region_aabb = Aabb {
        min: V3::new(x0, y0, z_lo),
        max: V3::new(x1, y1, z_hi),
    };
    let collider = UniformGrid::build(mesh, &mask, Some(region_aabb), 128.0)?;

    let hit_between = |wx: f32, wy: f32, zlo: f32, zhi: f32| -> bool {
        collider.box_intersects(
            V3::new(wx, wy, (zlo + zhi) * 0.5),
            V3::new(pixel_size * 0.5, pixel_size * 0.5, (zhi - zlo) * 0.5),
        )
    };

    let w = ((x1 - x0) / pixel_size).max(0.0) as u32;
    let h = ((y1 - y0) / pixel_size).max(0.0) as u32;

    // Image row 0 is the north edge so the viewer can blit directly (`ViewerDataCommand.cs:135-137`).
    let mut pixels: Vec<[u8; 4]> = (0..(w as usize * h as usize))
        .into_par_iter()
        .map(|i| {
            let px = (i as u32 % w) as i32;
            let py = (i as u32 / w) as i32;
            let wx = x0 + (px as f32 + 0.5) * pixel_size;
            let wy = y1 - (py as f32 + 0.5) * pixel_size;
            let Some(ground) = nav_cell_of(wx, wy) else {
                return [0, 0, 0, 0];
            };
            // Snap to the actual floor near the nav estimate before slicing, with a single
            // straight-down ray from `ground + 40` to `ground - 24` (`ViewerDataCommand.cs:146-151`).
            // Not the solver's 5-point hull-footprint floor probe: that probe takes the *max* of
            // its five samples, so it climbs onto whatever low object sits under a corner of the
            // player hull even when the pixel's own center is clear floor next to it, misreading
            // that object's own top as "the floor" and shrinking the low-cover band (class 128) a
            // pixel derived this way would otherwise report. A single ray under the pixel center
            // matches the reference exactly.
            let floor_z = match collider.first_hit_ray(
                V3::new(wx, wy, ground + 40.0),
                V3::new(wx, wy, ground - 24.0),
            ) {
                Some(hit) => ground + 40.0 + hit.t * -64.0,
                None => ground,
            };
            // R encodes the class (0 floor, 128 low cover, 255 wall); G encodes map-level ground
            // height for a subtle floor tint (`ViewerDataCommand.cs:152-164`).
            let cls = if hit_between(wx, wy, floor_z + 44.0, floor_z + 76.0) {
                255u8
            } else if hit_between(wx, wy, floor_z + 12.0, floor_z + 44.0) {
                128u8
            } else {
                0u8
            };
            let tint = (255.0 * (ground - nav_lo) / (nav_hi - nav_lo).max(1.0)) as u8;
            [cls, tint, 0, 255]
        })
        .collect();

    // Boundary pass: walls enclosing the playable space sit just outside nav coverage; probe
    // them from their covered neighbors (`ViewerDataCommand.cs:167-213`).
    let is_covered: Vec<bool> = pixels.iter().map(|p| p[3] != 0).collect();
    pixels = (0..(w as usize * h as usize))
        .into_par_iter()
        .map(|i| {
            if is_covered[i] {
                return pixels[i];
            }
            let px = (i as u32 % w) as i32;
            let py = (i as u32 / w) as i32;
            let mut neighbor_ground: Option<f32> = None;
            'outer: for dy in -1..=1 {
                for dx in -1..=1 {
                    let (nx, ny) = (px + dx, py + dy);
                    if nx < 0 || nx >= w as i32 || ny < 0 || ny >= h as i32 {
                        continue;
                    }
                    let ni = (ny as u32 * w + nx as u32) as usize;
                    if !is_covered[ni] {
                        continue;
                    }
                    let nwx = x0 + (nx as f32 + 0.5) * pixel_size;
                    let nwy = y1 - (ny as f32 + 0.5) * pixel_size;
                    if let Some(nz) = nav_cell_of(nwx, nwy) {
                        neighbor_ground = Some(nz);
                        break 'outer;
                    }
                }
            }
            let Some(ground) = neighbor_ground else {
                return pixels[i];
            };
            let wx = x0 + (px as f32 + 0.5) * pixel_size;
            let wy = y1 - (py as f32 + 0.5) * pixel_size;
            if hit_between(wx, wy, ground + 12.0, ground + 76.0) {
                [255, 0, 0, 255]
            } else {
                pixels[i]
            }
        })
        .collect();

    // Thicken the enclosing outline over two passes so it stays readable at every zoom
    // (`ViewerDataCommand.cs:214-250`).
    for _ in 0..2 {
        let snapshot = pixels.clone();
        pixels = (0..(w as usize * h as usize))
            .into_par_iter()
            .map(|i| {
                if snapshot[i][3] != 0 {
                    return snapshot[i];
                }
                let px = (i as u32 % w) as i32;
                let py = (i as u32 / w) as i32;
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        let (nx, ny) = (px + dx, py + dy);
                        if nx < 0 || nx >= w as i32 || ny < 0 || ny >= h as i32 {
                            continue;
                        }
                        let np = snapshot[(ny as u32 * w + nx as u32) as usize];
                        if np[3] != 0 && np[0] == 255 && np[1] == 0 {
                            return [255, 0, 0, 255];
                        }
                    }
                }
                snapshot[i]
            })
            .collect();
    }

    let mut rgba = Vec::with_capacity(pixels.len() * 4);
    for p in &pixels {
        rgba.extend_from_slice(p);
    }

    Ok(RadarImage {
        width: w,
        height: h,
        rgba,
        region: [x0 as i32, y0 as i32, x1 as i32, y1 as i32],
    })
}

/// `env_cs_place` entities inside `region`, grouped by name case-insensitively (average
/// coordinates per group), sorted by name (Ordinal) for a deterministic order
/// (`ViewerDataCommand.cs:260-289`).
pub fn callouts(entities: &[EntityRecord], region: [i32; 4]) -> Vec<(String, i32, i32)> {
    let [x0, y0, x1, y1] = region;
    let mut groups: HashMap<String, (String, f64, f64, usize)> = HashMap::new();
    for e in entities {
        if e.classname != "env_cs_place" {
            continue;
        }
        let Some(place) = e.properties.get("place_name").and_then(|v| v.as_str()) else {
            continue;
        };
        if place.is_empty() {
            continue;
        }
        let (ex, ey) = (e.origin[0], e.origin[1]);
        if ex < x0 as f32 || ex > x1 as f32 || ey < y0 as f32 || ey > y1 as f32 {
            continue;
        }
        let entry = groups
            .entry(place.to_ascii_lowercase())
            .or_insert_with(|| (place.to_string(), 0.0, 0.0, 0));
        entry.1 += ex as f64;
        entry.2 += ey as f64;
        entry.3 += 1;
    }
    let mut out: Vec<(String, i32, i32)> = groups
        .into_values()
        .map(|(name, sx, sy, n)| (name, (sx / n as f64) as i32, (sy / n as f64) as i32))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Writes `img` as an 8-bit RGBA PNG to `path` (not atomic; callers writing into a shared cache
/// directory should write to a temp path and rename, as elsewhere in this codebase).
pub fn write_png(img: &RadarImage, path: &Path) -> Result<(), RadarError> {
    let file = std::fs::File::create(path)?;
    let w = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(w, img.width, img.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&img.rgba)?;
    Ok(())
}
