//! Generic, CRC-verified zip extraction primitives.
//!
//! These are the reusable archive operations shared by every Thunderstore mod
//! manager: extracting a package zip with per-entry CRC32 verification and
//! path-traversal protection, deciding whether a downloaded version differs
//! from what is already on disk, and parsing the `Owner-Name-Version.zip`
//! filename convention. Where the extracted files ultimately land is install
//! policy and lives in the caller (see [`crate::install`]).

#![deny(missing_docs)]

use crate::error::{Error, Result};
use crate::manifest::Manifest;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{debug, error, warn};

/// Size of the reusable read buffer used to stream each archive entry to disk.
/// This bounds extraction memory to a constant regardless of entry size; it is an
/// IO buffer, not a cap on legitimate file size.
const EXTRACT_CHUNK_SIZE: usize = 64 * 1024;

/// Resource limits enforced while extracting an archive, to defend against
/// decompression bombs (an archive that is small on disk but expands to an
/// enormous number of entries or bytes).
///
/// These bound *disk* usage and entry count; extraction memory is already a
/// fixed `EXTRACT_CHUNK_SIZE` buffer regardless of these values. The defaults
/// are generous enough for real mod and texture packs while still rejecting
/// pathological archives; tune them with [`extract_zip_with_limits`] when a
/// caller needs different bounds.
#[derive(Debug, Clone, Copy)]
pub struct ExtractLimits {
  /// Maximum number of entries (files + directories) an archive may contain.
  pub max_entries: u64,
  /// Maximum uncompressed size of any single entry, in bytes.
  pub max_file_bytes: u64,
  /// Maximum total uncompressed size across all entries, in bytes.
  pub max_total_bytes: u64,
}

impl Default for ExtractLimits {
  fn default() -> Self {
    Self {
      max_entries: 100_000,
      max_file_bytes: 1 << 30,  // 1 GiB
      max_total_bytes: 4 << 30, // 4 GiB
    }
  }
}

/// Computes a unique sibling temp path next to `path` so a partially-extracted
/// entry can be written, CRC-verified, then atomically renamed into place — an
/// interrupted or corrupt entry never leaves a usable file behind.
fn temp_sibling_path(path: &Path) -> PathBuf {
  static COUNTER: AtomicU64 = AtomicU64::new(0);

  let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
  let pid = std::process::id();
  let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("entry");
  let tmp_name = format!(".{name}.{pid}.{seq}.partial");

  match path.parent() {
    Some(parent) => parent.join(tmp_name),
    None => PathBuf::from(tmp_name),
  }
}

/// Streams a single archive entry to `tmp_path`, holding only a fixed-size buffer
/// in memory, and verifies its CRC32 against `expected_crc32`.
///
/// Enforces the byte limits in `limits` against the *actual* number of bytes
/// streamed (not the archive's declared sizes, which an attacker controls):
/// `max_file_bytes` for this entry and `max_total_bytes` for the cumulative
/// `total_written` across the whole archive. Exceeding either returns
/// [`Error::ExtractLimitExceeded`].
///
/// Returns [`Error::ChecksumMismatch`] when the computed CRC differs from the one
/// recorded in the archive. The caller is responsible for renaming `tmp_path` into
/// place on success and removing it on any error.
fn write_entry<R: Read>(
  entry: &mut R,
  buffer: &mut [u8],
  tmp_path: &Path,
  expected_crc32: u32,
  dest: &Path,
  limits: &ExtractLimits,
  total_written: &mut u64,
) -> Result<()> {
  let mut outfile = fs::File::create(tmp_path)?;
  let mut hasher = crc32fast::Hasher::new();
  let mut file_written: u64 = 0;

  loop {
    let read = entry.read(&mut buffer[..])?;

    if read == 0 {
      break;
    }

    file_written += read as u64;
    *total_written += read as u64;

    check_extract_size(dest, file_written, *total_written, limits)?;

    hasher.update(&buffer[..read]);
    outfile.write_all(&buffer[..read])?;
  }

  let actual_crc32 = hasher.finalize();

  if actual_crc32 != expected_crc32 {
    error!(
      "CRC32 verification failed for {}: expected {:08x}, got {:08x}",
      dest.display(),
      expected_crc32,
      actual_crc32
    );

    return Err(Error::ChecksumMismatch {
      path: dest.display().to_string(),
      expected: expected_crc32,
      actual: actual_crc32,
    });
  }

  outfile.flush()?;

  Ok(())
}

