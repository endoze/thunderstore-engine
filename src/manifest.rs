#![deny(missing_docs)]

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// Maximum size of a `manifest.json` read from disk. A `manifest.json` comes from
/// an untrusted downloaded archive, so this bounds the memory a hostile or corrupt
/// file can force `from_file` to allocate.
const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;

/// Manifest metadata for an individual mod package.
///
/// This structure represents the manifest.json file found in mod archives,
/// containing version information, dependencies, and package metadata.
#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
  /// The version number of this mod (e.g., "1.2.3").
  pub version_number: String,
  /// The display name of the mod.
  #[serde(default)]
  pub name: String,
  /// A description of what the mod does.
  #[serde(default)]
  pub description: String,
  /// Optional URL to the mod's website or documentation.
  #[serde(default)]
  pub website_url: Option<String>,
  /// List of dependencies in "Owner-ModName" format.
  #[serde(default)]
  pub dependencies: Vec<String>,
}

impl Manifest {
  /// Loads and parses a `manifest.json` from disk.
  ///
  /// A leading BOM and stray C0 control characters are stripped before parsing,
  /// because a `manifest.json` comes from an arbitrary uploaded archive and those
  /// bytes are common enough in the wild that rejecting them would fail installs
  /// r2modman completes. DEL, the C1 controls, zero-width joiners, and
  /// directional marks are all preserved, since JSON permits them and stripping
  /// them would corrupt legitimate non-Latin text.
  ///
  /// # Errors
  ///
  /// - [`Error::SizeLimitExceeded`] if the file is larger than 8 MiB, checked
  ///   against the file's metadata before any of it is read into memory, so a
  ///   hostile archive cannot force a large allocation.
  /// - [`Error::ManifestParse`] if the file cannot be read, is not UTF-8, or is
  ///   not valid JSON with a `version_number`. Every other field defaults, so a
  ///   sparse manifest parses fine.
  pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
    Self::from_file_with_limit(path, MAX_MANIFEST_BYTES)
  }

  /// Loads and parses a manifest.json, rejecting a file larger than `max_bytes`.
  fn from_file_with_limit<P: AsRef<Path>>(path: P, max_bytes: u64) -> Result<Self> {
    let path = path.as_ref();

    let metadata = fs::metadata(path)
      .map_err(|e| Error::ManifestParse(format!("Failed to read manifest file: {}", e)))?;

    if metadata.len() > max_bytes {
      return Err(Error::SizeLimitExceeded {
        context: "manifest.json".to_string(),
        limit: max_bytes,
        actual: Some(metadata.len()),
      });
    }

    let manifest_content = fs::read_to_string(path)
      .map_err(|e| Error::ManifestParse(format!("Failed to read manifest file: {}", e)))?;

    let cleaned_content = Self::clean_manifest_content(&manifest_content);

    serde_json::from_str(&cleaned_content)
      .map_err(|e| Error::ManifestParse(format!("Failed to parse manifest JSON: {}", e)))
  }

  /// Sanitizes raw manifest content just enough for a strict JSON parser, while
  /// preserving all legitimate text.
  ///
  /// Two things are removed: a leading UTF-8 BOM (`U+FEFF`), which is not valid
  /// at the start of a JSON document, and stray C0 control characters
  /// (`U+0000`–`U+001F`), which RFC 8259 requires be escaped rather than appear
  /// literally inside JSON strings — except the whitespace JSON permits (tab,
  /// newline, carriage return). Everything else is left intact, including DEL
  /// (`U+007F`) and the C1 controls (`U+0080`–`U+009F`), which JSON permits
  /// unescaped, as well as zero-width joiners, directional marks, and
  /// non-breaking spaces — so real mod names and descriptions are never mangled.
  ///
  /// # Parameters
  ///
  /// * `content` - The raw manifest content string
  ///
  /// # Returns
  ///
  /// A cleaned string safe to hand to a strict JSON parser.
  fn clean_manifest_content(content: &str) -> String {
    let content = content.strip_prefix('\u{FEFF}').unwrap_or(content);

    content
      .chars()
      .filter(|&c| !('\u{0000}'..='\u{001F}').contains(&c) || c == '\n' || c == '\r' || c == '\t')
      .collect()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs::File;
  use std::io::Write;
  use tempfile::tempdir;

  #[test]
  fn test_clean_manifest_content_leaves_plain_json_untouched() {
    let content = r#"{
      "version_number": "1.0.0",
      "name": "Test Mod",
      "description": "This is a test mod with unicode: 你好, ¿Cómo estás?"
    }"#;

    let cleaned = Manifest::clean_manifest_content(content);

    assert_eq!(cleaned, content);
  }

  #[test]
  fn test_clean_manifest_content_strips_only_leading_bom() {
    let with_leading_bom = format!("{}{{\"version_number\": \"1.0.0\"}}", '\u{FEFF}');

    let cleaned = Manifest::clean_manifest_content(&with_leading_bom);

    assert!(cleaned.starts_with('{'));
    assert!(!cleaned.contains('\u{FEFF}'));
  }

  #[test]
  fn test_clean_manifest_content_strips_c0_but_keeps_del_and_c1() {
    // DEL (U+007F) and the C1 controls (U+0080-U+009F, e.g. NEL U+0085) are legal
    // unescaped inside JSON strings, so they must survive. Only C0 controls
    // (U+0000-U+001F), other than the tab/newline/CR whitespace JSON permits, are
    // stripped.
    let input = "a\u{007F}b\u{0085}c\u{0001}d\te";

    let cleaned = Manifest::clean_manifest_content(input);

    assert_eq!(cleaned, "a\u{007F}b\u{0085}cd\te");
  }

  #[test]
  fn test_clean_manifest_content_preserves_valid_unicode_format_chars() {
    // Zero-width joiner (emoji sequences), directional marks, non-breaking
    // space, and a non-leading BOM are all valid inside JSON string values and
    // must survive cleaning rather than being silently mangled.
    let name = "👨\u{200D}👩\u{200D}👧 \u{200E}\u{200F} \u{00A0} \u{FEFF}";
    let content = format!("{{\"name\": \"{name}\"}}");

    let cleaned = Manifest::clean_manifest_content(&content);

    assert!(cleaned.contains('\u{200D}'), "zero-width joiner preserved");
    assert!(cleaned.contains('\u{200E}'), "LTR mark preserved");
    assert!(cleaned.contains('\u{200F}'), "RTL mark preserved");
    assert!(cleaned.contains('\u{00A0}'), "non-breaking space preserved");
    assert!(cleaned.contains('\u{FEFF}'), "non-leading BOM preserved");
  }

  #[test]
  fn test_clean_manifest_content_strips_stray_control_chars() {
    // A stray C0 control char is invalid unescaped in JSON and is removed, while
    // the whitespace JSON permits (tab/newline/carriage return) is kept.
    let content = "{\u{0007}\n\t\"version_number\": \"1.0.0\"\r\n}";

    let cleaned = Manifest::clean_manifest_content(content);

    assert!(!cleaned.contains('\u{0007}'));
    assert!(cleaned.contains('\n'));
    assert!(cleaned.contains('\t'));
    assert!(cleaned.contains('\r'));
  }

  #[test]
  fn test_from_file_rejects_oversized_manifest() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("big.json");

    let mut file = File::create(&file_path).unwrap();
    file.write_all(&[b'x'; 64]).unwrap();

    let result = Manifest::from_file_with_limit(&file_path, 16);

    assert!(result.is_err());
  }

  #[test]
  fn test_from_file() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("manifest.json");

    let manifest_content = r#"{
      "version_number": "1.2.3",
      "name": "Test Mod",
      "description": "Test mod description",
      "website_url": "https://example.com",
      "dependencies": ["Owner1-ModA", "Owner2-ModB"]
    }"#;

    let mut file = File::create(&file_path).unwrap();
    file.write_all(manifest_content.as_bytes()).unwrap();

    let manifest = Manifest::from_file(&file_path).unwrap();

    assert_eq!(manifest.version_number, "1.2.3");
    assert_eq!(manifest.name, "Test Mod");
    assert_eq!(manifest.description, "Test mod description");
    assert_eq!(
      manifest.website_url,
      Some("https://example.com".to_string())
    );
    assert_eq!(manifest.dependencies, vec!["Owner1-ModA", "Owner2-ModB"]);

    let malformed_path = dir.path().join("malformed.json");
    let malformed_content = r#"{
      "version_number": "1.0.0",
      "name": "Malformed JSON,
      "description": "Missing quotes"
    }"#;

    let mut file = File::create(&malformed_path).unwrap();
    file.write_all(malformed_content.as_bytes()).unwrap();

    let result = Manifest::from_file(&malformed_path);
    assert!(result.is_err());
  }
}
