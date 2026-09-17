//! Shared helpers for the `vpk`/`res` dev commands: resolving a bare VPK
//! name against `--game`/`CS2_GAME_DIR`, and refusing to write extracted
//! game files into the repo's own git work tree.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, bail};

/// Resolves a `<VPK>` argument to an on-disk path.
///
/// - `pak01` -> `<game>/pak01_dir.vpk`.
/// - A bare name with no path separator, no `.vpk` extension, not `.`/`..`,
///   and that doesn't already exist on disk (e.g. `de_mirage`) ->
///   `<game>/maps/<name>.vpk`.
/// - Anything else (a path with a separator, a `.vpk` file, `.`/`..`, or an
///   existing file/directory) is used as a literal path (relative to the
///   current directory, or absolute), unchanged.
///
/// `<game>` is `game_dir` if given, else `CS2_GAME_DIR`; resolving a bare
/// name without either set is an error.
pub fn resolve_vpk_path(input: &str, game_dir: Option<&Path>) -> anyhow::Result<PathBuf> {
    let env_game = std::env::var_os("CS2_GAME_DIR").map(PathBuf::from);
    resolve_vpk_path_with(input, game_dir, env_game)
}

/// The testable core of [`resolve_vpk_path`], with the environment variable
/// read pulled out to a parameter.
fn resolve_vpk_path_with(
    input: &str,
    game_dir: Option<&Path>,
    env_game: Option<PathBuf>,
) -> anyhow::Result<PathBuf> {
    let is_literal = input.contains('/')
        || input.contains('\\')
        || input.to_ascii_lowercase().ends_with(".vpk")
        || input == "."
        || input == ".."
        || Path::new(input).exists();

    if !is_literal {
        let game = game_dir.map(Path::to_path_buf).or(env_game).context(
            "no --game given and CS2_GAME_DIR is not set; needed to resolve a bare VPK name",
        )?;
        return Ok(if input == "pak01" {
            game.join("pak01_dir.vpk")
        } else {
            game.join("maps").join(format!("{input}.vpk"))
        });
    }

    Ok(PathBuf::from(input))
}

/// Absolutizes `path` against the current directory, purely lexically (no
/// filesystem access, and no requirement that any component exists).
fn absolutize(path: &Path) -> anyhow::Result<PathBuf> {
    let base = if path.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir().context("failed to get current directory")?
    };
    let mut out = base;
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    Ok(out)
}

/// Canonicalizes the deepest existing ancestor of `path` (resolving
/// symlinks, junctions, and on-disk case) and re-appends whatever tail
/// components don't exist yet, unchanged. This lets the write guard see
/// through a junction/symlink for the part of the path that already exists,
/// while still working for a target file that doesn't exist yet (e.g.
/// `vpk cat`'s `--out`).
fn canonicalize_existing_prefix(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        if let Ok(canon) = std::fs::canonicalize(existing) {
            let mut out = canon;
            for component in tail.iter().rev() {
                out.push(component);
            }
            return out;
        }
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                existing = parent;
            }
            // Nothing on the path canonicalizes at all: fall back to the
            // lexical form as-is (e.g. a root that doesn't exist).
            _ => return path.to_path_buf(),
        }
    }
}

