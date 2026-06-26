#![deny(missing_docs)]

use std::io;
use std::num::ParseIntError;
use thiserror::Error;

/// A type alias for `Result<T, Error>` used throughout the engine.
pub type Result<T> = std::result::Result<T, Error>;

/// Represents all possible errors that can occur within the Thunderstore engine.
///
/// This enum is `#[non_exhaustive]`: new variants may be added in future
/// releases without a breaking change, so downstream `match`es must include a
/// wildcard arm.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
  /// Error from the IO system.
  #[error("IO error: {0}")]
  Io(#[from] io::Error),

  /// Error from HTTP operations.
  #[error("HTTP error: {0}")]
  Http(#[from] reqwest::Error),

  /// Error from JSON parsing.
  #[error("JSON error: {0}")]
  Json(#[from] serde_json::Error),

  /// Error from ZIP operations.
  #[error("ZIP error: {0}")]
  Zip(#[from] zip::result::ZipError),

  /// Error from header parsing.
  #[error("Header error: {0}")]
  Header(#[from] reqwest::header::ToStrError),

  /// Error from parsing integers.
  #[error("Parse integer error: {0}")]
  ParseInt(#[from] ParseIntError),

  /// Missing header in response.
  #[error("Missing header: {0}")]
  MissingHeader(String),

  /// Error from builder or configuration validation.
  #[error("Configuration error: {0}")]
  Config(String),

  /// Error parsing a per-mod `manifest.json`.
  #[error("manifest parse error: {0}")]
  ManifestParse(String),

  /// Error reading, writing, or validating the on-disk package-index cache.
  ///
  /// Covers cache load/decode/encode, a missing cache, an incompatible or
  /// corrupt cache layout, and the integrity checks in
  /// [`PackageIndex::validate`](crate::models::PackageIndex) — which run at the
  /// cache boundary.
  #[error("cache error: {0}")]
  Cache(String),

  /// A network body, manifest file, or decompressed payload exceeded a
  /// configured size limit.
  ///
  /// `actual` is a structured field for programmatic inspection; it is
  /// deliberately not rendered in `Display` because `thiserror` cannot
  /// conditionally format an `Option`.
  #[error("{context} exceeds size limit of {limit} bytes")]
  SizeLimitExceeded {
    /// A short label for what was being read (e.g. `"download body"`).
    context: String,
    /// The configured byte ceiling.
    limit: u64,
    /// The observed size, when known.
    actual: Option<u64>,
  },

  /// A download ended before the declared number of bytes arrived.
  #[error("incomplete download: expected {expected} bytes, received {actual}")]
  IncompleteDownload {
    /// The number of bytes the server declared.
    expected: u64,
    /// The number of bytes actually received.
    actual: u64,
  },

  /// A download filename was unsafe (empty, absolute, or path-traversing).
  #[error("unsafe name: {0}")]
  UnsafeName(String),

  /// The on-disk cache was written by an incompatible version of the cache
  /// format. The client treats this as a cache miss and re-fetches.
  #[error("unsupported cache format version {found} (expected {expected})")]
  UnsupportedCacheVersion {
    /// The format version read from the cache file.
    found: u32,
    /// The format version this build understands.
    expected: u32,
  },

  /// Error from ecosystem-schema parsing.
  #[error("Ecosystem error: {0}")]
  Ecosystem(String),

  /// Error from install routing, e.g. a package that would escape the install root.
  #[error("Install error: {0}")]
  Install(String),

  /// Error from the profile layer: an invalid profile name, a malformed
  /// `mods.yml`/state file, or a profile operation that could not complete.
  #[error("Profile error: {0}")]
  Profile(String),

  /// A profile name that could not be used, from any operation that gates on
  /// [`layout::require_profile`](crate::profile::layout::require_profile).
  ///
  /// Carries the reason rather than a sentence so an operation with other
  /// failure modes, such as
  /// [`layout::use_profile`](crate::profile::layout::use_profile), can still
  /// return one `Error` without a caller losing the ability to word the refusal
  /// itself. Renders exactly as [`Error::Profile`] did for the same refusal.
  #[error("Profile error: {0}")]
  BadProfile(#[from] crate::profile::layout::BadProfile),

  /// One or more files in a batch download failed.
  #[error("{failed} of {total} downloads failed; first error: {first}")]
  Download {
    /// How many files failed.
    failed: usize,
    /// How many files were attempted.
    total: usize,
    /// The first failure's message.
    first: String,
  },

  /// A response header was present but could not be parsed, e.g. an
  /// unparseable `Last-Modified` value.
  #[error("invalid header: {0}")]
  InvalidHeader(String),

  /// An archive exceeded a configured extraction limit (entry count or
  /// uncompressed size), e.g. a zip bomb.
  #[error("Archive extraction limit exceeded: {0}")]
  ExtractLimitExceeded(String),

  /// CRC32 verification failed while extracting an archive entry.
  #[error("CRC32 verification failed for {path}: expected {expected:08x}, got {actual:08x}")]
  ChecksumMismatch {
    /// The archive-relative path of the entry that failed verification.
    path: String,
    /// The CRC32 the archive recorded for the entry.
    expected: u32,
    /// The CRC32 computed over the bytes actually read.
    actual: u32,
  },
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn profile_error_displays_message() {
    let err = Error::Profile("bad profile".to_string());

    assert_eq!(err.to_string(), "Profile error: bad profile");
  }
}
