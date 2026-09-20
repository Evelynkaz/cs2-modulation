//! The 3D viewer's mesh payload: a binary `SM3D` v2 snapshot of every triangle the grenade sim
//! collides with, ported from `LineupApi.MeshPayloadSolid` (`LineupApi.cs:38-106`).
//!
//! Format, little-endian: `[u32 magic 'SM3D'][u32 format version][i32 vertex count]
//! [i32 world index count][i32 phantom index count][i32 door index count]
//! [i32 breakable index count][f32 x,y,z per vertex][u32 world indices][u32 phantom indices]
//! [u32 door indices][u32 breakable indices]`. The magic and version lead so a stale viewer
//! parsing a newer payload fails loudly instead of rendering scrambled geometry
//! (`LineupApi.cs:29-31`).

use geom::filter;
use geom::mesh::CollisionMesh;

/// `"SM3D"` little-endian (`LineupApi.cs:35`).
const MESH_MAGIC: u32 = 0x4433_4D53;
/// `LineupApi.cs:36`.
const MESH_FORMAT_VERSION: u32 = 2;

/// Builds the `SM3D` payload for `mesh`: every triangle the grenade filter treats as solid,
/// split into four index groups (world / phantom / door / breakable) exactly as
/// `LineupApi.cs:41-83` classifies them.
pub fn sm3d(mesh: &CollisionMesh) -> Vec<u8> {
    let grenade_solid = filter::grenade_mask(mesh);

    // `LineupApi.cs:41-53`: solid-for-grenade groups that are invisible or special in the real
    // world - grenade clips, "window" layers, and the synthetic physics-clip/breakable groups.
    let phantom: Vec<bool> = mesh
        .attributes
        .iter()
        .enumerate()
        .map(|(i, a)| {
            grenade_solid.is_solid(i as u16)
                && (a
                    .interact_as
                    .iter()
                    .any(|l| l.eq_ignore_ascii_case("csgo_grenadeclip"))
                    || a.interact_as
                        .iter()
                        .any(|l| l.eq_ignore_ascii_case("window"))
                    || a.name == "EntityPhysicsClip"
                    || a.name == "EntityBreakable")
        })
        .collect();
    // `LineupApi.cs:58-64`: doors and breakables ride in their own groups so the viewer can drop
    // them for a "broken" world state.
    let door_group: Vec<bool> = mesh
        .attributes
        .iter()
        .map(|a| a.name == "EntityDoor")
        .collect();
    let breakable_group: Vec<bool> = mesh
        .attributes
        .iter()
        .map(|a| a.name == "EntityBreakable")
        .collect();

    let mut world = Vec::new();
    let mut special = Vec::new();
    let mut doors = Vec::new();
    let mut breakables = Vec::new();

    // `LineupApi.cs:69-83`.
    for (i, tri) in mesh.triangles.iter().enumerate() {
        let attr = mesh.tri_attribute[i];
        if !grenade_solid.is_solid(attr) {
            continue;
        }
        let group = if door_group[attr as usize] {
            &mut doors
        } else if breakable_group[attr as usize] {
            &mut breakables
        } else if phantom[attr as usize] {
            &mut special
        } else {
            &mut world
        };
        group.push(tri[0]);
        group.push(tri[1]);
        group.push(tri[2]);
    }

    let mut out = Vec::with_capacity(
        20 + mesh.vertices.len() * 12
            + (world.len() + special.len() + doors.len() + breakables.len()) * 4,
    );
    out.extend_from_slice(&MESH_MAGIC.to_le_bytes());
    out.extend_from_slice(&MESH_FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&(mesh.vertices.len() as i32).to_le_bytes());
    out.extend_from_slice(&(world.len() as i32).to_le_bytes());
    out.extend_from_slice(&(special.len() as i32).to_le_bytes());
    out.extend_from_slice(&(doors.len() as i32).to_le_bytes());
    out.extend_from_slice(&(breakables.len() as i32).to_le_bytes());
    for v in &mesh.vertices {
        out.extend_from_slice(&v[0].to_le_bytes());
        out.extend_from_slice(&v[1].to_le_bytes());
        out.extend_from_slice(&v[2].to_le_bytes());
    }
    for group in [&world, &special, &doors, &breakables] {
        for &i in group.iter() {
            out.extend_from_slice(&i.to_le_bytes());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::mesh::{CollisionAttribute, MeshObject, ObjectKind, SurfaceProperty};

    fn attr(name: &str, interact_as: &[&str]) -> CollisionAttribute {
        CollisionAttribute {
            name: name.to_string(),
            interact_as: interact_as.iter().map(|s| s.to_string()).collect(),
            interact_with: vec![],
            interact_exclude: vec![],
            synthetic: false,
        }
    }

    fn push_tri(mesh: &mut CollisionMesh, attribute: u16, offset: f32) {
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
                [offset, 0.0, 0.0],
                [offset + 1.0, 0.0, 0.0],
                [offset, 1.0, 0.0],
            ],
            &[[0, 1, 2]],
            attribute,
            |_| SurfaceProperty::NONE,
            o,
        )
        .unwrap();
    }

    #[test]
    fn header_and_group_counts_match_classification() {
        let mut mesh = CollisionMesh::new();
        let world_attr = mesh.add_attribute(attr("Default", &[])).unwrap();
        let clip_attr = mesh
            .add_attribute(attr("Clip", &["csgo_grenadeclip"]))
            .unwrap();
        let door_attr = mesh.add_attribute(attr("EntityDoor", &[])).unwrap();
        let sky_attr = mesh.add_attribute(attr("Sky", &["sky"])).unwrap();
        push_tri(&mut mesh, world_attr, 0.0);
        push_tri(&mut mesh, clip_attr, 10.0);
        push_tri(&mut mesh, door_attr, 20.0);
        push_tri(&mut mesh, sky_attr, 30.0); // not grenade-solid, excluded entirely

        let bytes = sm3d(&mesh);
        let magic = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        let vertex_count = i32::from_le_bytes(bytes[8..12].try_into().unwrap());
        let world_count = i32::from_le_bytes(bytes[12..16].try_into().unwrap());
        let phantom_count = i32::from_le_bytes(bytes[16..20].try_into().unwrap());
        let door_count = i32::from_le_bytes(bytes[20..24].try_into().unwrap());
        let breakable_count = i32::from_le_bytes(bytes[24..28].try_into().unwrap());

        assert_eq!(magic, MESH_MAGIC);
        assert_eq!(version, MESH_FORMAT_VERSION);
        assert_eq!(vertex_count, mesh.vertices.len() as i32);
        assert_eq!(world_count, 3); // one world triangle
        assert_eq!(phantom_count, 3); // grenade clip is solid-but-phantom
        assert_eq!(door_count, 3);
        assert_eq!(breakable_count, 0);

        let expected_len = 28
            + mesh.vertices.len() * 12
            + (world_count + phantom_count + door_count + breakable_count) as usize * 4;
        assert_eq!(bytes.len(), expected_len);
    }

    /// One triangle per group (world/phantom/door/breakable), each at its own x offset, so a
    /// swap between any two groups' index lists (e.g. door <-> breakable) is caught by checking
    /// which offset each group's indices actually point at, not just the group sizes.
    #[test]
    fn group_indices_point_at_their_own_triangles() {
        let mut mesh = CollisionMesh::new();
        let world_attr = mesh.add_attribute(attr("Default", &[])).unwrap();
        let phantom_attr = mesh.add_attribute(attr("EntityPhysicsClip", &[])).unwrap();
        let door_attr = mesh.add_attribute(attr("EntityDoor", &[])).unwrap();
        let breakable_attr = mesh.add_attribute(attr("EntityBreakable", &[])).unwrap();
        push_tri(&mut mesh, world_attr, 0.0);
        push_tri(&mut mesh, phantom_attr, 10.0);
        push_tri(&mut mesh, door_attr, 20.0);
        push_tri(&mut mesh, breakable_attr, 30.0);

        let bytes = sm3d(&mesh);
        let vertex_count = i32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        let world_count = i32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let phantom_count = i32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;
        let door_count = i32::from_le_bytes(bytes[20..24].try_into().unwrap()) as usize;
        let breakable_count = i32::from_le_bytes(bytes[24..28].try_into().unwrap()) as usize;
        assert_eq!(
            (world_count, phantom_count, door_count, breakable_count),
            (3, 3, 3, 3)
        );

        let vertices_start = 28;
        let vertex_x = |idx: u32| -> f32 {
            let off = vertices_start + idx as usize * 12;
            f32::from_le_bytes(bytes[off..off + 4].try_into().unwrap())
        };
        let indices_start = vertices_start + vertex_count * 12;
        let group_min_x = |start: usize, count: usize| -> f32 {
            (0..count)
                .map(|i| {
                    let off = start + i * 4;
                    let idx = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
                    vertex_x(idx)
                })
                .fold(f32::INFINITY, f32::min)
        };
        let world_start = indices_start;
        let phantom_start = world_start + world_count * 4;
        let door_start = phantom_start + phantom_count * 4;
        let breakable_start = door_start + door_count * 4;

        assert_eq!(group_min_x(world_start, world_count), 0.0);
        assert_eq!(group_min_x(phantom_start, phantom_count), 10.0);
        assert_eq!(group_min_x(door_start, door_count), 20.0);
        assert_eq!(group_min_x(breakable_start, breakable_count), 30.0);
    }
}