/// Enforces the streamed-byte limits for one entry: the per-entry
/// `max_file_bytes` and the cumulative `max_total_bytes` across the archive.
///
/// Checked against the *actual* bytes streamed (which an attacker cannot forge,
/// unlike an archive's declared sizes). Pure — no I/O — so the decompression-bomb
/// guard is unit-testable. Returns [`Error::ExtractLimitExceeded`] on breach; this
/// stays distinct from the network `SizeCap`/[`Error::SizeLimitExceeded`] path
/// because archive limits also bound a non-byte quantity (entry count).
fn check_extract_size(
  dest: &Path,
  file_written: u64,
  total_written: u64,
  limits: &ExtractLimits,
) -> Result<()> {
  if file_written > limits.max_file_bytes {
    return Err(Error::ExtractLimitExceeded(format!(
      "entry {} exceeds max file size of {} bytes",
      dest.display(),
      limits.max_file_bytes
    )));
  }

  if total_written > limits.max_total_bytes {
    return Err(Error::ExtractLimitExceeded(format!(
      "archive exceeds max total uncompressed size of {} bytes",
      limits.max_total_bytes
    )));
  }

  Ok(())
}

/// Joins a validated archive entry path onto `dest_dir`, or returns `None` if the
/// entry would escape the destination.
///
/// `entry` is the result of [`zip::read::ZipFile::enclosed_name`] (already `None`
/// for a rejected path). As defense in depth on top of that, this rejects any
/// absolute, root, drive-prefix, or `..` component itself, so the traversal guard
/// is a single pure function testable against crafted paths without a real
/// archive.
fn safe_join(dest_dir: &Path, entry: Option<&Path>) -> Option<PathBuf> {
  let entry = entry?;

  for component in entry.components() {
    match component {
      Component::Normal(_) | Component::CurDir => {}
      Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
    }
  }

  Some(dest_dir.join(entry))
}

/// The permissions to force on an extracted entry, or `None` to keep whatever
/// [`fs::File::create`] and the process umask already produced.
///
/// Only the archive's *execute* bits are consulted, and only to widen the file to
/// `0o755`. A loader package ships `run_bepinex.sh` / `start_game_bepinex.sh`
/// marked executable and the generated Steam wrapper `exec`s whichever one it
/// finds, so an entry that loses that bit turns a modded launch into a bare
/// `Permission denied`, which under Steam surfaces only as the game silently
/// never starting.
///
/// The recorded mode is deliberately **not** reproduced wholesale. A downloaded
/// package is untrusted input: applying it verbatim would honour `setuid`,
/// `setgid`, and sticky bits, and would let an entry declare itself
/// world-writable. Widening to a fixed `0o755` can do neither. An entry with no
/// recorded mode at all, which is every archive built on Windows, is left alone
/// rather than guessed at.
#[cfg(unix)]
fn entry_permissions(unix_mode: Option<u32>) -> Option<fs::Permissions> {
  use std::os::unix::fs::PermissionsExt;

  let mode = unix_mode?;

  if mode & 0o111 == 0 {
    return None;
  }

  Some(fs::Permissions::from_mode(0o755))
}

/// Applies an archive entry's recorded execute bit to the file just extracted to
/// `path`. See [`entry_permissions`] for why only that bit is honoured.
///
/// A no-op off Unix, where a zip entry's Unix mode has nothing to map onto.
fn apply_entry_mode(path: &Path, unix_mode: Option<u32>) -> Result<()> {
  #[cfg(unix)]
  if let Some(permissions) = entry_permissions(unix_mode) {
    fs::set_permissions(path, permissions)?;
  }

  #[cfg(not(unix))]
  let _ = (path, unix_mode);

  Ok(())
}

/// Extracts a zip archive to `dest_dir` with per-entry CRC32 verification and
/// the default [`ExtractLimits`].
///
/// Entry paths are validated with [`zip::read::ZipFile::enclosed_name`] so that
/// archive entries containing `..` or absolute paths cannot escape `dest_dir`.
///
/// # Errors
///
/// Returns an error if the file is not a valid zip archive, if IO fails, if
/// CRC32 verification fails for any extracted entry ([`Error::ChecksumMismatch`]),
/// or if the archive exceeds a default extraction limit
/// ([`Error::ExtractLimitExceeded`]).
pub fn extract_zip(zip_path: &Path, dest_dir: &Path) -> Result<()> {
  extract_zip_with_limits(zip_path, dest_dir, &ExtractLimits::default())
}

