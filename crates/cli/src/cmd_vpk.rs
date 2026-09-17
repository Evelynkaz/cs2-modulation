//! `cs2mod vpk ls|cat|verify` dev commands.

use std::io::{self, Write};
use std::path::Path;

use anyhow::Context;
use s2fmt::vpk::Vpk;

use crate::game_path::{ensure_write_allowed, resolve_vpk_path};

pub fn ls(
    vpk: &str,
    game: Option<&Path>,
    filter: Option<&str>,
    ext: Option<&str>,
    long: bool,
) -> anyhow::Result<()> {
    let path = resolve_vpk_path(vpk, game)?;
    let archive = Vpk::open(&path).with_context(|| format!("failed to open {}", path.display()))?;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    for entry in archive.entries() {
        if let Some(f) = filter
            && !entry.path.contains(f)
        {
            continue;
        }
        if let Some(e) = ext
            && entry.extension() != e
        {
            continue;
        }
        if long {
            writeln!(
                out,
                "{:>10}  archive={:<5}  crc32=0x{:08X}  {}",
                entry.total_len(),
                entry.archive_index,
                entry.crc32,
                entry.path
            )?;
        } else {
            writeln!(out, "{}", entry.path)?;
        }
    }
    Ok(())
}

pub fn cat(vpk: &str, game: Option<&Path>, entry_path: &str, out: &Path) -> anyhow::Result<()> {
    ensure_write_allowed(out)?;
    let path = resolve_vpk_path(vpk, game)?;
    let archive = Vpk::open(&path).with_context(|| format!("failed to open {}", path.display()))?;
    let entry = archive
        .find(entry_path)
        .with_context(|| format!("no entry {entry_path:?} in {}", path.display()))?;
    let data = archive
        .read_verified(entry)
        .with_context(|| format!("failed to read {entry_path:?}"))?;
    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(out, &data).with_context(|| format!("failed to write {}", out.display()))?;
    writeln!(
        io::stdout().lock(),
        "wrote {} bytes to {}",
        data.len(),
        out.display()
    )?;
    Ok(())
}

pub fn verify(vpk: &str, game: Option<&Path>) -> anyhow::Result<()> {
    let path = resolve_vpk_path(vpk, game)?;
    let archive = Vpk::open(&path).with_context(|| format!("failed to open {}", path.display()))?;

    let mut checked = 0usize;
    let mut failures: Vec<(String, String)> = Vec::new();
    for entry in archive.entries() {
        match archive.read_verified(entry) {
            Ok(_) => checked += 1,
            Err(e) => failures.push((entry.path.clone(), e.to_string())),
        }
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(
        out,
        "checked {checked} entries, {} failures",
        failures.len()
    )?;
    for (path, err) in &failures {
        writeln!(out, "  FAIL {path}: {err}")?;
    }
    drop(out);
    if !failures.is_empty() {
        anyhow::bail!(
            "{} of {} entries failed CRC verification",
            failures.len(),
            checked + failures.len()
        );
    }
    Ok(())
}
