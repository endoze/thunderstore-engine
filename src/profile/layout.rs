//! Directory layout and profile lifecycle.
//!
//! Profiles live at `<base>/<game>/profiles/<name>/`, matching r2modman so a
//! profile directory produced here works when copied into r2modman's data dir.
//! The caller owns `base` (platform/path discovery stays outside the engine).
//!
//! # Errors
//!
//! Every fallible function in this module returns
//! [`Error::Profile`](crate::Error::Profile) and nothing else. Underlying IO
//! failures are wrapped with the offending path rather than surfaced as
//! [`Error::Io`](crate::Error::Io), so a caller can render the message directly
//! and does not need to match other variants or prepend a path itself.
//!
//! Every function taking a profile name calls
//! [`validate_name`](crate::profile::layout::validate_name) before touching the
//! filesystem, so a name containing a separator, NUL, `.`, or `..` is rejected
//! before it can escape the profiles directory. Individual functions document
//! only the conditions beyond that shared contract.

#![deny(missing_docs)]

use crate::error::{Error, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// The profiles directory for a game: `<base>/<game>/profiles`.
pub fn profiles_dir(base: &Path, game: &str) -> PathBuf {
  base.join(game).join("profiles")
}

/// A single profile directory: `<base>/<game>/profiles/<name>`.
pub fn profile_dir(base: &Path, game: &str, name: &str) -> PathBuf {
  profiles_dir(base, game).join(name)
}

/// Rejects a profile name that is empty, a path component (`.`/`..`), or that
/// contains a path separator or NUL — so a name can never escape the profiles
/// directory.
pub fn validate_name(name: &str) -> Result<()> {
  let bad = name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']);

  if bad {
    return Err(Error::Profile(format!("invalid profile name {name:?}")));
  }

  Ok(())
}

/// Lists profile names (immediate subdirectories) for a game, sorted. A missing
/// profiles directory yields an empty list.
pub fn list(base: &Path, game: &str) -> Result<Vec<String>> {
  let dir = profiles_dir(base, game);

  if !dir.is_dir() {
    return Ok(Vec::new());
  }

  let mut names = Vec::new();

  for entry in
    fs::read_dir(&dir).map_err(|e| Error::Profile(format!("reading {}: {}", dir.display(), e)))?
  {
    let entry = entry.map_err(|e| Error::Profile(format!("reading an entry: {e}")))?;

    if entry.path().is_dir()
      && let Some(name) = entry.file_name().to_str()
    {
      names.push(name.to_string());
    }
  }

  names.sort();

  Ok(names)
}

/// Creates a new, empty profile directory. Errors if it already exists so a
/// create never silently adopts an existing profile.
pub fn create(base: &Path, game: &str, name: &str) -> Result<PathBuf> {
  validate_name(name)?;

  let dir = profile_dir(base, game, name);

  if dir.exists() {
    return Err(Error::Profile(format!("profile {name:?} already exists")));
  }

  fs::create_dir_all(&dir)
    .map_err(|e| Error::Profile(format!("creating {}: {}", dir.display(), e)))?;

  Ok(dir)
}

/// Removes a profile directory and everything under it, and clears the stored
/// selection if it named this profile.
///
/// # Errors
///
/// Errors if the profile does not exist, so deleting an already-deleted profile
/// is reported rather than treated as success.
pub fn delete(base: &Path, game: &str, name: &str) -> Result<()> {
  validate_name(name)?;

  let dir = profile_dir(base, game, name);

  fs::remove_dir_all(&dir)
    .map_err(|e| Error::Profile(format!("deleting {}: {}", dir.display(), e)))?;

  if read_selection(base, game).as_deref() == Some(name) {
    clear_selection(base, game)?;
  }

  Ok(())
}

/// Renames a profile, carrying the stored selection across if it named `from`.
///
/// # Errors
///
/// Errors if `to` already exists, so a rename never clobbers another profile.
///
/// Not transactional: the rename can succeed and the selection update then fail,
/// leaving the selection naming a profile that no longer exists. Callers that
/// care can re-read it with [`read_selection`].
pub fn rename(base: &Path, game: &str, from: &str, to: &str) -> Result<()> {
  validate_name(from)?;
  validate_name(to)?;

  let src = profile_dir(base, game, from);
  let dst = profile_dir(base, game, to);

  if dst.exists() {
    return Err(Error::Profile(format!("profile {to:?} already exists")));
  }

  fs::rename(&src, &dst).map_err(|e| {
    Error::Profile(format!(
      "renaming {} to {}: {}",
      src.display(),
      dst.display(),
      e
    ))
  })?;

  // Carry the selection across, or it is left naming a directory that no longer
  // exists. Not transactional: a failed write here leaves that dangling state.
  // The window is narrow and accepted rather than missed.
  if read_selection(base, game).as_deref() == Some(from) {
    write_selection(base, game, to)?;
  }

  Ok(())
}

/// Duplicates a profile directory (recursive copy) under a new name.
///
/// Symlinks are skipped rather than followed or recreated, so the copy cannot be
/// made to write outside `to`.
///
/// # Errors
///
/// Errors if `to` already exists. A failed copy can leave `to` partially
/// written; it is not cleaned up.
pub fn duplicate(base: &Path, game: &str, from: &str, to: &str) -> Result<PathBuf> {
  validate_name(from)?;
  validate_name(to)?;

  let src = profile_dir(base, game, from);
  let dst = profile_dir(base, game, to);

  if dst.exists() {
    return Err(Error::Profile(format!("profile {to:?} already exists")));
  }

  copy_tree(&src, &dst)?;

  Ok(dst)
}

/// Recursively copies a directory tree, creating `dst`. Skips symlinks (never
/// follows them out of the tree), like the install layer's `walk_files`.
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
  fs::create_dir_all(dst)
    .map_err(|e| Error::Profile(format!("creating {}: {}", dst.display(), e)))?;

  for entry in
    fs::read_dir(src).map_err(|e| Error::Profile(format!("reading {}: {}", src.display(), e)))?
  {
    let entry = entry.map_err(|e| Error::Profile(format!("reading an entry: {e}")))?;
    let metadata = fs::symlink_metadata(entry.path())
      .map_err(|e| Error::Profile(format!("inspecting {}: {}", entry.path().display(), e)))?;

    if metadata.file_type().is_symlink() {
      continue;
    }

    let target = dst.join(entry.file_name());

    if metadata.is_dir() {
      copy_tree(&entry.path(), &target)?;
    } else {
      // `duplicate`'s src and dst are both `profile_dir(base, game, <name>)`
      // siblings under one parent, and `dst.exists()` refuses the equal case
      // before this ever runs, so an overlapping source and target cannot
      // reach here today. The skip is added anyway: a future caller of
      // `copy_tree` reaching it another way could otherwise truncate a file by
      // copying it onto itself, the same failure mode `same_file` guards
      // against in the portability layer's `copy_tree` and in
      // `stage_one_file`.
      if crate::util::same_file(&entry.path(), &target) {
        continue;
      }

      fs::copy(entry.path(), &target)
        .map_err(|e| Error::Profile(format!("copying to {}: {}", target.display(), e)))?;
    }
  }

  Ok(())
}

