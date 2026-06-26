//! Extract packages into the per-game package cache.
//!
//! Mirrors r2modman's `<game>/cache/<full_name>/<version>/` layout. This cache is
//! the install source and the restore source for re-enabling `State`-tracked
//! mods, so it must persist across installs.

#![deny(missing_docs)]

use crate::error::{Error, Result};
use crate::extract::{extract_zip, should_extract};
use std::path::{Path, PathBuf};

/// The cache directory for one package version:
/// `<base>/<game>/cache/<full_name>/<version>`.
pub fn package_cache_dir(base: &Path, game: &str, full_name: &str, version: &str) -> PathBuf {
  base.join(game).join("cache").join(full_name).join(version)
}

/// Extracts `archive` into the package cache directory if that version is not
/// already present, and returns the directory. Uses [`should_extract`] so an
/// unchanged version is not re-extracted.
pub fn ensure_extracted(
  base: &Path,
  game: &str,
  full_name: &str,
  version: &str,
  archive: &Path,
) -> Result<PathBuf> {
  let dir = package_cache_dir(base, game, full_name, version);

  if should_extract(&dir, version)? {
    extract_zip(archive, &dir)?;
  }

  if !dir.exists() {
    return Err(Error::Profile(format!(
      "package cache {} missing after extraction",
      dir.display()
    )));
  }

  Ok(dir)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io::{Cursor, Write};
  use tempfile::tempdir;
  use zip::write::{FileOptions, ZipWriter};

  fn build_zip() -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let opts: FileOptions<'_, ()> =
      FileOptions::default().compression_method(zip::CompressionMethod::Stored);

    zip.start_file("manifest.json", opts).unwrap();
    zip.write_all(br#"{"version_number":"1.0.0"}"#).unwrap();
    zip.start_file("plugins/Mod.dll", opts).unwrap();
    zip.write_all(b"dll").unwrap();

    zip.finish().unwrap().into_inner()
  }

  #[test]
  fn ensure_extracted_places_and_is_idempotent() {
    let dir = tempdir().unwrap();
    let base = dir.path();

    let archive = base.join("Author-Mod-1.0.0.zip");
    std::fs::write(&archive, build_zip()).unwrap();

    let out = ensure_extracted(base, "valheim", "Author-Mod", "1.0.0", &archive).unwrap();

    assert_eq!(
      out,
      package_cache_dir(base, "valheim", "Author-Mod", "1.0.0")
    );
    assert!(out.join("plugins/Mod.dll").exists());

    // Second call is a no-op (same version present) and still returns the dir.
    let again = ensure_extracted(base, "valheim", "Author-Mod", "1.0.0", &archive).unwrap();

    assert!(again.join("plugins/Mod.dll").exists());
  }
}