/// Extracts a zip archive to `dest_dir` with per-entry CRC32 verification,
/// enforcing the supplied [`ExtractLimits`] to defend against decompression
/// bombs.
///
/// Entry paths are validated with [`zip::read::ZipFile::enclosed_name`] so that
/// archive entries containing `..` or absolute paths cannot escape `dest_dir`.
/// The entry count and the actual streamed byte totals are checked against
/// `limits`; CRC32 is an integrity check only and does not authenticate content.
///
/// # Errors
///
/// Returns an error if the file is not a valid zip archive, if IO fails, if
/// CRC32 verification fails for any extracted entry ([`Error::ChecksumMismatch`]),
/// or if the archive exceeds a limit in `limits`
/// ([`Error::ExtractLimitExceeded`]).
pub fn extract_zip_with_limits(
  zip_path: &Path,
  dest_dir: &Path,
  limits: &ExtractLimits,
) -> Result<()> {
  let file = fs::File::open(zip_path)?;
  let mut archive = zip::ZipArchive::new(file)?;

  if archive.len() as u64 > limits.max_entries {
    return Err(Error::ExtractLimitExceeded(format!(
      "archive has {} entries, exceeding the limit of {}",
      archive.len(),
      limits.max_entries
    )));
  }

  fs::create_dir_all(dest_dir)?;

  let mut total_written: u64 = 0;
  let mut buffer = vec![0u8; EXTRACT_CHUNK_SIZE];

  for i in 0..archive.len() {
    let mut file_in_archive = archive.by_index(i)?;
    let enclosed = file_in_archive.enclosed_name();
    let outpath = match safe_join(dest_dir, enclosed.as_deref()) {
      Some(path) => path,
      None => {
        warn!("Invalid file path in zip: {:?}", file_in_archive.name());
        continue;
      }
    };

    let comment = file_in_archive.comment();

    if !comment.is_empty() {
      debug!("File {} comment: {:?}", i, comment);
    }

    if file_in_archive.name().ends_with('/') {
      debug!("Creating directory {}", outpath.display());
      fs::create_dir_all(&outpath)?;
    } else {
      debug!("Extracting file {} to {}", i, outpath.display());

      if let Some(parent) = outpath.parent()
        && !parent.exists()
      {
        fs::create_dir_all(parent)?;
      }

      let expected_crc32 = file_in_archive.crc32();
      // Read before streaming: `write_entry` borrows the entry mutably, and the
      // mode is needed after the file is renamed into place.
      let unix_mode = file_in_archive.unix_mode();
      let tmp_path = temp_sibling_path(&outpath);

      // Stream the entry to a temp file, verifying the CRC as we go. Any failure
      // (IO, a short/corrupt entry the `zip` reader rejects, or a CRC mismatch)
      // must remove the temp file so a corrupt entry never lands at `outpath`.
      match write_entry(
        &mut file_in_archive,
        &mut buffer,
        &tmp_path,
        expected_crc32,
        &outpath,
        limits,
        &mut total_written,
      ) {
        Ok(()) => {
          // Applied to the temp file rather than to `outpath`, so the entry
          // becomes visible at its final path already carrying its final mode
          // instead of appearing briefly as non-executable. Any failure here is
          // treated exactly like a failed rename: remove the temp file so a
          // half-finished entry never lands.
          if let Err(err) = apply_entry_mode(&tmp_path, unix_mode) {
            let _ = fs::remove_file(&tmp_path);

            return Err(err);
          }

          // A failed rename must also remove the temp file, so a CRC-verified
          // entry that cannot be moved into place leaves no orphan behind.
          if let Err(err) = fs::rename(&tmp_path, &outpath) {
            let _ = fs::remove_file(&tmp_path);

            return Err(err.into());
          }

          debug!("CRC32 verification successful for {}", outpath.display());
        }
        Err(err) => {
          let _ = fs::remove_file(&tmp_path);

          return Err(err);
        }
      }
    }
  }

  Ok(())
}

/// Calculates the CRC32 checksum for a byte buffer.
#[cfg(test)]
fn calculate_crc32(data: &[u8]) -> u32 {
  let mut hasher = crc32fast::Hasher::new();
  hasher.update(data);

  hasher.finalize()
}

/// Determines whether a mod should be (re)extracted based on version comparison.
///
/// Returns `Ok(true)` if `mod_dir` has no `manifest.json`, if its manifest has a
/// different `version_number` than `version`, or if its manifest cannot be
/// parsed. Returns `Ok(false)` only when an existing manifest already records
/// the same version.
pub fn should_extract(mod_dir: &Path, version: &str) -> Result<bool> {
  let manifest_path = mod_dir.join("manifest.json");

  let existing_version = if manifest_path.exists() {
    match Manifest::from_file(&manifest_path) {
      Ok(manifest) => Some(manifest.version_number),
      Err(err) => {
        warn!(
          "Failed to parse manifest in {}: {}; re-extracting",
          mod_dir.display(),
          err
        );

        None
      }
    }
  } else {
    debug!(
      "No manifest in {}, extracting version {}",
      mod_dir.display(),
      version
    );

    None
  };

  let reextract = should_reextract(existing_version.as_deref(), version);

  if let Some(existing) = &existing_version {
    if reextract {
      debug!(
        "{} version changed from {} to {}, extracting",
        mod_dir.display(),
        existing,
        version
      );
    } else {
      debug!(
        "{} already has version {}, skipping",
        mod_dir.display(),
        version
      );
    }
  }

  Ok(reextract)
}