/// The file under `<base>/<game>/` recording which profile is active.
const SELECTION_FILE: &str = "selected_profile";

/// The path of the persisted profile selection.
///
/// Public because two clients sharing one base directory must agree on this path
/// or they silently stop seeing each other's active profile.
pub fn selection_path(base: &Path, game: &str) -> PathBuf {
  base.join(game).join(SELECTION_FILE)
}

/// The persisted selection, or `None` when absent, empty, or unreadable.
pub fn read_selection(base: &Path, game: &str) -> Option<String> {
  let text = fs::read_to_string(selection_path(base, game)).ok()?;
  let name = text.trim();

  match name.is_empty() {
    true => None,
    false => Some(name.to_string()),
  }
}

/// Persists `name` as the selected profile.
pub fn write_selection(base: &Path, game: &str, name: &str) -> Result<()> {
  let path = selection_path(base, game);

  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent)
      .map_err(|e| Error::Profile(format!("creating {}: {}", parent.display(), e)))?;
  }

  fs::write(&path, name).map_err(|e| Error::Profile(format!("writing {}: {}", path.display(), e)))
}

/// Removes the persisted selection, returning to game-directory mode. Absent is
/// not an error.
pub fn clear_selection(base: &Path, game: &str) -> Result<()> {
  let path = selection_path(base, game);

  if !path.exists() {
    return Ok(());
  }

  fs::remove_file(&path).map_err(|e| Error::Profile(format!("removing {}: {}", path.display(), e)))
}

