//! Shared `--constants` resolution (`throw`, `smoke`, `sightline`, `solve`).

use std::path::Path;

use sim::ThrowConstants;

/// Reference default (`MeshSetup.cs:LoadConstants`: `<geo dir>/throw-constants.json`);
/// we have no single geo-file directory, so this is relative to the cwd, matching
/// `calibrate`'s own `--out` default.
const DEFAULT_CONSTANTS_PATH: &str = "data/throw-constants.json";

/// Loads `--constants` if given; otherwise auto-loads `data/throw-constants.json`
/// when it exists (printing `throw constants: <path>`, like `MeshSetup.cs:116-127`),
/// else falls back to `sim`'s built-in defaults.
pub fn resolve_constants(explicit: Option<&Path>) -> anyhow::Result<ThrowConstants> {
    let path = match explicit {
        Some(p) => Some(p.to_path_buf()),
        None => {
            let default = Path::new(DEFAULT_CONSTANTS_PATH);
            default.is_file().then(|| default.to_path_buf())
        }
    };
    match path {
        Some(p) => {
            println!("throw constants: {}", p.display());
            Ok(ThrowConstants::load_or_default(Some(&p))?)
        }
        None => Ok(ThrowConstants::default()),
    }
}
