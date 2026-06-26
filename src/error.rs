use std::io;
use std::num::ParseIntError;
use thiserror::Error;

/// A type alias for `Result<T, Error>` used throughout the engine.
pub type Result<T> = std::result::Result<T, Error>;

/// Represents all possible errors that can occur within the Thunderstore engine.
#[derive(Error, Debug)]
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

  /// Error from date/time parsing.
  #[error("DateTime parse error: {0}")]
  DateTime(#[from] chrono::ParseError),

  /// Error from header parsing.
  #[error("Header error: {0}")]
  Header(#[from] reqwest::header::ToStrError),

  /// Error from parsing integers.
  #[error("Parse integer error: {0}")]
  ParseInt(#[from] ParseIntError),

  /// Error when an async task fails to join.
  #[error("Task failed: {0}")]
  TaskFailed(#[from] tokio::task::JoinError),

  /// Missing header in response.
  #[error("Missing header: {0}")]
  MissingHeader(String),

  /// Error from manifest parsing or cache (de)serialization.
  #[error("Manifest error: {0}")]
  Manifest(String),

  /// Error from ecosystem-schema parsing.
  #[error("Ecosystem error: {0}")]
  Ecosystem(String),

  /// CRC32 verification failed while extracting an archive entry.
  #[error("CRC32 verification failed for {path}: expected {expected:08x}, got {actual:08x}")]
  ChecksumMismatch {
    path: String,
    expected: u32,
    actual: u32,
  },
}