/// The pure re-extract decision, given the version an existing on-disk manifest
/// records (`None` when there is no manifest, or it could not be parsed) and the
/// `requested` version.
///
/// Re-extract unless an existing manifest already records exactly the requested
/// version. Extracted from [`should_extract`] so the decision is testable without
/// writing manifest files to disk.
fn should_reextract(existing_version: Option<&str>, requested: &str) -> bool {
  match existing_version {
    Some(existing) => existing != requested,
    None => true,
  }
}

/// Parses a Thunderstore package zip filename of the form
/// `Owner-Name-Version.zip`.
///
/// Returns the package full name (`Owner-Name`) and the version string, or
/// `None` if the filename does not contain at least three `-`-separated parts.
///
/// Only the first two `-` delimiters are significant: the owner and name never
/// contain `-`, but everything after the second delimiter is taken verbatim as
/// the version, so a version that itself contains `-` (e.g. a `1.0.0-beta`
/// prerelease) is preserved rather than truncated.
pub fn parse_package_filename(file_name: &str) -> Option<(String, String)> {
  let stem = file_name.strip_suffix(".zip").unwrap_or(file_name);
  let parts: Vec<&str> = stem.splitn(3, '-').collect();

  if parts.len() < 3 {
    return None;
  }

  let full_name = format!("{}-{}", parts[0], parts[1]);
  let version = parts[2].to_string();

  Some((full_name, version))
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs::File;
  use std::io::Write;
  use tempfile::tempdir;
  use zip::write::{FileOptions, ZipWriter};

  #[test]
  fn test_calculate_crc32() {
    let data = b"test data for crc32 calculation";
    let crc = calculate_crc32(data);

    let expected_crc: u32 = 1707861357;

    assert_eq!(crc, expected_crc);
  }

  #[test]
  fn safe_join_rejects_traversal_and_absolute() {
    let base = Path::new("/dest");

    assert_eq!(safe_join(base, None), None);
    assert_eq!(safe_join(base, Some(Path::new("../escape"))), None);
    assert_eq!(safe_join(base, Some(Path::new("a/../../b"))), None);
    assert_eq!(safe_join(base, Some(Path::new("/etc/passwd"))), None);

    assert_eq!(
      safe_join(base, Some(Path::new("plugins/mod.dll"))),
      Some(PathBuf::from("/dest/plugins/mod.dll"))
    );
    assert!(safe_join(base, Some(Path::new("./ok.txt"))).is_some());
  }

  #[test]
  fn should_reextract_truth_table() {
    // No existing manifest (or an unparseable one) => re-extract.
    assert!(should_reextract(None, "1.0.0"));
    // Same version already present => skip.
    assert!(!should_reextract(Some("1.0.0"), "1.0.0"));
    // Different version present => re-extract.
    assert!(should_reextract(Some("1.0.0"), "1.0.1"));
  }

  #[test]
  fn check_extract_size_enforces_both_caps() {
    let limits = ExtractLimits {
      max_entries: 1,
      max_file_bytes: 10,
      max_total_bytes: 20,
    };

    // At the caps is allowed.
    assert!(check_extract_size(Path::new("f"), 10, 20, &limits).is_ok());

    // Over the per-file cap.
    assert!(matches!(
      check_extract_size(Path::new("f"), 11, 11, &limits).unwrap_err(),
      Error::ExtractLimitExceeded(_)
    ));

    // Over the whole-archive cap.
    assert!(matches!(
      check_extract_size(Path::new("f"), 5, 21, &limits).unwrap_err(),
      Error::ExtractLimitExceeded(_)
    ));
  }

  #[test]
  fn temp_sibling_path_is_unique_and_sibling() {
    let base = Path::new("/tmp/x/file.dll");

    let a = temp_sibling_path(base);
    let b = temp_sibling_path(base);

    assert_ne!(a, b);
    assert_eq!(a.parent(), Some(Path::new("/tmp/x")));
  }

  #[test]
  fn write_entry_streams_verifies_crc_and_enforces_cap() {
    let dir = tempdir().unwrap();
    let data = b"hello world, this is a streamed entry";
    let crc = calculate_crc32(data);
    let limits = ExtractLimits::default();

    // Success: a small buffer forces multiple read iterations; the file lands and
    // the running total reflects every byte.
    let tmp = dir.path().join("ok.partial");
    let mut reader = std::io::Cursor::new(data.to_vec());
    let mut buffer = vec![0u8; 4];
    let mut total = 0u64;

    write_entry(
      &mut reader,
      &mut buffer,
      &tmp,
      crc,
      Path::new("ok"),
      &limits,
      &mut total,
    )
    .unwrap();

    assert_eq!(std::fs::read(&tmp).unwrap(), data);
    assert_eq!(total, data.len() as u64);

    // CRC mismatch is reported.
    let tmp2 = dir.path().join("bad.partial");
    let mut reader = std::io::Cursor::new(data.to_vec());
    let mut buffer = vec![0u8; 64];
    let mut total = 0u64;

    let err = write_entry(
      &mut reader,
      &mut buffer,
      &tmp2,
      0xDEAD_BEEF,
      Path::new("bad"),
      &limits,
      &mut total,
    )
    .unwrap_err();

    assert!(matches!(err, Error::ChecksumMismatch { .. }));

    // The per-file byte cap is enforced against the actual streamed bytes.
    let tmp3 = dir.path().join("big.partial");
    let tight = ExtractLimits {
      max_file_bytes: 4,
      ..ExtractLimits::default()
    };
    let mut reader = std::io::Cursor::new(vec![0u8; 10]);
    let mut buffer = vec![0u8; 64];
    let mut total = 0u64;

    let err = write_entry(
      &mut reader,
      &mut buffer,
      &tmp3,
      0,
      Path::new("big"),
      &tight,
      &mut total,
    )
    .unwrap_err();

    assert!(matches!(err, Error::ExtractLimitExceeded(_)));
  }

  #[test]
  fn test_parse_package_filename() {
    assert_eq!(
      parse_package_filename("Owner-ModName-1.0.0.zip"),
      Some(("Owner-ModName".to_string(), "1.0.0".to_string()))
    );

    assert_eq!(parse_package_filename("InvalidFile.zip"), None);
  }

  #[test]
  fn test_parse_package_filename_preserves_hyphenated_version() {
    assert_eq!(
      parse_package_filename("Owner-ModName-1.0.0-beta.1.zip"),
      Some(("Owner-ModName".to_string(), "1.0.0-beta.1".to_string()))
    );
  }

  #[test]
  fn test_extract_zip_with_valid_paths() {
    let temp_dir = tempdir().unwrap();
    let output_dir = temp_dir.path().join("output");
    fs::create_dir_all(&output_dir).unwrap();

    let zip_path = temp_dir.path().join("test_invalid_paths.zip");
    let file = File::create(&zip_path).unwrap();
    let mut zip = ZipWriter::new(file);

    let options: FileOptions<'_, ()> =
      FileOptions::default().compression_method(zip::CompressionMethod::Stored);

    zip.start_file("valid_file.txt", options).unwrap();
    zip.write_all(b"Valid file content").unwrap();

    zip.finish().unwrap();

    let result = extract_zip(&zip_path, &output_dir);

    assert!(result.is_ok());

    let valid_file = output_dir.join("valid_file.txt");
    assert!(valid_file.exists());
  }

  /// A loader package ships `start_game_bepinex.sh` / `run_bepinex.sh` marked
  /// executable in the archive, and the generated Steam wrapper `exec`s whichever
  /// one it finds. Dropping the archive's execute bit makes that `exec` fail with
  /// `Permission denied`, which under Steam surfaces only as the game silently
  /// never starting, so the bit has to survive extraction.
  #[test]
  #[cfg(unix)]
  fn test_extract_zip_preserves_the_executable_bit() {
    use std::os::unix::fs::PermissionsExt;

    let temp_dir = tempdir().unwrap();
    let output_dir = temp_dir.path().join("output");

    let zip_path = temp_dir.path().join("test_modes.zip");
    let file = File::create(&zip_path).unwrap();
    let mut zip = ZipWriter::new(file);

    let executable: FileOptions<'_, ()> = FileOptions::default()
      .compression_method(zip::CompressionMethod::Stored)
      .unix_permissions(0o755);
    let plain: FileOptions<'_, ()> = FileOptions::default()
      .compression_method(zip::CompressionMethod::Stored)
      .unix_permissions(0o644);

    zip.start_file("start_game_bepinex.sh", executable).unwrap();
    zip.write_all(b"#!/bin/sh\n").unwrap();

    zip.start_file("mods.yml", plain).unwrap();
    zip.write_all(b"[]").unwrap();

    zip.finish().unwrap();

    extract_zip(&zip_path, &output_dir).unwrap();

    let script_mode = fs::metadata(output_dir.join("start_game_bepinex.sh"))
      .unwrap()
      .permissions()
      .mode();

    assert!(
      script_mode & 0o111 != 0,
      "executable entry extracted without an execute bit: {script_mode:o}"
    );

    // A non-executable entry must not be widened just because a sibling was
    // executable: only the archive's own execute bits are honoured.
    let plain_mode = fs::metadata(output_dir.join("mods.yml"))
      .unwrap()
      .permissions()
      .mode();

    assert!(
      plain_mode & 0o111 == 0,
      "plain entry gained an execute bit: {plain_mode:o}"
    );
  }

  /// An archive built on Windows records no Unix mode at all. Those entries must
  /// still extract, keeping the default permissions rather than erroring or being
  /// made executable.
  #[test]
  #[cfg(unix)]
  fn test_extract_zip_handles_entries_with_no_unix_mode() {
    use std::os::unix::fs::PermissionsExt;

    let temp_dir = tempdir().unwrap();
    let output_dir = temp_dir.path().join("output");

    let zip_path = temp_dir.path().join("test_no_mode.zip");
    let file = File::create(&zip_path).unwrap();
    let mut zip = ZipWriter::new(file);

    let options: FileOptions<'_, ()> =
      FileOptions::default().compression_method(zip::CompressionMethod::Stored);

    zip.start_file("plugin.dll", options).unwrap();
    zip.write_all(b"bytes").unwrap();

    zip.finish().unwrap();

    extract_zip(&zip_path, &output_dir).unwrap();

    let mode = fs::metadata(output_dir.join("plugin.dll"))
      .unwrap()
      .permissions()
      .mode();

    assert!(
      mode & 0o111 == 0,
      "entry with no mode made executable: {mode:o}"
    );
  }

  /// `setuid`/`setgid`/sticky bits in an archive are never reproduced on disk: a
  /// downloaded package must not be able to plant a setuid file.
  #[test]
  #[cfg(unix)]
  fn test_extract_zip_drops_setuid_bits() {
    use std::os::unix::fs::PermissionsExt;

    let temp_dir = tempdir().unwrap();
    let output_dir = temp_dir.path().join("output");

    let zip_path = temp_dir.path().join("test_setuid.zip");
    let file = File::create(&zip_path).unwrap();
    let mut zip = ZipWriter::new(file);

    let options: FileOptions<'_, ()> = FileOptions::default()
      .compression_method(zip::CompressionMethod::Stored)
      .unix_permissions(0o4755);

    zip.start_file("evil.sh", options).unwrap();
    zip.write_all(b"#!/bin/sh\n").unwrap();

    zip.finish().unwrap();

    extract_zip(&zip_path, &output_dir).unwrap();

    let mode = fs::metadata(output_dir.join("evil.sh"))
      .unwrap()
      .permissions()
      .mode();

    assert!(
      mode & 0o7000 == 0,
      "setuid/setgid/sticky survived: {mode:o}"
    );
    assert!(mode & 0o111 != 0, "execute bit lost: {mode:o}");
  }

  #[test]
  fn test_extract_zip_with_directory() {
    let temp_dir = tempdir().unwrap();
    let output_dir = temp_dir.path().join("output");
    fs::create_dir_all(&output_dir).unwrap();

    let zip_path = temp_dir.path().join("test_with_dir.zip");
    let file = File::create(&zip_path).unwrap();
    let mut zip = ZipWriter::new(file);

    let options: FileOptions<'_, ()> =
      FileOptions::default().compression_method(zip::CompressionMethod::Stored);

    zip.add_directory("test_dir/", options).unwrap();

    zip.start_file("test_dir/file_in_dir.txt", options).unwrap();
    zip.write_all(b"File in directory").unwrap();

    zip.finish().unwrap();

    let result = extract_zip(&zip_path, &output_dir);

    assert!(result.is_ok());

    let dir_path = output_dir.join("test_dir");
    assert!(dir_path.exists());
    assert!(dir_path.is_dir());

    let file_in_dir = dir_path.join("file_in_dir.txt");
    assert!(file_in_dir.exists());

    let content = fs::read_to_string(&file_in_dir).unwrap();
    assert_eq!(content, "File in directory");
  }

  #[test]
  fn test_extract_zip_with_nested_directories() {
    let temp_dir = tempdir().unwrap();
    let output_dir = temp_dir.path().join("output");

    let zip_path = temp_dir.path().join("test_nested.zip");
    let file = File::create(&zip_path).unwrap();
    let mut zip = ZipWriter::new(file);

    let options: FileOptions<'_, ()> =
      FileOptions::default().compression_method(zip::CompressionMethod::Stored);

    zip
      .start_file("deep/nested/path/file.txt", options)
      .unwrap();
    zip.write_all(b"Deep nested file").unwrap();

    zip.finish().unwrap();

    let result = extract_zip(&zip_path, &output_dir);

    assert!(result.is_ok());

    let nested_file = output_dir.join("deep/nested/path/file.txt");
    assert!(nested_file.exists());

    let content = fs::read_to_string(&nested_file).unwrap();
    assert_eq!(content, "Deep nested file");
  }

  #[test]
  fn test_extract_zip_streams_large_file() {
    let temp_dir = tempdir().unwrap();
    let output_dir = temp_dir.path().join("output");

    let zip_path = temp_dir.path().join("test_large.zip");
    let file = File::create(&zip_path).unwrap();
    let mut zip = ZipWriter::new(file);

    let options: FileOptions<'_, ()> =
      FileOptions::default().compression_method(zip::CompressionMethod::Stored);

    // Larger than EXTRACT_CHUNK_SIZE so the streaming loop runs many iterations.
    let payload: Vec<u8> = (0..(64 * 1024 * 3 + 17)).map(|i| (i % 251) as u8).collect();

    zip.start_file("big.bin", options).unwrap();
    zip.write_all(&payload).unwrap();

    zip.finish().unwrap();

    let result = extract_zip(&zip_path, &output_dir);

    assert!(result.is_ok());

    let extracted = output_dir.join("big.bin");
    let content = fs::read(&extracted).unwrap();

    assert_eq!(content, payload);

    // No leftover temp/partial files from the atomic write.
    let leftover = fs::read_dir(&output_dir)
      .unwrap()
      .filter_map(|e| e.ok())
      .any(|e| e.file_name().to_string_lossy().contains(".partial"));

    assert!(!leftover, "no .partial temp files should remain");
  }

  #[test]
  fn test_extract_zip_reuses_buffer_across_entries() {
    let temp_dir = tempdir().unwrap();
    let output_dir = temp_dir.path().join("output");

    // A large entry followed by a smaller one: the read buffer is reused across
    // entries, so the smaller entry must not inherit stale bytes from the larger.
    let big: Vec<u8> = (0..(64 * 1024 * 2 + 5)).map(|i| (i % 251) as u8).collect();
    let small = b"tiny".to_vec();
    let zip_path = temp_dir.path().join("multi.zip");
    write_stored_zip(&zip_path, &[("big.bin", &big), ("small.txt", &small)]);

    let result = extract_zip(&zip_path, &output_dir);

    assert!(result.is_ok());

    let big_out = fs::read(output_dir.join("big.bin")).unwrap();
    let small_out = fs::read(output_dir.join("small.txt")).unwrap();

    assert_eq!(big_out, big);
    assert_eq!(small_out, small);
    assert!(no_partial_files(&output_dir));
  }

  #[test]
  fn test_extract_zip_leaves_no_file_on_crc_mismatch() {
    let temp_dir = tempdir().unwrap();
    let output_dir = temp_dir.path().join("output");

    let zip_path = temp_dir.path().join("test_corrupt.zip");
    let file = File::create(&zip_path).unwrap();
    let mut zip = ZipWriter::new(file);

    let options: FileOptions<'_, ()> =
      FileOptions::default().compression_method(zip::CompressionMethod::Stored);

    // Stored compression keeps the payload as raw bytes in the local entry, so
    // flipping one payload byte makes the actual CRC diverge from the stored one.
    let marker = b"UNIQUEMARKERPAYLOAD";
    let mut payload = marker.to_vec();
    payload.extend_from_slice(&[7u8; 64]);

    zip.start_file("data.bin", options).unwrap();
    zip.write_all(&payload).unwrap();

    zip.finish().unwrap();

    let mut bytes = fs::read(&zip_path).unwrap();
    let pos = bytes
      .windows(marker.len())
      .position(|w| w == marker)
      .expect("payload marker present in archive");

    // Corrupt a byte inside the payload (after the marker).
    bytes[pos + marker.len()] ^= 0xFF;
    fs::write(&zip_path, &bytes).unwrap();

    let result = extract_zip(&zip_path, &output_dir);

    // A corrupted entry must fail extraction. Depending on compression, the
    // mismatch is caught either by our CRC check (`ChecksumMismatch`) or by the
    // `zip` reader's own validation (surfaced as an IO/ZIP error) — either way
    // extraction errors and nothing is left behind.
    assert!(result.is_err());

    // The corrupt entry must not leave a usable file (nor a .partial temp).
    assert!(!output_dir.join("data.bin").exists());

    let leftover = fs::read_dir(&output_dir)
      .unwrap()
      .filter_map(|e| e.ok())
      .any(|e| e.file_name().to_string_lossy().contains(".partial"));

    assert!(!leftover, "no .partial temp files should remain");
  }

  fn write_stored_zip(zip_path: &Path, entries: &[(&str, &[u8])]) {
    let file = File::create(zip_path).unwrap();
    let mut zip = ZipWriter::new(file);

    let options: FileOptions<'_, ()> =
      FileOptions::default().compression_method(zip::CompressionMethod::Stored);

    for (name, contents) in entries {
      zip.start_file(*name, options).unwrap();
      zip.write_all(contents).unwrap();
    }

    zip.finish().unwrap();
  }

  fn no_partial_files(dir: &Path) -> bool {
    !fs::read_dir(dir)
      .unwrap()
      .filter_map(|e| e.ok())
      .any(|e| e.file_name().to_string_lossy().contains(".partial"))
  }

  #[test]
  fn test_extract_zip_rejects_too_many_entries() {
    let temp_dir = tempdir().unwrap();
    let output_dir = temp_dir.path().join("output");

    let zip_path = temp_dir.path().join("many_entries.zip");
    write_stored_zip(
      &zip_path,
      &[("a.txt", b"a"), ("b.txt", b"b"), ("c.txt", b"c")],
    );

    let limits = ExtractLimits {
      max_entries: 2,
      ..Default::default()
    };

    let result = extract_zip_with_limits(&zip_path, &output_dir, &limits);

    assert!(matches!(result, Err(Error::ExtractLimitExceeded(_))));
  }

  #[test]
  fn test_extract_zip_rejects_oversized_entry() {
    let temp_dir = tempdir().unwrap();
    let output_dir = temp_dir.path().join("output");

    let zip_path = temp_dir.path().join("big_entry.zip");
    let payload = vec![0u8; 100];
    write_stored_zip(&zip_path, &[("big.bin", &payload)]);

    let limits = ExtractLimits {
      max_file_bytes: 8,
      ..Default::default()
    };

    let result = extract_zip_with_limits(&zip_path, &output_dir, &limits);

    assert!(matches!(result, Err(Error::ExtractLimitExceeded(_))));
    assert!(!output_dir.join("big.bin").exists());
    assert!(no_partial_files(&output_dir));
  }

  #[test]
  fn test_extract_zip_rejects_exceeding_total() {
    let temp_dir = tempdir().unwrap();
    let output_dir = temp_dir.path().join("output");

    let zip_path = temp_dir.path().join("total.zip");
    let chunk = vec![7u8; 80];
    write_stored_zip(&zip_path, &[("one.bin", &chunk), ("two.bin", &chunk)]);

    // Each entry fits under max_file_bytes, but together they exceed the total.
    let limits = ExtractLimits {
      max_file_bytes: 1_000,
      max_total_bytes: 100,
      ..Default::default()
    };

    let result = extract_zip_with_limits(&zip_path, &output_dir, &limits);

    assert!(matches!(result, Err(Error::ExtractLimitExceeded(_))));
    assert!(no_partial_files(&output_dir));
  }

  #[test]
  fn test_extract_zip_within_default_limits_succeeds() {
    let temp_dir = tempdir().unwrap();
    let output_dir = temp_dir.path().join("output");

    let zip_path = temp_dir.path().join("normal.zip");
    write_stored_zip(&zip_path, &[("plugin.dll", b"content")]);

    let result = extract_zip_with_limits(&zip_path, &output_dir, &ExtractLimits::default());

    assert!(result.is_ok());
    assert!(output_dir.join("plugin.dll").exists());
  }

  #[test]
  fn test_extract_zip_rejects_path_traversal() {
    let temp_dir = tempdir().unwrap();
    let output_dir = temp_dir.path().join("output");
    fs::create_dir_all(&output_dir).unwrap();

    // A crafted archive whose entry tries to climb out of the extraction root,
    // alongside a benign entry.
    let zip_path = temp_dir.path().join("traversal.zip");
    write_stored_zip(
      &zip_path,
      &[("../escaped.txt", b"malicious"), ("safe.txt", b"benign")],
    );

    let result = extract_zip(&zip_path, &output_dir);

    // Extraction succeeds by skipping the unsafe entry, the benign file lands
    // inside the root, and nothing is written to the parent (outside the root).
    assert!(result.is_ok());
    assert!(output_dir.join("safe.txt").exists());
    assert!(!temp_dir.path().join("escaped.txt").exists());
    assert!(no_partial_files(&output_dir));
  }

  #[test]
  fn test_should_extract() {
    let temp_dir = tempdir().unwrap();
    let mod_dir = temp_dir.path().join("Owner-ModName");
    fs::create_dir_all(&mod_dir).unwrap();

    let result = should_extract(&mod_dir, "1.0.0");
    assert!(result.unwrap());

    let manifest_content = r#"{
      "version_number": "1.0.0",
      "name": "TestMod",
      "description": "Test mod description"
    }"#;

    let manifest_path = mod_dir.join("manifest.json");
    let mut file = File::create(&manifest_path).unwrap();
    file.write_all(manifest_content.as_bytes()).unwrap();

    let result = should_extract(&mod_dir, "1.0.0");
    assert!(!result.unwrap());

    let result = should_extract(&mod_dir, "2.0.0");
    assert!(result.unwrap());

    let mut file = File::create(&manifest_path).unwrap();
    file.write_all(b"invalid json content").unwrap();

    let result = should_extract(&mod_dir, "1.0.0");
    assert!(result.unwrap());
  }
}