/// Walks up from `dir` to find the nearest ancestor containing `.git`
/// (a directory for a normal checkout, or a file for a linked worktree).
fn find_git_root(mut dir: &Path) -> Option<PathBuf> {
    loop {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

/// True if `component` is (ASCII-case-insensitively, on Windows) `data` or
/// `cache`.
fn is_data_or_cache(component: Component) -> bool {
    let Some(s) = component.as_os_str().to_str() else {
        return false;
    };
    if cfg!(windows) {
        s.eq_ignore_ascii_case("data") || s.eq_ignore_ascii_case("cache")
    } else {
        s == "data" || s == "cache"
    }
}

/// Refuses to write game data inside the repo's git work tree, unless it's
/// under `data/` or `cache/` (both scratch locations for this purpose).
/// Extracted game assets are Valve's, not ours to commit.
///
/// Resolves `out_path` to an absolute, canonicalized form first (so case
/// changes, `\\?\`-prefixed paths, `..` traversal, and junctions/symlinks
/// can't be used to sneak a write into the tree), then walks up from there
/// to find the nearest `.git`. If none is found, `out_path` isn't inside any
/// git work tree and the write is allowed.
pub fn ensure_write_allowed(out_path: &Path) -> anyhow::Result<()> {
    let absolute = absolutize(out_path)?;
    let canonical = canonicalize_existing_prefix(&absolute);
    let Some(git_root) = find_git_root(&canonical) else {
        return Ok(());
    };
    let relative = canonical.strip_prefix(&git_root).unwrap_or(&canonical);
    if relative.components().next().is_some_and(is_data_or_cache) {
        return Ok(());
    }
    bail!(
        "refusing to write {} inside the git work tree ({}): game files aren't checked in here; \
         use a path under 'data/' or 'cache/', or write outside the repo",
        out_path.display(),
        git_root.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    /// A `std::env::temp_dir()` subdirectory unique to one test, containing
    /// a fake `.git` directory (so it acts as its own isolated work tree),
    /// removed on drop.
    struct FakeRepo(PathBuf);

    impl FakeRepo {
        fn new(name: &str) -> Self {
            let mut dir = std::env::temp_dir();
            dir.push(format!(
                "cs2mod-game-path-test-{name}-{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(dir.join(".git")).unwrap();
            FakeRepo(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for FakeRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn resolves_bare_map_name() {
        let resolved =
            resolve_vpk_path_with("de_mirage", Some(Path::new("C:/game")), None).unwrap();
        assert_eq!(resolved, PathBuf::from("C:/game/maps/de_mirage.vpk"));
    }

    #[test]
    fn resolves_pak01() {
        let resolved = resolve_vpk_path_with("pak01", Some(Path::new("C:/game")), None).unwrap();
        assert_eq!(resolved, PathBuf::from("C:/game/pak01_dir.vpk"));
    }

    #[test]
    fn bare_name_without_game_dir_errors() {
        assert!(resolve_vpk_path_with("de_mirage", None, None).is_err());
    }

    #[test]
    fn env_game_dir_is_used_when_no_flag() {
        let resolved =
            resolve_vpk_path_with("de_mirage", None, Some(PathBuf::from("C:/game"))).unwrap();
        assert_eq!(resolved, PathBuf::from("C:/game/maps/de_mirage.vpk"));
    }

    #[test]
    fn literal_path_is_passed_through() {
        let resolved = resolve_vpk_path_with("some/dir/custom.vpk", None, None).unwrap();
        assert_eq!(resolved, PathBuf::from("some/dir/custom.vpk"));
    }

    #[test]
    fn literal_bare_vpk_extension_is_passed_through() {
        let resolved = resolve_vpk_path_with("custom.vpk", None, None).unwrap();
        assert_eq!(resolved, PathBuf::from("custom.vpk"));
    }

    #[test]
    fn dot_and_dotdot_are_literal_not_bare_names() {
        assert_eq!(
            resolve_vpk_path_with(".", None, None).unwrap(),
            PathBuf::from(".")
        );
        assert_eq!(
            resolve_vpk_path_with("..", None, None).unwrap(),
            PathBuf::from("..")
        );
    }

    #[test]
    fn existing_bare_name_on_disk_is_literal() {
        // "Cargo.toml" has no separator, no `.vpk` extension, and isn't
        // `.`/`..`, but it exists in this crate's directory (`cargo test`'s
        // cwd) -- it must be treated as a literal path, not a bare VPK name
        // looked up under a game dir.
        let result = resolve_vpk_path_with("Cargo.toml", None, None);
        assert_eq!(result.unwrap(), PathBuf::from("Cargo.toml"));
    }

    #[test]
    fn refuses_write_inside_git_work_tree() {
        let repo = FakeRepo::new("refuse");
        let target = repo.path().join("some_extracted_file.txt");
        assert!(ensure_write_allowed(&target).is_err());
    }

    #[test]
    fn allows_write_under_data_dir_not_yet_created() {
        let repo = FakeRepo::new("data-nested");
        let target = repo.path().join("data").join("nested").join("file.bin");
        assert!(ensure_write_allowed(&target).is_ok());
    }

    #[test]
    fn allows_write_under_cache_dir() {
        let repo = FakeRepo::new("cache");
        let target = repo.path().join("cache").join("sub").join("file.bin");
        assert!(ensure_write_allowed(&target).is_ok());
    }

    #[test]
    fn allows_write_outside_any_git_work_tree() {
        // A bare temp dir with no `.git` anywhere above it (assuming the OS
        // temp root itself isn't inside a git checkout).
        let target = std::env::temp_dir().join("cs2mod-test-write-guard-standalone.bin");
        assert!(ensure_write_allowed(&target).is_ok());
    }

    #[test]
    fn dotdot_traversal_out_of_data_is_refused() {
        let repo = FakeRepo::new("traversal");
        std::fs::create_dir_all(repo.path().join("data")).unwrap();
        let target = repo
            .path()
            .join("data")
            .join("..")
            .join("escape_attempt.txt");
        assert!(ensure_write_allowed(&target).is_err());
    }

    #[test]
    fn case_changed_existing_root_and_data_component_still_allowed() {
        if !cfg!(windows) {
            return; // relies on a case-insensitive filesystem.
        }
        let repo = FakeRepo::new("CaseTest");
        // Flip the case of the repo dir's own name (an *existing* ancestor,
        // resolved back to its real casing by `canonicalize`) and of "data"
        // (a *new* tail component, compared case-insensitively): both must
        // still resolve to an allowed write.
        let repo_str = repo.path().to_str().unwrap();
        let flipped_root = if repo_str.chars().any(|c| c.is_ascii_lowercase()) {
            repo_str.to_ascii_uppercase()
        } else {
            repo_str.to_ascii_lowercase()
        };
        let mut flipped = PathBuf::from(flipped_root);
        flipped.push("DATA");
        flipped.push("file.bin");
        assert!(ensure_write_allowed(&flipped).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_prefix_path_is_recognised() {
        let repo = FakeRepo::new("verbatim");
        let canonical_root = std::fs::canonicalize(repo.path()).unwrap();
        let mut verbatim = canonical_root.clone();
        verbatim.push("data");
        verbatim.push("file.bin");
        // `std::fs::canonicalize` already returns a `\\?\`-prefixed path on
        // Windows, so `verbatim` here already exercises that form; assert
        // it's still recognised as being under `data/`.
        assert!(verbatim.to_string_lossy().starts_with(r"\\?\"));
        assert!(ensure_write_allowed(&verbatim).is_ok());
    }
}
