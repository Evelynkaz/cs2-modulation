//! Wavefront OBJ export for visual inspection.

use std::io::{self, BufWriter, Write};

use crate::filter::AttributeMask;
use crate::mesh::CollisionMesh;

/// Export options for [`write_obj`].
#[derive(Debug, Clone, Copy)]
pub struct ObjOptions {
    /// Write vertices as `(x, z, -y)` so Source's Z-up coordinates come out
    /// upright in Y-up OBJ importers (e.g. Blender's default OBJ import).
    pub swap_to_y_up: bool,
}

impl Default for ObjOptions {
    fn default() -> Self {
        Self { swap_to_y_up: true }
    }
}

/// Counts from a [`write_obj`] call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ObjStats {
    pub triangles_written: usize,
    pub vertices_written: usize,
    pub groups_written: usize,
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn aabb_overlap(a: ([f32; 3], [f32; 3]), b: ([f32; 3], [f32; 3])) -> bool {
    for i in 0..3 {
        if a.1[i] < b.0[i] || b.1[i] < a.0[i] {
            return false;
        }
    }
    true
}

fn tri_aabb(a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> ([f32; 3], [f32; 3]) {
    let mut min = a;
    let mut max = a;
    for v in [b, c] {
        for i in 0..3 {
            min[i] = min[i].min(v[i]);
            max[i] = max[i].max(v[i]);
        }
    }
    (min, max)
}

/// Deterministic distinct colour for an attribute name: FNV-1a hash of the
/// name to a hue, fixed saturation/value, converted to RGB.
fn attribute_color(name: &str) -> (f32, f32, f32) {
    let mut hash: u32 = 0x811c9dc5;
    for b in name.as_bytes() {
        hash ^= u32::from(*b);
        hash = hash.wrapping_mul(0x01000193);
    }
    let hue = (hash % 360) as f32;
    hsv_to_rgb(hue, 0.65, 0.9)
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
    let c = v * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    (r1 + m, g1 + m, b1 + m)
}

/// Writes `mesh` as a Wavefront OBJ, optionally restricted to triangles that
/// pass `mask` and whose AABB overlaps `region`, grouped and coloured per
/// collision attribute. Only vertices actually referenced are written.
/// Validates `mesh` first, surfacing failures as `io::ErrorKind::InvalidData`.
pub fn write_obj(
    mesh: &CollisionMesh,
    mask: Option<&AttributeMask>,
    region: Option<([f32; 3], [f32; 3])>,
    obj: impl Write,
    mtl: Option<(&mut dyn Write, &str)>,
    options: ObjOptions,
) -> io::Result<ObjStats> {
    mesh.validate()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    let mut obj = BufWriter::new(obj);

    // Select triangles per attribute, preserving attribute order.
    let mut by_attribute: Vec<Vec<usize>> = vec![Vec::new(); mesh.attributes.len()];
    for (tri_idx, tri) in mesh.triangles.iter().enumerate() {
        let attr = mesh.tri_attribute[tri_idx];
        if let Some(mask) = mask
            && !mask.is_solid(attr)
        {
            continue;
        }
        if let Some(region) = region {
            let a = mesh.vertices[tri[0] as usize];
            let b = mesh.vertices[tri[1] as usize];
            let c = mesh.vertices[tri[2] as usize];
            if !aabb_overlap(tri_aabb(a, b, c), region) {
                continue;
            }
        }
        by_attribute[usize::from(attr)].push(tri_idx);
    }

    // Re-index referenced vertices in first-use order.
    let mut remap: Vec<Option<u32>> = vec![None; mesh.vertices.len()];
    let mut written_vertices: Vec<[f32; 3]> = Vec::new();
    for tris in &by_attribute {
        for &tri_idx in tris {
            for &v in &mesh.triangles[tri_idx] {
                let slot = &mut remap[v as usize];
                if slot.is_none() {
                    *slot = Some(written_vertices.len() as u32);
                    written_vertices.push(mesh.vertices[v as usize]);
                }
            }
        }
    }

    if options.swap_to_y_up {
        writeln!(
            obj,
            "# cs2-modulation export: Source (x,y,z) written as (x, z, -y) for Y-up OBJ importers"
        )?;
    } else {
        writeln!(obj, "# cs2-modulation export: Source (x,y,z) as-is")?;
    }

    let mtl_filename = mtl.as_ref().map(|(_, name)| *name);
    if let Some(name) = mtl_filename {
        writeln!(obj, "mtllib {name}")?;
    }

    for v in &written_vertices {
        if options.swap_to_y_up {
            writeln!(obj, "v {} {} {}", v[0], v[2], -v[1])?;
        } else {
            writeln!(obj, "v {} {} {}", v[0], v[1], v[2])?;
        }
    }

    let mut mtl_body = String::new();
    let mut groups_written = 0usize;
    let mut triangles_written = 0usize;

    for (attr_idx, tris) in by_attribute.iter().enumerate() {
        if tris.is_empty() {
            continue;
        }
        let attr_name = &mesh.attributes[attr_idx].name;
        let group_name = format!("{}_{}", sanitize(attr_name), attr_idx);
        writeln!(obj, "g {group_name}")?;
        writeln!(obj, "usemtl {group_name}")?;
        for &tri_idx in tris {
            let tri = mesh.triangles[tri_idx];
            let a = remap[tri[0] as usize].unwrap() + 1;
            let b = remap[tri[1] as usize].unwrap() + 1;
            let c = remap[tri[2] as usize].unwrap() + 1;
            writeln!(obj, "f {a} {b} {c}")?;
        }
        groups_written += 1;
        triangles_written += tris.len();

        if mtl.is_some() {
            let (r, g, b) = attribute_color(attr_name);
            mtl_body.push_str(&format!("newmtl {group_name}\nKd {r:.6} {g:.6} {b:.6}\n"));
        }
    }

    obj.flush()?;

    if let Some((mtl_writer, _)) = mtl {
        let mut mtl_writer = BufWriter::new(mtl_writer);
        write!(mtl_writer, "{mtl_body}")?;
        mtl_writer.flush()?;
    }

    Ok(ObjStats {
        triangles_written,
        vertices_written: written_vertices.len(),
        groups_written,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::all_mask;
    use crate::mesh::{CollisionAttribute, MeshObject, ObjectKind, SurfaceProperty};

    fn two_triangle_mesh() -> CollisionMesh {
        let mut mesh = CollisionMesh::new();
        let a = mesh
            .add_attribute(CollisionAttribute {
                name: "Default".to_string(),
                interact_as: vec![],
                interact_with: vec![],
                interact_exclude: vec![],
                synthetic: false,
            })
            .unwrap();
        let o = mesh.add_object(MeshObject {
            kind: ObjectKind::WorldHull,
            classname: None,
            targetname: None,
            model: None,
            hammer_id: None,
            source_index: 0,
            hull_flags: None,
        });
        mesh.push_triangles(
            &[
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [0.0, 1.0, 0.0],
                [2.0, 0.0, 0.0],
            ],
            &[[0, 1, 2], [1, 3, 2]],
            a,
            |_| SurfaceProperty::NONE,
            o,
        )
        .unwrap();
        mesh
    }

    #[test]
    fn two_triangle_exact_text_y_up() {
        let mesh = two_triangle_mesh();
        let mut out = Vec::new();
        let stats = write_obj(&mesh, None, None, &mut out, None, ObjOptions::default()).unwrap();
        let text = String::from_utf8(out).unwrap();
        let expected = "# cs2-modulation export: Source (x,y,z) written as (x, z, -y) for Y-up OBJ importers\n\
v 0 0 -0\n\
v 1 0 -0\n\
v 0 0 -1\n\
v 2 0 -0\n\
g Default_0\n\
usemtl Default_0\n\
f 1 2 3\n\
f 2 4 3\n";
        assert_eq!(text, expected);
        assert_eq!(stats.triangles_written, 2);
        assert_eq!(stats.vertices_written, 4);
        assert_eq!(stats.groups_written, 1);
    }

    #[test]
    fn no_swap_writes_coords_as_is() {
        let mesh = two_triangle_mesh();
        let mut out = Vec::new();
        write_obj(
            &mesh,
            None,
            None,
            &mut out,
            None,
            ObjOptions {
                swap_to_y_up: false,
            },
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("v 0 0 0\n"));
        assert!(text.contains("v 0 1 0\n"));
    }

    #[test]
    fn mask_filters_triangles() {
        let mut mesh = two_triangle_mesh();
        // Add a second attribute with zero solid triangles via all_mask-then-from_fn style test.
        let mask = crate::filter::from_fn(&mesh, |_| false);
        let mut out = Vec::new();
        let stats = write_obj(
            &mesh,
            Some(&mask),
            None,
            &mut out,
            None,
            ObjOptions::default(),
        )
        .unwrap();
        assert_eq!(stats.triangles_written, 0);
        assert_eq!(stats.vertices_written, 0);
        let _ = &mut mesh;
    }

    #[test]
    fn region_filters_triangles() {
        let mut mesh = CollisionMesh::new();
        let a = mesh
            .add_attribute(CollisionAttribute {
                name: "Default".to_string(),
                interact_as: vec![],
                interact_with: vec![],
                interact_exclude: vec![],
                synthetic: false,
            })
            .unwrap();
        let o = mesh.add_object(MeshObject {
            kind: ObjectKind::WorldHull,
            classname: None,
            targetname: None,
            model: None,
            hammer_id: None,
            source_index: 0,
            hull_flags: None,
        });
        mesh.push_triangles(
            &[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            &[[0, 1, 2]],
            a,
            |_| SurfaceProperty::NONE,
            o,
        )
        .unwrap();
        mesh.push_triangles(
            &[[50.0, 0.0, 0.0], [51.0, 0.0, 0.0], [50.0, 1.0, 0.0]],
            &[[0, 1, 2]],
            a,
            |_| SurfaceProperty::NONE,
            o,
        )
        .unwrap();
        let mask = all_mask(&mesh);
        // Region only covers the first triangle's corner area.
        let region = ([-1.0, -1.0, -1.0], [1.5, 1.5, 1.0]);
        let mut out = Vec::new();
        let stats = write_obj(
            &mesh,
            Some(&mask),
            Some(region),
            &mut out,
            None,
            ObjOptions::default(),
        )
        .unwrap();
        assert_eq!(stats.triangles_written, 1);
    }

    #[test]
    fn writes_mtl_with_distinct_colors() {
        let mut mesh = CollisionMesh::new();
        let a1 = mesh
            .add_attribute(CollisionAttribute {
                name: "Default".to_string(),
                interact_as: vec![],
                interact_with: vec![],
                interact_exclude: vec![],
                synthetic: false,
            })
            .unwrap();
        let a2 = mesh
            .add_attribute(CollisionAttribute {
                name: "PlayerClip".to_string(),
                interact_as: vec![],
                interact_with: vec![],
                interact_exclude: vec![],
                synthetic: false,
            })
            .unwrap();
        let o = mesh.add_object(MeshObject {
            kind: ObjectKind::WorldHull,
            classname: None,
            targetname: None,
            model: None,
            hammer_id: None,
            source_index: 0,
            hull_flags: None,
        });
        mesh.push_triangles(
            &[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            &[[0, 1, 2]],
            a1,
            |_| SurfaceProperty::NONE,
            o,
        )
        .unwrap();
        mesh.push_triangles(
            &[[5.0, 0.0, 0.0], [6.0, 0.0, 0.0], [5.0, 1.0, 0.0]],
            &[[0, 1, 2]],
            a2,
            |_| SurfaceProperty::NONE,
            o,
        )
        .unwrap();
        let mut out = Vec::new();
        let mut mtl_out = Vec::new();
        let stats = write_obj(
            &mesh,
            None,
            None,
            &mut out,
            Some((&mut mtl_out, "test.mtl")),
            ObjOptions::default(),
        )
        .unwrap();
        assert_eq!(stats.groups_written, 2);
        let mtl_text = String::from_utf8(mtl_out).unwrap();
        assert!(mtl_text.contains("newmtl Default_0"));
        assert!(mtl_text.contains("newmtl PlayerClip_1"));
        let obj_text = String::from_utf8(out).unwrap();
        assert!(obj_text.contains("mtllib test.mtl"));
    }
}
