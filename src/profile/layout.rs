//! Directory layout and profile lifecycle.
//!
//! Profiles live at `<base>/<game>/profiles/<name>/`, matching r2modman so a
//! profile directory produced here works when copied into r2modman's data dir.
//! The caller owns `base` (platform/path discovery stays outside the engine).
//!
//! # Errors
//!
//! A refused or missing profile name is
//! [`Error::BadProfile`](crate::Error::BadProfile); every other failure is
//! [`Error::Profile`](crate::Error::Profile). Underlying IO failures are wrapped
//! with the offending path rather than surfaced as
//! [`Error::Io`](crate::Error::Io), so a caller can render the message directly
//! and does not need to prepend a path itself.
//!
//! A name for a new profile (`create`, and the destination of `rename` and
//! `duplicate`) goes through
//! [`validate_name`](crate::profile::layout::validate_name), so a name that
//! could escape the profiles directory, or that Windows cannot hold, is refused
//! before the filesystem is touched. A name for an existing profile is refused
//! only if it could escape the profiles directory, so a profile named under
//! older, looser rules stays usable. Individual functions document only the
//! conditions beyond that shared contract.

#![deny(missing_docs)]

#[cfg(feature = "io")]
use crate::error::{Error, Result};
#[cfg(feature = "io")]
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

/// Refuses a profile name that could escape the profiles directory or that
/// Windows cannot hold as a folder name, with the reason as a [`BadName`].
///
/// The Windows rules apply on every OS so a profile stays portable. The error
/// is always [`BadProfile::InvalidName`].
///
/// # Examples
///
/// ```
/// use thunderstore_engine::profile::layout::{BadName, BadProfile, validate_name};
///
/// assert!(validate_name("Main").is_ok());
/// assert!(matches!(
///   validate_name("CON.txt"),
///   Err(BadProfile::InvalidName { reason: BadName::Reserved, .. })
/// ));
/// ```
pub fn validate_name(name: &str) -> std::result::Result<(), BadProfile> {
  refuse_if(
    name,
    escape_problem(name).or_else(|| portability_problem(name)),
  )
}

/// Refuses the name of an existing profile only when it could escape the
/// profiles directory, so a profile named under older, looser rules can still
/// be used, renamed, and deleted.
#[cfg(feature = "io")]
pub(crate) fn validate_existing_name(name: &str) -> std::result::Result<(), BadProfile> {
  refuse_if(name, escape_problem(name))
}

fn refuse_if(name: &str, problem: Option<BadName>) -> std::result::Result<(), BadProfile> {
  match problem {
    Some(reason) => Err(BadProfile::InvalidName {
      name: name.to_string(),
      reason,
    }),
    None => Ok(()),
  }
}

/// Why `name` could resolve outside the profiles directory, if it could.
fn escape_problem(name: &str) -> Option<BadName> {
  if name.is_empty() {
    return Some(BadName::Empty);
  }

  // Windows drops trailing dots and spaces from a path's last component, so
  // `" "` or `"..."` would resolve to the profiles directory itself.
  let resolved = name.trim_end_matches(['.', ' ']);

  if matches!(resolved, "" | "." | "..") || name.contains(['/', '\\']) {
    return Some(BadName::Escapes);
  }

  let mut chars = name.chars();

  if matches!((chars.next(), chars.next()), (Some(drive), Some(':')) if drive.is_ascii_alphabetic())
  {
    return Some(BadName::DrivePrefix);
  }

  None
}

