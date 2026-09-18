//! Collision attribute filters (grenade, player, named groups).

use thiserror::Error;

use crate::mesh::{CollisionAttribute, CollisionMesh};

/// A per-attribute solidity mask, precomputed once and applied per triangle
/// via `tri_attribute`.
#[derive(Debug, Clone)]
pub struct AttributeMask {
    solid: Vec<bool>,
}

impl AttributeMask {
    pub fn is_solid(&self, attribute: u16) -> bool {
        self.solid
            .get(usize::from(attribute))
            .copied()
            .unwrap_or(false)
    }

    pub fn solid_triangle_count(&self, mesh: &CollisionMesh) -> usize {
        mesh.tri_attribute
            .iter()
            .filter(|&&a| self.is_solid(a))
            .count()
    }
}

fn any_ci(layers: &[String], name: &str) -> bool {
    layers.iter().any(|l| l.eq_ignore_ascii_case(name))
}

/// Attribute filter for grenade flight, ported from
/// `CollisionMesh.cs:GrenadeSolidFilter` (`cs2-smoke-solver/src/Sim/CollisionMesh.cs:52-63`):
/// player/NPC clips and sky volumes do not block grenades; everything else
/// (including `csgo_grenadeclip` and `passbullets`) does, unless the group
/// explicitly excludes thrown grenades (`csgo_thrown_grenade` in
/// `interact_exclude`). Validated against real per-tick trajectories on
/// de_dust2 (see FORMATS.md §10 / CollisionMesh.cs doc comment).
pub fn grenade_mask(mesh: &CollisionMesh) -> AttributeMask {
    let solid = mesh
        .attributes
        .iter()
        .map(|a| {
            !any_ci(&a.interact_exclude, "csgo_thrown_grenade")
                && !any_ci(&a.interact_as, "playerclip")
                && !any_ci(&a.interact_as, "npcclip")
                && !any_ci(&a.interact_as, "sky")
        })
        .collect();
    AttributeMask { solid }
}

/// Attribute filter for PLAYER movement, ported from
/// `CollisionMesh.cs:PlayerSolidFilter` (`cs2-smoke-solver/src/Sim/CollisionMesh.cs:74-88`):
/// npcclip-only groups (npcclip without playerclip) do not stop players,
/// groups excluding `player` do not, `csgo_grenadeclip` never blocks players,
/// and the synthetic `EntityPhysicsClip` group (from `func_clip_vphysics`,
/// FORMATS.md §10) is grenade/NPC-only by construction.
pub fn player_mask(mesh: &CollisionMesh) -> AttributeMask {
    let solid = mesh
        .attributes
        .iter()
        .map(|a| {
            let npc_only =
                any_ci(&a.interact_as, "npcclip") && !any_ci(&a.interact_as, "playerclip");
            !npc_only
                && !any_ci(&a.interact_exclude, "player")
                && !any_ci(&a.interact_as, "csgo_grenadeclip")
                && a.name != "EntityPhysicsClip"
        })
        .collect();
    AttributeMask { solid }
}

/// Attribute filter for a caller-supplied list of group names, matching the
/// reference `--attrs "Default,default,EntitySolid"`
/// (`MeshSetup.cs:SingleTargetDefaultAttrs`): names are trimmed and matched
/// case-insensitively, exactly as `attrs.Split(',', TrimEntries).ToHashSet(
/// StringComparer.OrdinalIgnoreCase)` does (`MeshSetup.cs:53-54`). Also
/// applies the compatibility rule that requesting `EntitySolid` (in any case)
/// also pulls in `EntityDoor` and `EntityBreakable` (`MeshSetup.cs:61-65`):
/// those groups were split out of `EntitySolid` but are still solid at round
/// start, so a deployment naming `EntitySolid` literally must not silently
/// drop doors/breakables.
pub fn names_mask(mesh: &CollisionMesh, names: &[&str]) -> AttributeMask {
    let mut wanted: Vec<String> = names
        .iter()
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .collect();
    let contains_ci =
        |list: &[String], target: &str| list.iter().any(|n| n.eq_ignore_ascii_case(target));
    if contains_ci(&wanted, "EntitySolid") {
        if !contains_ci(&wanted, "EntityDoor") {
            wanted.push("EntityDoor".to_string());
        }
        if !contains_ci(&wanted, "EntityBreakable") {
            wanted.push("EntityBreakable".to_string());
        }
    }
    let solid = mesh
        .attributes
        .iter()
        .map(|a| wanted.iter().any(|n| n.eq_ignore_ascii_case(&a.name)))
        .collect();
    AttributeMask { solid }
}