/// Why a profile name cannot be used.
///
/// Returned rather than flattened into a message so a caller wording the refusal
/// itself, e.g. to offer to create the profile, gets the reason and the path it
/// would go at instead of a sentence it would have to re-derive both from.
/// Travels inside [`Error::BadProfile`] out of operations that have other ways
/// to fail, so no such operation has to give up its single error type or its
/// composition to keep the reason intact.
///
/// Deliberately **not** `#[non_exhaustive]`, matching [`super::NotRemovable`]: a
/// caller that words each refusal should stop compiling when a new reason
/// appears, rather than printing a generic fallback for a case nobody worded.
/// [`Error`] being `#[non_exhaustive]` does not weaken that: the wildcard arm a
/// client needs there is for other kinds of failure, not for other refusals.
#[derive(Debug)]
pub enum BadProfile {
  /// The name is not a safe single path component, so it could escape the
  /// profiles directory. See [`validate_name`].
  InvalidName {
    /// The name as given.
    name: String,
  },
  /// No profile directory exists at `dir`.
  Missing {
    /// The name as given.
    name: String,
    /// Where the profile was expected.
    dir: PathBuf,
  },
}

impl std::fmt::Display for BadProfile {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::InvalidName { name } => write!(f, "invalid profile name {name:?}"),
      Self::Missing { name, .. } => write!(f, "there is no profile named {name:?}"),
    }
  }
}

impl std::error::Error for BadProfile {}

/// The directory of an existing profile, classifying why not when there is none.
///
/// The single place that decides "is this a usable profile name": both
/// [`use_profile`] and [`crate::profile::target::InstallTarget::resolve`] go
/// through it, so they cannot drift apart, and a caller that wants to word the
/// refusal itself gets the reason rather than a sentence.
pub fn require_profile(
  base: &Path,
  game: &str,
  name: &str,
) -> std::result::Result<PathBuf, BadProfile> {
  validate_name(name).map_err(|_| BadProfile::InvalidName {
    name: name.to_string(),
  })?;

  let dir = profile_dir(base, game, name);

  if !dir.is_dir() {
    return Err(BadProfile::Missing {
      name: name.to_string(),
      dir,
    });
  }

  Ok(dir)
}

/// Selects `name` as the active profile, refusing one that does not exist.
pub fn use_profile(base: &Path, game: &str, name: &str) -> Result<()> {
  require_profile(base, game, name)?;

  write_selection(base, game, name)
}