/// The first Windows folder-name rule `name` breaks, checked from most to least
/// specific.
fn portability_problem(name: &str) -> Option<BadName> {
  if let Some(found) = name
    .chars()
    .find(|c| c.is_ascii_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
  {
    return Some(BadName::ForbiddenChar(found));
  }

  if name.ends_with(['.', ' ']) {
    return Some(BadName::TrailingDotOrSpace);
  }

  if is_reserved_device(name) {
    return Some(BadName::Reserved);
  }

  None
}

/// Whether the part of `name` before its first `.` is a Windows device name,
/// per Microsoft's "Naming Files, Paths, and Namespaces", which also reserves
/// `COM` and `LPT` followed by 0 or a superscript 1, 2, or 3.
fn is_reserved_device(name: &str) -> bool {
  let stem = name
    .split('.')
    .next()
    .unwrap_or(name)
    .trim_end_matches(' ')
    .to_ascii_uppercase();

  match stem.as_str() {
    "CON" | "PRN" | "AUX" | "NUL" => true,
    _ => {
      let numbered = stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"));

      matches!(
        numbered,
        Some(
          "0"
            | "1"
            | "2"
            | "3"
            | "4"
            | "5"
            | "6"
            | "7"
            | "8"
            | "9"
            | "\u{b9}"
            | "\u{b2}"
            | "\u{b3}"
        )
      )
    }
  }
}

/// Lists profile names (immediate subdirectories) for a game, sorted. A missing
/// profiles directory yields an empty list.
#[cfg(feature = "io")]
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
#[cfg(feature = "io")]
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
/// [`BadProfile::Missing`] if the profile does not exist, so deleting an
/// already-deleted profile is reported rather than treated as success.
#[cfg(feature = "io")]
pub fn delete(base: &Path, game: &str, name: &str) -> Result<()> {
  let dir = require_profile(base, game, name)?;

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
/// [`BadProfile::Missing`] if `from` does not exist. Errors if `to` already
/// exists, so a rename never clobbers another profile.
///
/// Not transactional: the rename can succeed and the selection update then fail,
/// leaving the selection naming a profile that no longer exists. Callers that
/// care can re-read it with [`read_selection`].
#[cfg(feature = "io")]
pub fn rename(base: &Path, game: &str, from: &str, to: &str) -> Result<()> {
  let src = require_profile(base, game, from)?;

  validate_name(to)?;

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
/// [`BadProfile::Missing`] if `from` does not exist. Errors if `to` already
/// exists. A failed copy can leave `to` partially written; it is not cleaned
/// up.
#[cfg(feature = "io")]
pub fn duplicate(base: &Path, game: &str, from: &str, to: &str) -> Result<PathBuf> {
  let src = require_profile(base, game, from)?;

  validate_name(to)?;

  let dst = profile_dir(base, game, to);

  if dst.exists() {
    return Err(Error::Profile(format!("profile {to:?} already exists")));
  }

  copy_tree(&src, &dst)?;

  Ok(dst)
}

/// Recursively copies a directory tree, creating `dst`. Skips symlinks (never
/// follows them out of the tree), like the install layer's `walk_files`.
#[cfg(feature = "io")]
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
#[cfg(feature = "io")]
pub fn read_selection(base: &Path, game: &str) -> Option<String> {
  let text = fs::read_to_string(selection_path(base, game)).ok()?;
  let name = text.trim();

  match name.is_empty() {
    true => None,
    false => Some(name.to_string()),
  }
}

/// Persists `name` as the selected profile.
#[cfg(feature = "io")]
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
#[cfg(feature = "io")]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BadProfile {
  /// The name is refused by [`validate_name`] or, for an existing profile,
  /// could escape the profiles directory.
  InvalidName {
    /// The name as given.
    name: String,
    /// Which rule the name breaks.
    reason: BadName,
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
      Self::InvalidName { name, .. } => write!(f, "invalid profile name {name:?}"),
      Self::Missing { name, .. } => write!(f, "there is no profile named {name:?}"),
    }
  }
}

impl std::error::Error for BadProfile {}

/// Why [`validate_name`] refuses a profile name.
///
/// Not `#[non_exhaustive]`, for the reason [`BadProfile`] gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BadName {
  /// The name is empty.
  Empty,
  /// The name is `.` or `..`, or contains `/` or `\`, so it could resolve
  /// outside the profiles directory.
  Escapes,
  /// The name starts with a drive letter and a colon (`C:foo`), which Windows
  /// resolves on that drive instead of under the profiles directory.
  DrivePrefix,
  /// A Windows device name (`CON`, `PRN`, `AUX`, `NUL`, `COM1`-`COM9`,
  /// `LPT1`-`LPT9`, and `COM`/`LPT` with a superscript `¹`, `²`, or `³`), in
  /// any case, alone or followed by an extension.
  Reserved,
  /// A character Windows forbids in a folder name: `< > : " | ? *` or a
  /// control character.
  ForbiddenChar(char),
  /// The name ends with `.` or a space, which Windows strips.
  TrailingDotOrSpace,
}

impl std::fmt::Display for BadName {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Empty => write!(f, "the name is empty"),
      Self::Escapes => write!(f, "the name is a path, not a single folder name"),
      Self::DrivePrefix => write!(f, "the name starts with a drive letter"),
      Self::Reserved => write!(f, "the name is reserved by Windows"),
      Self::ForbiddenChar(found) => write!(f, "the name contains {found:?}"),
      Self::TrailingDotOrSpace => write!(f, "the name ends with a dot or a space"),
    }
  }
}

