//! Small internal utilities shared across the engine.

#![deny(missing_docs)]

use crate::error::{Error, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// Enforces a byte ceiling in two phases: an up-front check of a declared
/// length (e.g. a `Content-Length` header) and a running accumulator over
/// streamed chunks.
///
/// This centralizes the size-cap logic that would otherwise be hand-rolled at
/// every download/decompress site, and — being pure — is unit-testable without
/// any network or filesystem I/O. Both rejection paths return
/// [`Error::SizeLimitExceeded`] carrying the `context` label and the observed
/// size.
pub(crate) struct SizeCap {
  context: &'static str,
  limit: u64,
  seen: u64,
}

impl SizeCap {
  /// Creates a cap of `limit` bytes. `context` is a short label naming what is
  /// being read (e.g. `"download body"`), surfaced in the error message.
  pub(crate) fn new(context: &'static str, limit: u64) -> Self {
    Self {
      context,
      limit,
      seen: 0,
    }
  }

  /// Rejects a declared length that already exceeds the limit, before any bytes
  /// are streamed. `None` (no declared length, e.g. a chunked body) always
  /// passes — the streamed total is still enforced by [`add`](Self::add).
  pub(crate) fn check_declared(&self, declared: Option<u64>) -> Result<()> {
    if let Some(n) = declared
      && n > self.limit
    {
      return Err(self.exceeded(Some(n)));
    }

    Ok(())
  }

  /// Accumulates `n` streamed bytes, erroring once the running total exceeds
  /// the limit.
  pub(crate) fn add(&mut self, n: u64) -> Result<()> {
    self.seen = self.seen.saturating_add(n);

    if self.seen > self.limit {
      return Err(self.exceeded(Some(self.seen)));
    }

    Ok(())
  }

  fn exceeded(&self, actual: Option<u64>) -> Error {
    Error::SizeLimitExceeded {
      context: self.context.to_string(),
      limit: self.limit,
      actual,
    }
  }
}

/// Wraps an [`std::io::Error`] with a context prefix — typically the operation
/// and the path it acted on — while preserving the original error kind.
///
/// Used to turn a bare `?`-propagated I/O error (which says only "No such file
/// or directory") into one that names what failed and where.
pub(crate) fn io_context(context: impl std::fmt::Display, err: std::io::Error) -> Error {
  Error::Io(std::io::Error::new(err.kind(), format!("{context}: {err}")))
}

/// The `Owner-Name` prefix (the first two hyphen-separated segments) of a
/// Thunderstore dependency string or package filename.
///
/// Thunderstore identifiers follow the `Owner-Name[-Version]` convention.
/// Dependency strings and archive filenames both carry a trailing version this
/// prefix strips. The input is borrowed; nothing is allocated. Strings with
/// fewer than two segments (no hyphen) are returned unchanged.
pub(crate) fn full_name_prefix(s: &str) -> &str {
  match s.match_indices('-').nth(1) {
    Some((idx, _)) => &s[..idx],
    None => s,
  }
}

/// Whether `source` and `dest` name the same file on disk, following symlinks
/// and resolving `.`/`..` so a differently spelled path to the identical file
/// is recognized, not merely a similarly spelled one.
///
/// Guards a caller against `fs::copy(path, path)`, which opens the destination
/// with `O_TRUNC` before reading the source, so copying a file onto itself
/// truncates it to zero bytes. That happens whenever a copy's source and
/// destination resolve to the identical file, for example when a profile
/// import's target equals its source, or a launch stage's destination is
/// simply `game_dir.join(file_name)` and the staging target is `game_dir`
/// itself.
///
/// A destination that does not exist yet cannot be canonicalized directly, so
/// its parent is canonicalized and the file name rejoined; any failure there
/// (a parent that also doesn't exist, a permissions error, and so on) is left
/// as an `Err` and falls through to the plain path comparison below, since
/// this exists only to avoid a destructive self-copy, never to validate that a
/// path exists.
pub(crate) fn same_file(source: &Path, dest: &Path) -> bool {
  let canonical_dest = fs::canonicalize(dest).or_else(|_| canonicalize_missing(dest));

  match (fs::canonicalize(source), canonical_dest) {
    (Ok(canonical_source), Ok(canonical_dest)) => canonical_source == canonical_dest,
    _ => source == dest,
  }
}

/// Canonicalizes a path that may not exist yet by canonicalizing its parent
/// directory and rejoining the file name, since [`fs::canonicalize`] itself
/// errors unless every component already exists.
///
/// Any failure here (a parent that also doesn't exist, a permissions error,
/// and so on) is left as an `Err` for [`same_file`] to fall back on a plain
/// path comparison: this exists only to avoid a destructive self-copy, never
/// to validate that a path exists.
pub(crate) fn canonicalize_missing(path: &Path) -> std::io::Result<PathBuf> {
  let parent = path
    .parent()
    .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
  let file_name = path
    .file_name()
    .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;

  fs::canonicalize(parent).map(|canonical_parent| canonical_parent.join(file_name))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn check_declared_accepts_none_and_within_limit() {
    let cap = SizeCap::new("body", 100);

    assert!(cap.check_declared(None).is_ok());
    assert!(cap.check_declared(Some(100)).is_ok());
  }

  #[test]
  fn check_declared_rejects_over_limit() {
    let cap = SizeCap::new("body", 100);

    let err = cap.check_declared(Some(101)).unwrap_err();

    assert!(matches!(
      err,
      Error::SizeLimitExceeded {
        limit: 100,
        actual: Some(101),
        ..
      }
    ));
  }

  #[test]
  fn add_accumulates_and_rejects_when_total_exceeds() {
    let mut cap = SizeCap::new("body", 10);

    assert!(cap.add(6).is_ok());

    // The cumulative total (6 + 5 = 11) exceeds the limit; `actual: Some(11)`
    // confirms the accumulator summed both chunks rather than checking each alone.
    let err = cap.add(5).unwrap_err();

    assert!(matches!(
      err,
      Error::SizeLimitExceeded {
        limit: 10,
        actual: Some(11),
        ..
      }
    ));
  }

  #[test]
  fn add_accepts_exactly_at_limit() {
    let mut cap = SizeCap::new("body", 10);

    assert!(cap.add(10).is_ok());
  }

  #[test]
  fn io_context_prefixes_message_and_preserves_kind() {
    let base = std::io::Error::new(std::io::ErrorKind::NotFound, "nope");

    let wrapped = io_context("reading /x", base);

    match wrapped {
      Error::Io(e) => {
        assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
        assert!(e.to_string().contains("reading /x"));
        assert!(e.to_string().contains("nope"));
      }
      other => panic!("unexpected error: {other}"),
    }
  }

  #[test]
  fn full_name_prefix_variants() {
    assert_eq!(full_name_prefix("Owner-Name"), "Owner-Name");
    assert_eq!(full_name_prefix("Owner-Name-1.2.3"), "Owner-Name");
    assert_eq!(
      full_name_prefix("Owner-Multi-Word-Name-1.0.0"),
      "Owner-Multi"
    );
    assert_eq!(full_name_prefix("NoHyphen"), "NoHyphen");
    assert_eq!(full_name_prefix(""), "");
  }
}