/// Chooses the active profile from a caller's override and the persisted
/// selection. Pure, so the precedence is testable without touching disk.
///
/// An explicit `flag` wins. `no_profile` opts out of a persisted selection.
/// Otherwise the persisted selection applies, and its absence means
/// game-directory mode.
pub fn select_profile(
  flag: Option<&str>,
  no_profile: bool,
  persisted: Option<&str>,
) -> Option<String> {
  if let Some(name) = flag {
    return Some(name.to_string());
  }

  if no_profile {
    return None;
  }

  persisted.map(|name| name.to_string())
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs;
  use tempfile::tempdir;

  #[test]
  fn paths_follow_r2modman_layout() {
    let base = Path::new("/base");

    assert_eq!(
      profiles_dir(base, "valheim"),
      Path::new("/base/valheim/profiles")
    );
    assert_eq!(
      profile_dir(base, "valheim", "Main"),
      Path::new("/base/valheim/profiles/Main")
    );
  }

  #[test]
  fn validate_name_rejects_unsafe() {
    assert!(validate_name("Main").is_ok());
    assert!(validate_name("").is_err());
    assert!(validate_name("..").is_err());
    assert!(validate_name("a/b").is_err());
    assert!(validate_name("a\\b").is_err());
  }

  #[test]
  fn create_list_rename_duplicate_delete() {
    let dir = tempdir().unwrap();
    let base = dir.path();

    assert_eq!(list(base, "valheim").unwrap(), Vec::<String>::new());

    create(base, "valheim", "Main").unwrap();

    assert!(profile_dir(base, "valheim", "Main").is_dir());
    assert!(create(base, "valheim", "Main").is_err()); // already exists

    fs::write(profile_dir(base, "valheim", "Main").join("mods.yml"), "[]").unwrap();

    duplicate(base, "valheim", "Main", "Copy").unwrap();

    assert!(
      profile_dir(base, "valheim", "Copy")
        .join("mods.yml")
        .exists()
    );

    rename(base, "valheim", "Copy", "Renamed").unwrap();

    let mut names = list(base, "valheim").unwrap();
    names.sort();

    assert_eq!(names, vec!["Main".to_string(), "Renamed".to_string()]);

    delete(base, "valheim", "Main").unwrap();

    assert_eq!(list(base, "valheim").unwrap(), vec!["Renamed".to_string()]);
  }

  #[test]
  fn the_selection_follows_a_rename_and_clears_on_delete() {
    let base = tempdir().unwrap();
    let root = base.path();

    create(root, "valheim", "one").unwrap();
    create(root, "valheim", "two").unwrap();

    assert_eq!(read_selection(root, "valheim"), None);

    use_profile(root, "valheim", "one").unwrap();

    assert_eq!(read_selection(root, "valheim"), Some("one".to_string()));

    // Renaming the selected profile must carry the selection with it, or the
    // selection is left naming a directory that no longer exists.
    rename(root, "valheim", "one", "renamed").unwrap();

    assert_eq!(read_selection(root, "valheim"), Some("renamed".to_string()));

    // Renaming a profile that is not selected must not disturb the selection.
    rename(root, "valheim", "two", "other").unwrap();

    assert_eq!(read_selection(root, "valheim"), Some("renamed".to_string()));

    // Deleting a profile that is not selected leaves the selection alone.
    delete(root, "valheim", "other").unwrap();

    assert_eq!(read_selection(root, "valheim"), Some("renamed".to_string()));

    // Deleting the selected one clears it, returning to game-directory mode.
    delete(root, "valheim", "renamed").unwrap();

    assert_eq!(read_selection(root, "valheim"), None);

    // Selecting something that does not exist is refused.
    assert!(use_profile(root, "valheim", "ghost").is_err());
    // Clearing an already-absent selection is not an error.
    clear_selection(root, "valheim").unwrap();
  }

  #[test]
  fn require_profile_classifies_why_a_name_is_unusable() {
    let base = tempdir().unwrap();
    let root = base.path();

    create(root, "valheim", "one").unwrap();

    assert_eq!(
      require_profile(root, "valheim", "one").unwrap(),
      profile_dir(root, "valheim", "one")
    );

    let unsafe_name = require_profile(root, "valheim", "../escape").unwrap_err();

    assert!(
      matches!(&unsafe_name, BadProfile::InvalidName { name } if name == "../escape"),
      "an unsafe name must be told apart from an absent one; got: {unsafe_name:?}"
    );

    let missing = require_profile(root, "valheim", "ghost").unwrap_err();

    // The expected directory comes back with the refusal, so a caller offering
    // to create the profile does not have to recompute where it would go.
    assert!(
      matches!(&missing, BadProfile::Missing { name, dir }
        if name == "ghost" && dir == &profile_dir(root, "valheim", "ghost")),
      "a missing profile must carry its name and expected directory; got: {missing:?}"
    );
  }

  #[test]
  fn use_profile_and_resolve_refuse_a_missing_profile_identically() {
    let base = tempdir().unwrap();
    let root = base.path();
    let game_dir = tempdir().unwrap();

    // Both entry points classify through `require_profile`, so neither can drift
    // into wording the same refusal differently.
    let selecting = use_profile(root, "valheim", "ghost").unwrap_err();
    let resolving = crate::profile::target::InstallTarget::resolve(
      root.to_path_buf(),
      game_dir.path().to_path_buf(),
      "valheim",
      Some("ghost"),
    )
    .unwrap_err();

    assert_eq!(selecting.to_string(), resolving.to_string());
    assert_eq!(
      selecting.to_string(),
      "Profile error: there is no profile named \"ghost\""
    );

    // Not just the same sentence: the same matchable reason, so a caller wording
    // it does not have to care which entry point refused.
    assert!(
      matches!(selecting, Error::BadProfile(BadProfile::Missing { .. }))
        && matches!(resolving, Error::BadProfile(BadProfile::Missing { .. })),
      "both must carry the reason, not a flattened message"
    );

    let selecting = use_profile(root, "valheim", "a/b").unwrap_err();
    let resolving = crate::profile::target::InstallTarget::resolve(
      root.to_path_buf(),
      game_dir.path().to_path_buf(),
      "valheim",
      Some("a/b"),
    )
    .unwrap_err();

    assert_eq!(selecting.to_string(), resolving.to_string());
    assert_eq!(
      selecting.to_string(),
      "Profile error: invalid profile name \"a/b\""
    );
  }

  #[test]
  fn select_profile_precedence() {
    // An explicit flag wins over everything.
    assert_eq!(
      select_profile(Some("experiment"), false, Some("saved")),
      Some("experiment".to_string())
    );
    // Opting out ignores a persisted selection.
    assert_eq!(select_profile(None, true, Some("saved")), None);
    // A persisted selection applies when nothing overrides it.
    assert_eq!(
      select_profile(None, false, Some("saved")),
      Some("saved".to_string())
    );
    // Nothing set at all means game-directory mode.
    assert_eq!(select_profile(None, false, None), None);
  }
}