/// Every attribute is solid.
pub fn all_mask(mesh: &CollisionMesh) -> AttributeMask {
    AttributeMask {
        solid: vec![true; mesh.attributes.len()],
    }
}

/// A mask from an arbitrary predicate over attribute definitions.
pub fn from_fn(mesh: &CollisionMesh, f: impl Fn(&CollisionAttribute) -> bool) -> AttributeMask {
    AttributeMask {
        solid: mesh.attributes.iter().map(f).collect(),
    }
}

#[derive(Debug, Error)]
pub enum FilterError {
    #[error(
        "unknown filter spec '{0}' (expected \"grenade\", \"player\", \"all\", or \"attrs:Name1,Name2\")"
    )]
    Unknown(String),
}

/// Parses a filter spec string: `"grenade"`, `"player"`, `"all"`, or
/// `"attrs:Default,default,EntitySolid"` (names trimmed, matched
/// case-insensitively; see [`names_mask`]).
pub fn parse_filter(spec: &str, mesh: &CollisionMesh) -> Result<AttributeMask, FilterError> {
    match spec {
        "grenade" => Ok(grenade_mask(mesh)),
        "player" => Ok(player_mask(mesh)),
        "all" => Ok(all_mask(mesh)),
        s if s.starts_with("attrs:") => {
            let names: Vec<&str> = s["attrs:".len()..].split(',').collect();
            Ok(names_mask(mesh, &names))
        }
        other => Err(FilterError::Unknown(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::MeshObject;
    use crate::mesh::ObjectKind;
    use crate::mesh::SurfaceProperty;

    fn attr(
        name: &str,
        interact_as: &[&str],
        interact_exclude: &[&str],
        synthetic: bool,
    ) -> CollisionAttribute {
        CollisionAttribute {
            name: name.to_string(),
            interact_as: interact_as.iter().map(|s| s.to_string()).collect(),
            interact_with: vec![],
            interact_exclude: interact_exclude.iter().map(|s| s.to_string()).collect(),
            synthetic,
        }
    }

    fn sample_mesh() -> CollisionMesh {
        let mut mesh = CollisionMesh::new();
        // 0: plain solid group
        mesh.add_attribute(attr("Default", &[], &[], false))
            .unwrap();
        // 1: player clip - case mixed to test case-insensitivity
        mesh.add_attribute(attr("PlayerClipGroup", &["PlayerClip"], &[], false))
            .unwrap();
        // 2: npc clip only
        mesh.add_attribute(attr("NpcClipOnly", &["npcclip"], &[], false))
            .unwrap();
        // 3: npc clip + player clip (playerclip present -> not npc-only)
        mesh.add_attribute(attr(
            "NpcAndPlayerClip",
            &["npcclip", "playerclip"],
            &[],
            false,
        ))
        .unwrap();
        // 4: sky
        mesh.add_attribute(attr("Sky", &["sky"], &[], false))
            .unwrap();
        // 5: grenade clip
        mesh.add_attribute(attr("GrenadeClip", &["csgo_grenadeclip"], &[], false))
            .unwrap();
        // 6: excludes thrown grenade
        mesh.add_attribute(attr("HeavenRailing", &[], &["csgo_thrown_grenade"], false))
            .unwrap();
        // 7: excludes player
        mesh.add_attribute(attr("PlayerExcluded", &[], &["player"], false))
            .unwrap();
        // 8: synthetic EntityPhysicsClip
        mesh.add_attribute(attr("EntityPhysicsClip", &[], &[], true))
            .unwrap();
        // 9: EntitySolid
        mesh.add_attribute(attr("EntitySolid", &[], &[], true))
            .unwrap();
        // 10: EntityDoor
        mesh.add_attribute(attr("EntityDoor", &[], &[], true))
            .unwrap();
        // 11: EntityBreakable
        mesh.add_attribute(attr("EntityBreakable", &[], &[], true))
            .unwrap();
        // 12: lowercase default (distinct name for names_mask exact match test)
        mesh.add_attribute(attr("default", &[], &[], false))
            .unwrap();
        for i in 0..mesh.attributes.len() {
            let a = i as u16;
            let o = mesh.add_object(MeshObject {
                kind: ObjectKind::WorldHull,
                classname: None,
                targetname: None,
                model: None,
                hammer_id: None,
                source_index: i as u32,
                hull_flags: None,
            });
            mesh.push_triangles(
                &[
                    [i as f32 * 10.0, 0.0, 0.0],
                    [i as f32 * 10.0 + 1.0, 0.0, 0.0],
                    [i as f32 * 10.0, 1.0, 0.0],
                ],
                &[[0, 1, 2]],
                a,
                |_| SurfaceProperty::NONE,
                o,
            )
            .unwrap();
        }
        mesh
    }

    #[test]
    fn grenade_filter_excludes_clips_and_sky_but_not_grenadeclip() {
        let mesh = sample_mesh();
        let mask = grenade_mask(&mesh);
        assert!(mask.is_solid(0)); // Default
        assert!(!mask.is_solid(1)); // PlayerClip (case-insensitive)
        assert!(!mask.is_solid(2)); // npcclip
        assert!(!mask.is_solid(3)); // npc+player clip
        assert!(!mask.is_solid(4)); // sky
        assert!(mask.is_solid(5)); // grenadeclip is solid for grenade
        assert!(!mask.is_solid(6)); // excludes thrown grenade layer -> not solid for grenade
        assert!(mask.is_solid(7)); // excludes "player" layer only, irrelevant to grenade filter
    }

    #[test]
    fn grenade_filter_respects_thrown_grenade_exclude() {
        let mut mesh = CollisionMesh::new();
        let a = mesh
            .add_attribute(attr(
                "RailingExcludesGrenade",
                &[],
                &["csgo_thrown_grenade"],
                false,
            ))
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
        let mask = grenade_mask(&mesh);
        assert!(!mask.is_solid(a));
    }

    #[test]
    fn player_filter_rules() {
        let mesh = sample_mesh();
        let mask = player_mask(&mesh);
        assert!(mask.is_solid(0)); // Default
        assert!(mask.is_solid(1)); // playerclip stops players
        assert!(!mask.is_solid(2)); // npc-clip only does not stop players
        assert!(mask.is_solid(3)); // has playerclip too -> npc_only false
        assert!(mask.is_solid(4)); // sky is solid for player filter (only grenade excludes it)
        assert!(!mask.is_solid(5)); // grenadeclip never blocks player
        assert!(!mask.is_solid(7)); // excludes player layer
        assert!(!mask.is_solid(8)); // EntityPhysicsClip name excluded
    }

    #[test]
    fn names_mask_case_insensitive_and_trimmed() {
        let mesh = sample_mesh();
        let mask = names_mask(&mesh, &["default"]);
        assert!(mask.is_solid(0)); // Default
        assert!(mask.is_solid(12)); // default
        assert!(!mask.is_solid(9)); // EntitySolid not requested

        let mask = names_mask(&mesh, &["Default", " entitysolid "]);
        assert!(mask.is_solid(0));
        assert!(mask.is_solid(9)); // EntitySolid (case-insensitive, trimmed)
        assert!(mask.is_solid(10)); // EntityDoor implied
        assert!(mask.is_solid(11)); // EntityBreakable implied
    }

    #[test]
    fn names_mask_entity_solid_implies_door_and_breakable() {
        let mesh = sample_mesh();
        let mask = names_mask(&mesh, &["Default", "default", "EntitySolid"]);
        assert!(mask.is_solid(9)); // EntitySolid
        assert!(mask.is_solid(10)); // EntityDoor
        assert!(mask.is_solid(11)); // EntityBreakable
        assert!(!mask.is_solid(8)); // EntityPhysicsClip not implied
    }

    #[test]
    fn all_mask_marks_everything_solid() {
        let mesh = sample_mesh();
        let mask = all_mask(&mesh);
        for i in 0..mesh.attributes.len() as u16 {
            assert!(mask.is_solid(i));
        }
    }

    #[test]
    fn from_fn_uses_predicate() {
        let mesh = sample_mesh();
        let mask = from_fn(&mesh, |a| a.synthetic);
        assert!(!mask.is_solid(0));
        assert!(mask.is_solid(8));
    }

    #[test]
    fn parse_filter_dispatches() {
        let mesh = sample_mesh();
        assert!(parse_filter("all", &mesh).unwrap().is_solid(0));
        assert!(parse_filter("grenade", &mesh).unwrap().is_solid(0));
        assert!(parse_filter("player", &mesh).unwrap().is_solid(0));
        let m = parse_filter("attrs:default", &mesh).unwrap();
        assert!(m.is_solid(0));
        assert!(m.is_solid(12));
        assert!(!m.is_solid(9));

        let m = parse_filter("attrs:Default, entitysolid", &mesh).unwrap();
        assert!(m.is_solid(0));
        assert!(m.is_solid(9)); // EntitySolid
        assert!(m.is_solid(10)); // EntityDoor
        assert!(m.is_solid(11)); // EntityBreakable

        assert!(parse_filter("bogus", &mesh).is_err());
    }

    #[test]
    fn solid_triangle_count_matches_solid_attributes() {
        let mesh = sample_mesh();
        let mask = all_mask(&mesh);
        assert_eq!(mask.solid_triangle_count(&mesh), mesh.triangle_count());
    }
}
