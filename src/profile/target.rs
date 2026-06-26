//! Where an install goes.
//!
//! Two modes share one type: a named r2modman profile under
//! `<base>/<game>/profiles/<name>`, or the game directory itself. Every operation
//! takes [`InstallTarget::dir`](crate::profile::target::InstallTarget::dir), so the modes differ by exactly one path.

#![deny(missing_docs)]

use crate::error::Result;
use crate::profile::layout;
use std::path::PathBuf;

/// Where this invocation installs, tracks, and launches from.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct InstallTarget {
  /// The install root. The record and its `_state` directory live here.
  pub dir: PathBuf,
  /// The base holding `<game>/{cache,exports,profiles}`.
  pub base: PathBuf,
  /// The game root. Known in both modes: profile mode still needs it for
  /// pre-launch staging and executable resolution.
  pub game_dir: PathBuf,
  /// The ecosystem game key.
  pub game: String,
  /// `Some(name)` in profile mode, `None` in game-directory mode.
  pub profile: Option<String>,
}

impl InstallTarget {
  /// Resolves the install root, erroring rather than creating a profile
  /// directory that does not exist yet.
  ///
  /// Classifies a profile name through [`layout::require_profile`], the same
  /// gate [`layout::use_profile`] uses, so selecting a bad name and resolving
  /// one cannot refuse differently. The reason reaches a caller intact as
  /// [`Error::BadProfile`](crate::error::Error::BadProfile), so wording the
  /// refusal does not mean re-checking the name.
  pub fn resolve(
    base: PathBuf,
    game_dir: PathBuf,
    game: &str,
    profile: Option<&str>,
  ) -> Result<Self> {
    let dir = match profile {
      Some(name) => layout::require_profile(&base, game, name)?,
      None => game_dir.clone(),
    };

    Ok(Self {
      dir,
      base,
      game_dir,
      game: game.to_string(),
      profile: profile.map(|name| name.to_string()),
    })
  }

  /// This target's record, the authoritative install list.
  pub fn mods_yml(&self) -> PathBuf {
    self.dir.join("mods.yml")
  }

  /// Where `.r2z` exports are written.
  pub fn exports_dir(&self) -> PathBuf {
    self.base.join(&self.game).join("exports")
  }

  /// The name used wherever a profile name is required: the active profile, or
  /// `default` in game-directory mode.
  pub fn name(&self) -> &str {
    self.profile.as_deref().unwrap_or("default")
  }

  /// Whether pre-launch staging has anywhere to copy to.
  ///
  /// False when the install root *is* the game directory, where every root file
  /// already sits at its staging destination. Staging detects a same-file
  /// destination itself, so this is a second line of defence and a way to skip a
  /// pointless directory walk in the common case.
  ///
  /// Also load-bearing when the game directory does not exist yet:
  /// [`crate::profile::launch::stage_for_launch_in`] does not detect that case
  /// itself and returns the `read_dir` error from listing a missing directory
  /// rather than an empty list, so a caller should check this first rather
  /// than call staging unconditionally.
  pub fn needs_staging(&self) -> bool {
    let dir = self.dir.canonicalize().unwrap_or_else(|_| self.dir.clone());
    let game = self
      .game_dir
      .canonicalize()
      .unwrap_or_else(|_| self.game_dir.clone());

    dir != game
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use tempfile::tempdir;

  #[test]
  fn game_dir_mode_targets_the_game_dir_and_never_stages() {
    let base = tempdir().unwrap();
    let game_dir = tempdir().unwrap();
    let target = InstallTarget::resolve(
      base.path().to_path_buf(),
      game_dir.path().to_path_buf(),
      "valheim",
      None,
    )
    .unwrap();

    assert_eq!(target.dir, game_dir.path());
    assert_eq!(target.name(), "default");
    assert_eq!(target.mods_yml(), game_dir.path().join("mods.yml"));
    // Every root file is already at its staging destination.
    assert!(!target.needs_staging());
  }

  #[test]
  fn profile_mode_targets_the_profile_dir_and_stages() {
    let base = tempdir().unwrap();
    let game_dir = tempdir().unwrap();

    crate::profile::layout::create(base.path(), "valheim", "experiment").unwrap();

    let target = InstallTarget::resolve(
      base.path().to_path_buf(),
      game_dir.path().to_path_buf(),
      "valheim",
      Some("experiment"),
    )
    .unwrap();

    assert_eq!(
      target.dir,
      base
        .path()
        .join("valheim")
        .join("profiles")
        .join("experiment")
    );
    assert_eq!(target.name(), "experiment");
    assert!(target.needs_staging());
    assert_eq!(
      target.exports_dir(),
      base.path().join("valheim").join("exports")
    );
  }

  #[test]
  fn resolve_rejects_a_missing_profile() {
    let base = tempdir().unwrap();
    let game_dir = tempdir().unwrap();

    let err = InstallTarget::resolve(
      base.path().to_path_buf(),
      game_dir.path().to_path_buf(),
      "valheim",
      Some("nope"),
    )
    .unwrap_err();

    assert!(
      err.to_string().contains("there is no profile named"),
      "expected missing profile error, got: {err}"
    );
  }

  #[test]
  fn resolve_rejects_an_unsafe_profile_name() {
    let base = tempdir().unwrap();
    let game_dir = tempdir().unwrap();

    let err = InstallTarget::resolve(
      base.path().to_path_buf(),
      game_dir.path().to_path_buf(),
      "valheim",
      Some("../escape"),
    )
    .unwrap_err();

    assert!(
      err.to_string().contains("invalid profile name"),
      "expected unsafe name error, got: {err}"
    );
  }
}
