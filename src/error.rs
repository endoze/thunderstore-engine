#![deny(missing_docs)]

use std::fmt;
use std::io;
use std::num::ParseIntError;
use std::sync::Arc;
use thiserror::Error;

/// A type alias for `Result<T, Error>` used throughout the engine.
pub type Result<T> = std::result::Result<T, Error>;

/// Represents all possible errors that can occur within the Thunderstore engine.
///
/// This enum is `#[non_exhaustive]`: new variants may be added in future
/// releases without a breaking change, so downstream `match`es must include a
/// wildcard arm.
///
/// `Error` is `Clone`, so a client can keep one in its state or hand it to
/// several listeners. Sources that are not `Clone` themselves are held in a
/// [`Shared`]; `?` still converts them directly, and
/// [`source`](std::error::Error::source) still downcasts to them.
#[derive(Error, Debug, Clone)]
#[non_exhaustive]
pub enum Error {
  /// Error from the IO system.
  #[error("IO error: {0}")]
  Io(#[source] Shared<io::Error>),

  /// Error from HTTP operations.
  #[cfg(feature = "io")]
  #[error("HTTP error: {0}")]
  Http(#[source] Shared<reqwest::Error>),

  /// Error from JSON parsing.
  #[error("JSON error: {0}")]
  Json(#[source] Shared<serde_json::Error>),

  /// Error from ZIP operations.
  #[cfg(feature = "io")]
  #[error("ZIP error: {0}")]
  Zip(#[source] Shared<zip::result::ZipError>),

  /// Error from header parsing.
  #[cfg(feature = "io")]
  #[error("Header error: {0}")]
  Header(#[source] Shared<reqwest::header::ToStrError>),

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

  /// The package is not recorded in the target's `mods.yml`.
  #[error("Profile error: {package} is not installed")]
  NotInstalled {
    /// The `Owner-Name` identifier as given.
    package: String,
  },

  /// A requested package, or a package in its dependency closure, is not in
  /// the package index, so nothing was installed.
  #[error("Profile error: cannot install {package}: could not resolve {}", .unresolved.join(", "))]
  UnresolvedDependency {
    /// The identifier whose install was requested.
    package: String,
    /// Every identifier that could not be resolved, sorted.
    unresolved: Vec<String>,
  },

  /// The index has the package, but not the version the request names.
  #[error("Profile error: cannot install {package}-{version}: {package} has no version {version}")]
  MissingVersion {
    /// The `Owner-Name` identifier.
    package: String,
    /// The requested version.
    version: String,
  },

  /// The ecosystem has no game with this slug.
  #[error("Profile error: unknown game {game:?}")]
  UnknownGame {
    /// The game slug as given.
    game: String,
  },

  /// The game is known, but the ecosystem gives it no profile, so there are no
  /// install rules to route its packages with.
  #[error("Profile error: no ecosystem profile for game {game:?}")]
  NoGameProfile {
    /// The game slug as given.
    game: String,
  },

  /// One or more files in a batch download failed.
  #[error("{} of {total} downloads failed: {}", .failures.len(), list_failures(.failures))]
  Download {
    /// How many files were attempted.
    total: usize,
    /// Each file that failed, with the reason, sorted by file name.
    failures: Vec<(String, Error)>,
  },

  /// An import stopped because one or more of its mods failed to install, so
  /// its config files were not restored. Importing again once the causes are
  /// fixed skips the mods already installed and completes it.
  #[error(
    "Profile error: {}; the import is unfinished (its config files were not restored)",
    list_import_failures(.failures)
  )]
  ImportUnfinished {
    /// Each exported identifier that failed, with the reason.
    failures: Vec<(String, Error)>,
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

/// Renders `name: reason` pairs as one line, separated by `; `.
fn list_failures(failures: &[(String, Error)]) -> String {
  failures
    .iter()
    .map(|(name, error)| format!("{name}: {error}"))
    .collect::<Vec<_>>()
    .join("; ")
}

/// An error that is not `Clone`, shared behind an [`Arc`] so [`Error`] can be.
///
/// It derefs to the error it holds, and that error, not the `Shared`, is what
/// [`Error`]'s [`source`](std::error::Error::source) returns, so a caller can
/// downcast it:
///
/// ```
/// use std::error::Error as _;
/// use thunderstore_engine::Error;
///
/// let err = Error::from(std::io::Error::other("gone"));
///
/// assert!(err.source().unwrap().downcast_ref::<std::io::Error>().is_some());
/// ```
#[derive(Debug)]
pub struct Shared<T>(Arc<T>);

impl<T> Clone for Shared<T> {
  fn clone(&self) -> Self {
    Self(Arc::clone(&self.0))
  }
}

impl<T> std::ops::Deref for Shared<T> {
  type Target = T;

  fn deref(&self) -> &T {
    &self.0
  }
}

impl<T: fmt::Display> fmt::Display for Shared<T> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    self.0.fmt(f)
  }
}

/// Renders import failures as `importing name: reason`, separated by `; `.
fn list_import_failures(failures: &[(String, Error)]) -> String {
  failures
    .iter()
    .map(|(name, error)| format!("importing {name}: {error}"))
    .collect::<Vec<_>>()
    .join("; ")
}

impl From<io::Error> for Error {
  fn from(error: io::Error) -> Self {
    Self::Io(Shared(Arc::new(error)))
  }
}

#[cfg(feature = "io")]
impl From<reqwest::Error> for Error {
  fn from(error: reqwest::Error) -> Self {
    Self::Http(Shared(Arc::new(error)))
  }
}

impl From<serde_json::Error> for Error {
  fn from(error: serde_json::Error) -> Self {
    Self::Json(Shared(Arc::new(error)))
  }
}

#[cfg(feature = "io")]
impl From<zip::result::ZipError> for Error {
  fn from(error: zip::result::ZipError) -> Self {
    Self::Zip(Shared(Arc::new(error)))
  }
}

#[cfg(feature = "io")]
impl From<reqwest::header::ToStrError> for Error {
  fn from(error: reqwest::header::ToStrError) -> Self {
    Self::Header(Shared(Arc::new(error)))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn profile_error_displays_message() {
    let err = Error::Profile("bad profile".to_string());

    assert_eq!(err.to_string(), "Profile error: bad profile");
  }

  #[test]
  fn a_cloned_error_keeps_its_message_and_source() {
    let err = Error::from(io::Error::new(io::ErrorKind::NotFound, "gone"));

    let kept = err.clone();

    assert_eq!(kept.to_string(), "IO error: gone");
    assert_eq!(
      std::error::Error::source(&kept).map(|source| source.to_string()),
      Some("gone".to_string())
    );
  }

  #[test]
  fn a_wrapped_source_downcasts_to_the_original_error() {
    let io = Error::from(io::Error::new(io::ErrorKind::NotFound, "gone"));
    let json = Error::from(serde_json::from_str::<u8>("x").unwrap_err());

    let io_source =
      std::error::Error::source(&io).and_then(|source| source.downcast_ref::<io::Error>());
    let json_source = std::error::Error::source(&json)
      .and_then(|source| source.downcast_ref::<serde_json::Error>());

    assert_eq!(
      io_source.map(io::Error::kind),
      Some(io::ErrorKind::NotFound)
    );
    assert!(json_source.is_some());
  }
}
