//! Resource type from a path's extension (`docs/FORMATS.md` section 2.4;
//! `VRF/Resource/Enums/ResourceType.cs`, `Utils/ResourceTypeExtensions.cs`).

/// The kind of resource, determined from the path's extension (with the
/// trailing `_c` stripped). `NTRO`/`REDI`-based fallback by compiler
/// identifier is out of scope (CS2 resources are KV3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceType {
    Model,
    PhysicsCollisionMesh,
    World,
    WorldNode,
    EntityLump,
    ResourceManifest,
    Material,
    Texture,
    VData,
    Map,
    /// Any other extension, without its leading dot or trailing `_c`.
    Other(String),
}

/// Determines a [`ResourceType`] from a resource's path: takes the file
/// name's extension (without the leading dot), then strips a trailing `_c`
/// if present, then matches it against the known Source 2 resource
/// extensions (`docs/FORMATS.md` section 2.4).
pub fn resource_type_from_path(path: &str) -> ResourceType {
    let file_name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let ext = match file_name.rsplit_once('.') {
        Some((_, ext)) => ext,
        None => "",
    };
    let ext = ext.strip_suffix("_c").unwrap_or(ext);
    match ext {
        "vmdl" => ResourceType::Model,
        "vphys" => ResourceType::PhysicsCollisionMesh,
        "vwrld" => ResourceType::World,
        "vwnod" => ResourceType::WorldNode,
        "vents" => ResourceType::EntityLump,
        "vrman" => ResourceType::ResourceManifest,
        "vmat" => ResourceType::Material,
        "vtex" => ResourceType::Texture,
        "vdata" => ResourceType::VData,
        "vmap" => ResourceType::Map,
        other => ResourceType::Other(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_dot_and_trailing_c() {
        assert_eq!(
            resource_type_from_path("maps/de_mirage/world_physics.vmdl_c"),
            ResourceType::Model
        );
        assert_eq!(
            resource_type_from_path("materials/foo.vmat_c"),
            ResourceType::Material
        );
    }

    #[test]
    fn non_resource_extension_is_other() {
        assert_eq!(
            resource_type_from_path("maps/de_mirage.nav"),
            ResourceType::Other("nav".to_string())
        );
    }

    #[test]
    fn unknown_extension_is_other() {
        assert_eq!(
            resource_type_from_path("foo.bar_c"),
            ResourceType::Other("bar".to_string())
        );
    }

    #[test]
    fn no_dot_at_all_is_other_empty() {
        assert_eq!(
            resource_type_from_path("maps/README"),
            ResourceType::Other(String::new())
        );
    }
}
