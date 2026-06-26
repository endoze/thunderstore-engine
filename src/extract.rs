//! Generic, CRC-verified zip extraction primitives.
//!
//! These are the reusable archive operations shared by every Thunderstore mod
//! manager: extracting a package zip with per-entry CRC32 verification and
//! path-traversal protection, deciding whether a downloaded version differs
//! from what is already on disk, and parsing the `Owner-Name-Version.zip`
//! filename convention. Where the extracted files ultimately land is install
//! policy and lives in the caller (see [`crate::install`]).

use crate::error::{Error, Result};
use crate::manifest::Manifest;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use tracing::{debug, error, info, warn};

/// Extracts a zip archive to `dest_dir` with per-entry CRC32 verification.
///
/// Entry paths are validated with [`zip::read::ZipFile::enclosed_name`] so that
/// archive entries containing `..` or absolute paths cannot escape `dest_dir`.
///
/// # Errors
///
/// Returns an error if the file is not a valid zip archive, if IO fails, or if
/// CRC32 verification fails for any extracted entry
/// ([`Error::ChecksumMismatch`]).
pub fn extract_zip(zip_path: &Path, dest_dir: &Path) -> Result<()> {
  let file = fs::File::open(zip_path)?;
  let mut archive = zip::ZipArchive::new(file)?;

  fs::create_dir_all(dest_dir)?;

  for i in 0..archive.len() {
    let mut file_in_archive = archive.by_index(i)?;
    let outpath = match file_in_archive.enclosed_name() {
      Some(path) => dest_dir.join(path),
      None => {
        warn!("Invalid file path in zip: {}", file_in_archive.name());
        continue;
      }
    };

    let comment = file_in_archive.comment();

    if !comment.is_empty() {
      info!("File {} comment: {}", i, comment);
    }

    if file_in_archive.name().ends_with('/') {
      info!("Creating directory {}", outpath.display());
      fs::create_dir_all(&outpath)?;
    } else {
      info!("Extracting file {} to {}", i, outpath.display());

      if let Some(parent) = outpath.parent()
        && !parent.exists()
      {
        fs::create_dir_all(parent)?;
      }

      let expected_crc32 = file_in_archive.crc32();
      let mut outfile = fs::File::create(&outpath)?;

      let mut buffer = Vec::new();
      file_in_archive.read_to_end(&mut buffer)?;

      let actual_crc32 = calculate_crc32(&buffer);

      if actual_crc32 != expected_crc32 {
        error!(
          "CRC32 verification failed for {}: expected {:08x}, got {:08x}",
          outpath.display(),
          expected_crc32,
          actual_crc32
        );

        return Err(Error::ChecksumMismatch {
          path: outpath.display().to_string(),
          expected: expected_crc32,
          actual: actual_crc32,
        });
      }

      outfile.write_all(&buffer)?;

      debug!("CRC32 verification successful for {}", outpath.display());
    }
  }

  Ok(())
}

/// Calculates the CRC32 checksum for a byte buffer.
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

  if !manifest_path.exists() {
    debug!(
      "No manifest in {}, extracting version {}",
      mod_dir.display(),
      version
    );

    return Ok(true);
  }

  match Manifest::from_file(&manifest_path) {
    Ok(manifest) => {
      if manifest.version_number == version {
        debug!(
          "{} already has version {}, skipping",
          mod_dir.display(),
          version
        );

        Ok(false)
      } else {
        debug!(
          "{} version changed from {} to {}, extracting",
          mod_dir.display(),
          manifest.version_number,
          version
        );

        Ok(true)
      }
    }
    Err(err) => {
      warn!("Failed to parse manifest in {}: {}", mod_dir.display(), err);

      Ok(true)
    }
  }
}

/// Parses a Thunderstore package zip filename of the form
/// `Owner-Name-Version.zip`.
///
/// Returns the package full name (`Owner-Name`) and the version string, or
/// `None` if the filename does not contain at least three `-`-separated parts.
pub fn parse_package_filename(file_name: &str) -> Option<(String, String)> {
  let parts: Vec<&str> = file_name.split('-').collect();

  if parts.len() < 3 {
    return None;
  }

  let full_name = format!("{}-{}", parts[0], parts[1]);
  let version = parts[2].trim_end_matches(".zip").to_string();

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
  fn test_parse_package_filename() {
    assert_eq!(
      parse_package_filename("Owner-ModName-1.0.0.zip"),
      Some(("Owner-ModName".to_string(), "1.0.0".to_string()))
    );

    assert_eq!(parse_package_filename("InvalidFile.zip"), None);
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