/// The directory of an existing profile, classifying why not when there is none.
///
/// The single place that decides "is this a usable profile name": both
/// [`use_profile`] and [`crate::profile::target::InstallTarget::resolve`] go
/// through it, so they cannot drift apart, and a caller that wants to word the
/// refusal itself gets the reason rather than a sentence.
///
/// Only a name that could escape the profiles directory is refused as
/// [`BadProfile::InvalidName`]; the stricter [`validate_name`] rules apply to
/// new names, so a profile named under older rules can still be found here.
#[cfg(feature = "io")]
pub fn require_profile(
  base: &Path,
  game: &str,
  name: &str,
) -> std::result::Result<PathBuf, BadProfile> {
  validate_existing_name(name)?;

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
#[cfg(feature = "io")]
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
  #[cfg(feature = "io")]
  use std::fs;
  #[cfg(feature = "io")]
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

  fn refusal(name: &str) -> Option<BadName> {
    match validate_name(name) {
      Ok(()) => None,
      Err(BadProfile::InvalidName { reason, .. }) => Some(reason),
      Err(other) => panic!("{name:?} must be refused as a name; got: {other:?}"),
    }
  }

  #[test]
  fn names_windows_cannot_hold_are_refused_on_every_os() {
    let cases = [
      ("", BadName::Empty),
      (".", BadName::Escapes),
      ("..", BadName::Escapes),
      ("a/b", BadName::Escapes),
      ("a\\b", BadName::Escapes),
      ("/abs", BadName::Escapes),
      ("C:foo", BadName::DrivePrefix),
      ("c:", BadName::DrivePrefix),
      ("CON", BadName::Reserved),
      ("con", BadName::Reserved),
      ("Nul.txt", BadName::Reserved),
      ("aux.tar.gz", BadName::Reserved),
      ("NUL .txt", BadName::Reserved),
      ("PRN", BadName::Reserved),
      ("COM1", BadName::Reserved),
      ("com9.log", BadName::Reserved),
      ("LPT5", BadName::Reserved),
      ("COM0", BadName::Reserved),
      ("lpt0.bak", BadName::Reserved),
      ("COM\u{b9}", BadName::Reserved),
      ("com\u{b2}.log", BadName::Reserved),
      ("COM\u{b3}", BadName::Reserved),
      ("LPT\u{b9}", BadName::Reserved),
      ("lpt\u{b2}", BadName::Reserved),
      ("LPT\u{b3}.txt", BadName::Reserved),
      ("a:b", BadName::DrivePrefix),
      ("ab:c", BadName::ForbiddenChar(':')),
      ("why?", BadName::ForbiddenChar('?')),
      ("a*b", BadName::ForbiddenChar('*')),
      ("a<b", BadName::ForbiddenChar('<')),
      ("a>b", BadName::ForbiddenChar('>')),
      ("a|b", BadName::ForbiddenChar('|')),
      ("say \"hi\"", BadName::ForbiddenChar('"')),
      ("tab\there", BadName::ForbiddenChar('\t')),
      ("nul\0byte", BadName::ForbiddenChar('\0')),
      ("trailing.", BadName::TrailingDotOrSpace),
      ("trailing ", BadName::TrailingDotOrSpace),
      ("...", BadName::Escapes),
    ];

    for (name, expected) in cases {
      assert_eq!(refusal(name), Some(expected), "for {name:?}");
    }
  }

  #[test]
  fn names_that_only_resemble_reserved_ones_are_allowed() {
    for name in [
      "Main",
      "work profile",
      "CONSOLE",
      "COM10",
      "COM\u{2074}",
      "LPT\u{b9}\u{b2}",
      "nul-run",
      "my.profile",
      ".hidden",
      "1.0",
    ] {
      assert_eq!(refusal(name), None, "for {name:?}");
    }
  }

  #[test]
  fn a_refused_name_keeps_its_message_and_carries_the_reason() {
    let error = crate::Error::from(validate_name("CON").unwrap_err());

    assert_eq!(
      error.to_string(),
      "Profile error: invalid profile name \"CON\""
    );
    assert!(matches!(
      error,
      crate::Error::BadProfile(BadProfile::InvalidName {
        reason: BadName::Reserved,
        ..
      })
    ));
  }

  #[test]
  #[cfg(feature = "io")]
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
  #[cfg(feature = "io")]
  fn delete_rename_and_duplicate_refuse_a_missing_profile_by_reason() {
    let base = tempdir().unwrap();
    let root = base.path();
    let expected = profile_dir(root, "valheim", "ghost");
    let is_missing = |error: Error| {
      matches!(error, Error::BadProfile(BadProfile::Missing { name, dir })
        if name == "ghost" && dir == expected)
    };

    assert!(is_missing(delete(root, "valheim", "ghost").unwrap_err()));
    assert!(is_missing(
      rename(root, "valheim", "ghost", "other").unwrap_err()
    ));
    assert!(is_missing(
      duplicate(root, "valheim", "ghost", "other").unwrap_err()
    ));
    assert!(
      !profile_dir(root, "valheim", "other").exists(),
      "a refused duplicate must not leave a target behind"
    );
  }

  #[test]
  #[cfg(feature = "io")]
  fn create_rename_and_duplicate_refuse_a_name_windows_cannot_hold() {
    let base = tempdir().unwrap();
    let root = base.path();

    create(root, "valheim", "Main").unwrap();

    for error in [
      create(root, "valheim", "C:evil").unwrap_err(),
      rename(root, "valheim", "Main", "CON").unwrap_err(),
      duplicate(root, "valheim", "Main", "copy.").unwrap_err(),
    ] {
      assert!(
        matches!(error, Error::BadProfile(BadProfile::InvalidName { .. })),
        "got: {error:?}"
      );
    }

    assert_eq!(list(root, "valheim").unwrap(), vec!["Main".to_string()]);
  }

  #[test]
  #[cfg(all(unix, feature = "io"))]
  fn a_profile_named_under_the_old_rules_stays_usable() {
    let base = tempdir().unwrap();
    let root = base.path();
    let legacy = ["Modded.", "v2: test", "aux"];

    for name in legacy {
      fs::create_dir_all(profile_dir(root, "valheim", name)).unwrap();
    }

    assert_eq!(
      list(root, "valheim").unwrap(),
      vec!["Modded.", "aux", "v2: test"]
    );

    use_profile(root, "valheim", "Modded.").unwrap();
    rename(root, "valheim", "Modded.", "Modded").unwrap();

    assert_eq!(read_selection(root, "valheim"), Some("Modded".to_string()));

    duplicate(root, "valheim", "v2: test", "v2 copy").unwrap();
    delete(root, "valheim", "aux").unwrap();
    delete(root, "valheim", "v2: test").unwrap();

    assert_eq!(list(root, "valheim").unwrap(), vec!["Modded", "v2 copy"]);

    let resolved = crate::profile::target::InstallTarget::resolve(
      root.to_path_buf(),
      root.join("game"),
      "valheim",
      Some("v2 copy"),
    )
    .unwrap();

    assert_eq!(resolved.dir, profile_dir(root, "valheim", "v2 copy"));
  }

  #[test]
  #[cfg(all(unix, feature = "io"))]
  fn an_existing_profile_still_cannot_be_renamed_to_a_name_windows_cannot_hold() {
    let base = tempdir().unwrap();
    let root = base.path();

    fs::create_dir_all(profile_dir(root, "valheim", "aux")).unwrap();

    for error in [
      rename(root, "valheim", "aux", "nul").unwrap_err(),
      duplicate(root, "valheim", "aux", "copy.").unwrap_err(),
    ] {
      assert!(
        matches!(error, Error::BadProfile(BadProfile::InvalidName { .. })),
        "got: {error:?}"
      );
    }
  }

  #[test]
  #[cfg(feature = "io")]
  fn an_existing_profile_name_that_escapes_is_still_refused() {
    let base = tempdir().unwrap();
    let root = base.path();

    for (name, expected) in [
      ("", BadName::Empty),
      (".", BadName::Escapes),
      ("..", BadName::Escapes),
      (" ", BadName::Escapes),
      ("...", BadName::Escapes),
      (". .", BadName::Escapes),
      (".. ", BadName::Escapes),
      ("../escape", BadName::Escapes),
      ("a\\b", BadName::Escapes),
      ("C:evil", BadName::DrivePrefix),
    ] {
      let refused = require_profile(root, "valheim", name).unwrap_err();

      assert_eq!(
        refused,
        BadProfile::InvalidName {
          name: name.to_string(),
          reason: expected,
        },
        "for {name:?}"
      );
    }
  }

  #[test]
  #[cfg(feature = "io")]
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
  #[cfg(feature = "io")]
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
      matches!(&unsafe_name, BadProfile::InvalidName { name, .. } if name == "../escape"),
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
  #[cfg(feature = "io")]
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
