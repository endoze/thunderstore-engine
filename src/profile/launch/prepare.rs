//! Launch preparation: every step between choosing to launch and spawning the
//! game, in the order a client must run them.

use super::{
  HostOs, LOADER_FILES, LaunchContext, LaunchFacts, LaunchMode, LaunchPlan, launch_plan_from,
};
#[cfg(feature = "io")]
use super::{LaunchProgram, Runtime, proxy_dll};
#[cfg(feature = "io")]
use super::{
  StagedFiles, ensure_loader_scripts_executable, stage_files_in, steam_launch_options,
  wrapper_path, write_wrapper_script,
};
use crate::ecosystem::{Ecosystem, LoaderKind};
use crate::error::{Error, Result};
#[cfg(feature = "io")]
use crate::profile::target::InstallTarget;
use std::path::{Path, PathBuf};

/// Something a client should tell the user about a prepared launch.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LaunchNotice {
  /// The launch goes through Steam, which runs the loader only once
  /// `launch_options` are pasted into the game's Steam launch options. The
  /// wrapper `script` they name has been written.
  SteamWrapper {
    /// The wrapper script Steam must invoke.
    script: PathBuf,
    /// The line to paste into the game's Steam launch options.
    launch_options: String,
  },
  /// The launch goes through Steam under Proton, which starts the game with its
  /// own environment, so the loader's proxy DLL loads only once
  /// `launch_options` are pasted into the game's Steam launch options. Without
  /// them Wine uses its builtin DLL and the game starts unmodded.
  ProtonDllOverride {
    /// The proxy DLL Wine must take from the game folder.
    dll: String,
    /// The line to paste into the game's Steam launch options.
    launch_options: String,
  },
  /// A loader script lacked its execute bit and was given one.
  MadeExecutable {
    /// The script whose mode changed.
    path: PathBuf,
  },
}

/// A launch that is ready to spawn, from [`prepare_launch`].
#[cfg(feature = "io")]
#[derive(Debug)]
#[non_exhaustive]
pub struct PreparedLaunch {
  /// The process to spawn.
  pub command: std::process::Command,
  /// The plan `command` was built from.
  pub plan: LaunchPlan,
  /// The files this launch created in the game folder, which a client records
  /// so it can remove them later.
  pub staged: Vec<PathBuf>,
  /// The files already in the game folder that this launch overwrote. One may
  /// be the client's own copy from an earlier launch or the player's own file,
  /// so a client removes one only if it recorded it from an earlier launch.
  pub replaced: Vec<PathBuf>,
  /// What the user should be told, in the order it happened.
  pub notices: Vec<LaunchNotice>,
}

/// Validates a launch from `facts` about `target_dir` and computes its plan,
/// without touching disk. With the `io` feature, `prepare_launch` makes this
/// decision before it writes anything.
///
/// A modded launch needs the game's loader installed: its files are in the
/// target ([`LaunchFacts::preloader`] for BepInEx and BepisLoader,
/// [`LaunchFacts::loaders`] for the rest), or `mods.yml` records a loader pack
/// of that kind ([`LaunchFacts::recorded`]). A game whose loader is
/// [`LoaderKind::None`] or [`LoaderKind::Other`] needs none, and neither does a
/// vanilla launch.
///
/// # Errors
///
/// - [`Error::NoModLoader`] for a modded launch with no loader installed.
/// - Any error from [`launch_plan_from`].
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// use thunderstore_engine::Error;
/// use thunderstore_engine::ecosystem::{Ecosystem, LoaderKind};
/// use thunderstore_engine::profile::launch::{
///   HostOs, LaunchContext, LaunchFacts, LaunchMode, Runtime, StorePlatform, prepare_plan_from,
/// };
///
/// let ctx = LaunchContext::new(
///   HostOs::Windows,
///   StorePlatform::SteamDirect,
///   Runtime::Native,
///   LaunchMode::Modded,
/// );
///
/// let err = prepare_plan_from(
///   Path::new("C:/profiles/main"),
///   &LaunchFacts::default(),
///   &Ecosystem::bundled(),
///   "valheim",
///   &ctx,
/// )
/// .unwrap_err();
///
/// assert!(matches!(err, Error::NoModLoader { loader: LoaderKind::BepInEx }));
/// ```
pub fn prepare_plan_from(
  target_dir: &Path,
  facts: &LaunchFacts,
  eco: &Ecosystem,
  game: &str,
  ctx: &LaunchContext,
) -> Result<LaunchPlan> {
  if ctx.mode == LaunchMode::Modded && ctx.os != HostOs::MacOs {
    let loader = eco.require_profile(game)?.package_loader;

    if !loader_installed(facts, eco, loader) {
      return Err(Error::NoModLoader { loader });
    }
  }

  launch_plan_from(target_dir, facts, eco, game, ctx)
}

/// Prepares `target` for launch and returns the command to spawn.
///
/// Runs every step a launch needs, in order, and validates before writing:
///
/// 1. Refuses a missing game folder.
/// 2. Reads the target's [`LaunchFacts`], decides with [`prepare_plan_from`],
///    and builds the command, which finds the game executable.
/// 3. Writes the Steam wrapper script when the plan needs one.
/// 4. Marks the target's loader scripts executable.
/// 5. Copies the loader files into the game folder for a modded launch, keeping
///    files it created apart from files it overwrote.
///
/// The caller spawns [`PreparedLaunch::command`] and shows
/// [`PreparedLaunch::notices`].
///
/// # Errors
///
/// - [`Error::MissingGameDir`] when `target.game_dir` is not a directory.
/// - Any error from [`prepare_plan_from`] or [`LaunchPlan::to_command`], before
///   anything is written.
/// - Any error writing the wrapper, marking scripts executable, or staging. A
///   staging error first removes the game-folder files this launch created.
#[cfg(feature = "io")]
pub fn prepare_launch(
  target: &InstallTarget,
  eco: &Ecosystem,
  ctx: &LaunchContext,
) -> Result<PreparedLaunch> {
  if !target.game_dir.is_dir() {
    return Err(Error::MissingGameDir {
      path: target.game_dir.clone(),
    });
  }

  let facts = LaunchFacts::read(&target.dir);
  let plan = prepare_plan_from(&target.dir, &facts, eco, &target.game, ctx)?;
  let command = plan.to_command(&target.game_dir)?;
  let mut notices = Vec::new();

  if plan.steam_wrapper.is_some() {
    let script = wrapper_path(&target.base, &target.game);

    write_wrapper_script(&script)?;
    notices.push(LaunchNotice::SteamWrapper {
      launch_options: steam_launch_options(&script),
      script,
    });
  }

  if ctx.mode == LaunchMode::Modded
    && ctx.runtime == Runtime::Proton
    && matches!(plan.program, LaunchProgram::Steam { .. })
  {
    let dll = proxy_dll(eco.require_profile(&target.game)?.package_loader);

    notices.push(LaunchNotice::ProtonDllOverride {
      dll: dll.to_string(),
      launch_options: format!("WINEDLLOVERRIDES=\"{dll}=n,b\" %command%"),
    });
  }

  for path in ensure_loader_scripts_executable(&target.dir)? {
    notices.push(LaunchNotice::MadeExecutable { path });
  }

  let staged = match ctx.mode {
    LaunchMode::Modded => stage_files_in(&target.dir, eco, &target.game, &target.game_dir, ctx)?,
    LaunchMode::Vanilla => StagedFiles::default(),
  };

  Ok(PreparedLaunch {
    command,
    plan,
    staged: staged.created,
    replaced: staged.replaced,
    notices,
  })
}

/// Whether `facts` show `loader` installed: by its files when the engine knows
/// them, otherwise by a recorded loader pack of that kind. A pack still
/// recorded after its files were deleted does not count for a loader whose
/// files are known, since the launch would start unmodded.
fn loader_installed(facts: &LaunchFacts, eco: &Ecosystem, loader: LoaderKind) -> bool {
  match loader {
    LoaderKind::None | LoaderKind::Other => true,
    LoaderKind::BepInEx | LoaderKind::BepisLoader => facts.preloader.is_some(),
    _ if LOADER_FILES.iter().any(|(kind, _)| *kind == loader) => facts.loaders.contains(&loader),
    _ => {
      facts.loaders.contains(&loader)
        || facts.recorded.iter().any(|name| {
          eco
            .modloader_package(name)
            .is_some_and(|pack| pack.loader == loader)
        })
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ecosystem::{LoaderKind, ModloaderPackage};
  use crate::error::Error;
  use crate::profile::launch::{HostOs, LaunchMode, Runtime, StorePlatform};

  const TARGET: &str = "/profiles/main";

  /// Every loader kind that needs a loader installed to load mods.
  const LOADERS: [LoaderKind; 12] = [
    LoaderKind::BepInEx,
    LoaderKind::BepisLoader,
    LoaderKind::GdWeave,
    LoaderKind::GodotMl,
    LoaderKind::Lovely,
    LoaderKind::MelonLoader,
    LoaderKind::RecursiveMelonLoader,
    LoaderKind::Northstar,
    LoaderKind::ReturnOfModding,
    LoaderKind::Rivet,
    LoaderKind::Shimloader,
    LoaderKind::Umm,
  ];

  fn windows(mode: LaunchMode) -> LaunchContext {
    LaunchContext::new(
      HostOs::Windows,
      StorePlatform::SteamDirect,
      Runtime::Native,
      mode,
    )
  }

  /// The bundled ecosystem with Valheim switched to `loader`.
  fn valheim_with(loader: LoaderKind) -> Ecosystem {
    let mut eco = Ecosystem::bundled();

    for profile in &mut eco.games.get_mut("valheim").unwrap().profiles {
      profile.package_loader = loader;
    }

    eco
  }

  fn prepare(facts: &LaunchFacts, eco: &Ecosystem, mode: LaunchMode) -> Result<LaunchPlan> {
    prepare_plan_from(Path::new(TARGET), facts, eco, "valheim", &windows(mode))
  }

  #[test]
  fn a_modded_launch_with_no_loader_names_the_missing_loader_for_every_kind() {
    for kind in LOADERS {
      let err = prepare(
        &LaunchFacts::default(),
        &valheim_with(kind),
        LaunchMode::Modded,
      )
      .unwrap_err();

      assert!(
        matches!(err, Error::NoModLoader { loader } if loader == kind),
        "{kind:?}: {err:?}"
      );
    }
  }

  #[test]
  fn a_loader_found_in_the_target_satisfies_a_modded_launch() {
    for kind in LOADERS {
      let mut facts = LaunchFacts::default();

      match kind {
        LoaderKind::BepInEx | LoaderKind::BepisLoader => {
          facts.preloader = Some("BepInEx.Preloader.dll".to_string());
        }
        _ => facts.loaders = vec![kind],
      }

      let plan = prepare(&facts, &valheim_with(kind), LaunchMode::Modded);

      assert!(plan.is_ok(), "{kind:?}: {plan:?}");
    }
  }

  #[test]
  fn a_recorded_pack_stands_in_only_for_a_loader_whose_files_are_unknown() {
    for kind in [LoaderKind::BepInEx, LoaderKind::Umm, LoaderKind::Shimloader] {
      let mut eco = valheim_with(kind);
      let mut facts = LaunchFacts::default();

      eco.modloader_packages = vec![ModloaderPackage {
        package_id: "Someone-Loader".to_string(),
        root_folder: String::new(),
        loader: kind,
      }];
      facts.recorded = vec!["Someone-Loader".to_string()];

      let deleted = prepare(&facts, &eco, LaunchMode::Modded);

      assert!(
        matches!(deleted, Err(Error::NoModLoader { .. })),
        "{kind:?}: {deleted:?}"
      );
    }
  }

  #[test]
  fn a_recorded_loader_pack_of_the_games_kind_satisfies_a_modded_launch() {
    for kind in [
      LoaderKind::GodotMl,
      LoaderKind::Lovely,
      LoaderKind::ReturnOfModding,
    ] {
      let mut eco = valheim_with(kind);
      let mut facts = LaunchFacts::default();

      eco.modloader_packages = vec![
        ModloaderPackage {
          package_id: "Someone-RightLoader".to_string(),
          root_folder: String::new(),
          loader: kind,
        },
        ModloaderPackage {
          package_id: "Someone-WrongLoader".to_string(),
          root_folder: String::new(),
          loader: LoaderKind::Other,
        },
      ];
      facts.recorded = vec!["Someone-WrongLoader".to_string()];

      let wrong = prepare(&facts, &eco, LaunchMode::Modded);

      facts.recorded.push("Someone-RightLoader".to_string());

      let right = prepare(&facts, &eco, LaunchMode::Modded);

      assert!(
        matches!(wrong, Err(Error::NoModLoader { .. })),
        "{kind:?}: {wrong:?}"
      );
      assert!(
        !matches!(right, Err(Error::NoModLoader { .. })),
        "{kind:?}: {right:?}"
      );
    }
  }

  #[test]
  fn a_vanilla_launch_needs_no_loader() {
    for kind in LOADERS {
      let plan = prepare(
        &LaunchFacts::default(),
        &valheim_with(kind),
        LaunchMode::Vanilla,
      );

      assert!(plan.is_ok(), "{kind:?}: {plan:?}");
    }
  }

  #[test]
  fn a_game_without_a_known_loader_launches_modded_with_none_installed() {
    for kind in [LoaderKind::None, LoaderKind::Other] {
      let plan = prepare(
        &LaunchFacts::default(),
        &valheim_with(kind),
        LaunchMode::Modded,
      );

      assert!(plan.is_ok(), "{kind:?}: {plan:?}");
    }
  }

  #[test]
  fn macos_is_refused_before_the_loader_is_checked() {
    let mac = LaunchContext::new(
      HostOs::MacOs,
      StorePlatform::SteamDirect,
      Runtime::Native,
      LaunchMode::Modded,
    );

    let err = prepare_plan_from(
      Path::new(TARGET),
      &LaunchFacts::default(),
      &Ecosystem::bundled(),
      "valheim",
      &mac,
    )
    .unwrap_err();

    assert!(err.to_string().contains("macOS launch is not supported"));
  }
}

#[cfg(all(test, feature = "io"))]
mod io_tests {
  use super::*;
  use crate::profile::launch::tests::assert_options_name_script;
  use crate::profile::launch::{Runtime, StorePlatform};
  use crate::profile::layout;
  use crate::profile::target::InstallTarget;
  use std::path::PathBuf;
  use tempfile::TempDir;

  /// A `Main` profile holding BepInEx, beside a Valheim game folder.
  struct Fixture {
    _dir: TempDir,
    base: PathBuf,
    game_dir: PathBuf,
    profile_dir: PathBuf,
  }

  impl Fixture {
    fn new() -> Self {
      let dir = tempfile::tempdir().unwrap();
      let base = dir.path().join("base");
      let game_dir = dir.path().join("game");
      let profile_dir = layout::create(&base, "valheim", "Main").unwrap();
      let exe = Ecosystem::bundled()
        .require_profile("valheim")
        .unwrap()
        .exe_names[0]
        .clone();

      std::fs::create_dir_all(profile_dir.join("BepInEx/core")).unwrap();
      std::fs::write(profile_dir.join("BepInEx/core/BepInEx.Preloader.dll"), b"x").unwrap();
      std::fs::write(profile_dir.join("winhttp.dll"), b"proxy").unwrap();
      std::fs::create_dir_all(&game_dir).unwrap();
      std::fs::write(game_dir.join(exe), b"game").unwrap();

      Self {
        _dir: dir,
        base,
        game_dir,
        profile_dir,
      }
    }

    fn target(&self) -> InstallTarget {
      InstallTarget::resolve(
        self.base.clone(),
        self.game_dir.clone(),
        "valheim",
        Some("Main"),
      )
      .unwrap()
    }
  }

  fn context(os: HostOs, store: StorePlatform, mode: LaunchMode) -> LaunchContext {
    LaunchContext::new(os, store, Runtime::Native, mode)
  }

  #[test]
  fn a_missing_game_folder_is_refused() {
    let fixture = Fixture::new();
    let target = fixture.target();

    std::fs::remove_dir_all(&fixture.game_dir).unwrap();

    let err = prepare_launch(
      &target,
      &Ecosystem::bundled(),
      &context(
        HostOs::Windows,
        StorePlatform::SteamDirect,
        LaunchMode::Modded,
      ),
    )
    .unwrap_err();

    assert!(
      matches!(&err, Error::MissingGameDir { path } if *path == fixture.game_dir),
      "{err:?}"
    );
  }

  #[test]
  fn a_windows_launch_stages_the_proxy_and_returns_the_game_command() {
    let fixture = Fixture::new();
    let exe = Ecosystem::bundled()
      .require_profile("valheim")
      .unwrap()
      .exe_names[0]
      .clone();

    let prepared = prepare_launch(
      &fixture.target(),
      &Ecosystem::bundled(),
      &context(
        HostOs::Windows,
        StorePlatform::SteamDirect,
        LaunchMode::Modded,
      ),
    )
    .unwrap();
    let args: Vec<_> = prepared.command.get_args().collect();

    assert_eq!(prepared.command.get_program(), fixture.game_dir.join(exe));
    assert_eq!(args[0], "--doorstop-enable");
    assert_eq!(prepared.staged, vec![fixture.game_dir.join("winhttp.dll")]);
    assert_eq!(
      std::fs::read(fixture.game_dir.join("winhttp.dll")).unwrap(),
      b"proxy"
    );
    assert!(prepared.notices.is_empty());
  }

  #[test]
  #[cfg(unix)]
  fn a_linux_native_launch_makes_the_loader_script_executable_and_runs_it() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let script = fixture.profile_dir.join("start_game_bepinex.sh");

    std::fs::write(&script, b"#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();

    let prepared = prepare_launch(
      &fixture.target(),
      &Ecosystem::bundled(),
      &context(
        HostOs::Linux,
        StorePlatform::SteamDirect,
        LaunchMode::Modded,
      ),
    )
    .unwrap();
    let mode = std::fs::metadata(&script).unwrap().permissions().mode();

    assert_eq!(prepared.command.get_program(), script);
    assert_eq!(mode & 0o777, 0o755);
    assert_eq!(
      prepared.notices,
      vec![LaunchNotice::MadeExecutable {
        path: script.clone()
      }]
    );
    assert!(prepared.staged.is_empty());
    assert!(!fixture.game_dir.join("winhttp.dll").exists());
  }

  #[test]
  fn a_linux_native_steam_launch_writes_the_wrapper_and_gives_the_line_to_paste() {
    let fixture = Fixture::new();

    std::fs::write(fixture.profile_dir.join("run_bepinex.sh"), b"#!/bin/sh\n").unwrap();

    let prepared = prepare_launch(
      &fixture.target(),
      &Ecosystem::bundled(),
      &context(HostOs::Linux, StorePlatform::Steam, LaunchMode::Modded),
    )
    .unwrap();
    let script = fixture.base.join("valheim").join("steam-wrapper.sh");

    match &prepared.notices[0] {
      LaunchNotice::SteamWrapper {
        script: written,
        launch_options,
      } => {
        assert_eq!(written, &script);
        assert_options_name_script(launch_options, &script);
      }
      other => panic!("expected the Steam wrapper notice, got {other:?}"),
    }

    assert!(script.is_file());
    assert!(prepared.command.get_args().any(|arg| arg == "-applaunch"));
  }

  #[test]
  fn a_profile_with_no_loader_is_refused_before_the_game_folder_is_touched() {
    let fixture = Fixture::new();

    std::fs::remove_dir_all(fixture.profile_dir.join("BepInEx")).unwrap();

    let err = prepare_launch(
      &fixture.target(),
      &Ecosystem::bundled(),
      &context(
        HostOs::Windows,
        StorePlatform::SteamDirect,
        LaunchMode::Modded,
      ),
    )
    .unwrap_err();

    assert!(
      matches!(
        err,
        Error::NoModLoader {
          loader: LoaderKind::BepInEx
        }
      ),
      "{err:?}"
    );
    assert!(!fixture.game_dir.join("winhttp.dll").exists());
  }

  #[test]
  fn a_missing_game_executable_is_refused_before_the_game_folder_is_touched() {
    let fixture = Fixture::new();
    let exe = Ecosystem::bundled()
      .require_profile("valheim")
      .unwrap()
      .exe_names[0]
      .clone();

    std::fs::remove_file(fixture.game_dir.join(exe)).unwrap();

    let result = prepare_launch(
      &fixture.target(),
      &Ecosystem::bundled(),
      &context(
        HostOs::Windows,
        StorePlatform::SteamDirect,
        LaunchMode::Modded,
      ),
    );

    assert!(result.is_err());
    assert!(!fixture.game_dir.join("winhttp.dll").exists());
  }

  #[test]
  fn a_launch_from_the_game_folder_itself_stages_nothing() {
    let fixture = Fixture::new();
    let core = fixture.game_dir.join("BepInEx/core");

    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(core.join("BepInEx.Preloader.dll"), b"x").unwrap();
    std::fs::write(fixture.game_dir.join("winhttp.dll"), b"proxy").unwrap();

    let target = InstallTarget::resolve(
      fixture.base.clone(),
      fixture.game_dir.clone(),
      "valheim",
      None,
    )
    .unwrap();

    let prepared = prepare_launch(
      &target,
      &Ecosystem::bundled(),
      &context(
        HostOs::Windows,
        StorePlatform::SteamDirect,
        LaunchMode::Modded,
      ),
    )
    .unwrap();

    assert!(prepared.staged.is_empty());
  }

  #[test]
  fn a_shimloader_launch_from_the_game_folder_still_stages_its_files() {
    let eco = Ecosystem::bundled();
    let dir = tempfile::tempdir().unwrap();
    let game_dir = dir.path().join("game");
    let exe = eco.require_profile("palworld").unwrap().exe_names[0].clone();

    std::fs::create_dir_all(&game_dir).unwrap();
    std::fs::write(game_dir.join(exe), b"game").unwrap();
    std::fs::write(game_dir.join("ue4ss.dll"), b"loader").unwrap();
    std::fs::write(game_dir.join("dwmapi.dll"), b"proxy").unwrap();

    let target =
      InstallTarget::resolve(dir.path().join("base"), game_dir.clone(), "palworld", None).unwrap();
    let prepared = prepare_launch(
      &target,
      &eco,
      &context(
        HostOs::Windows,
        StorePlatform::SteamDirect,
        LaunchMode::Modded,
      ),
    )
    .unwrap();
    let binaries = game_dir.join("Pal/Binaries/Win64");

    assert_eq!(
      prepared.staged,
      vec![binaries.join("dwmapi.dll"), binaries.join("ue4ss.dll")]
    );
    assert_eq!(
      std::fs::read(binaries.join("dwmapi.dll")).unwrap(),
      b"proxy"
    );
  }

  #[test]
  fn a_proton_steam_launch_gives_the_dll_override_to_paste() {
    let fixture = Fixture::new();
    let proton =
      |mode| LaunchContext::new(HostOs::Linux, StorePlatform::Steam, Runtime::Proton, mode);

    let modded = prepare_launch(
      &fixture.target(),
      &Ecosystem::bundled(),
      &proton(LaunchMode::Modded),
    )
    .unwrap();
    let vanilla = prepare_launch(
      &fixture.target(),
      &Ecosystem::bundled(),
      &proton(LaunchMode::Vanilla),
    )
    .unwrap();

    assert_eq!(
      modded.notices,
      vec![LaunchNotice::ProtonDllOverride {
        dll: "winhttp".to_string(),
        launch_options: "WINEDLLOVERRIDES=\"winhttp=n,b\" %command%".to_string(),
      }]
    );
    assert!(vanilla.notices.is_empty());
  }

  #[test]
  #[cfg(unix)]
  fn staging_that_fails_partway_removes_the_files_it_created() {
    let fixture = Fixture::new();
    let elsewhere = fixture.game_dir.join("elsewhere.dll");

    std::fs::write(fixture.profile_dir.join("a.ini"), b"config").unwrap();
    std::os::unix::fs::symlink(&elsewhere, fixture.game_dir.join("winhttp.dll")).unwrap();

    let result = prepare_launch(
      &fixture.target(),
      &Ecosystem::bundled(),
      &context(
        HostOs::Windows,
        StorePlatform::SteamDirect,
        LaunchMode::Modded,
      ),
    );

    assert!(result.is_err());
    assert!(!fixture.game_dir.join("a.ini").exists());
    assert!(!elsewhere.exists());
  }

  #[test]
  fn a_vanilla_launch_copies_nothing_into_the_game_folder() {
    let fixture = Fixture::new();

    let prepared = prepare_launch(
      &fixture.target(),
      &Ecosystem::bundled(),
      &context(
        HostOs::Windows,
        StorePlatform::SteamDirect,
        LaunchMode::Vanilla,
      ),
    )
    .unwrap();

    assert!(prepared.staged.is_empty());
    assert!(!fixture.game_dir.join("winhttp.dll").exists());
  }

  #[test]
  fn a_file_already_in_the_game_folder_is_reported_as_replaced_not_staged() {
    let fixture = Fixture::new();
    let own = fixture.game_dir.join("winhttp.dll");

    std::fs::write(&own, b"the player's own").unwrap();

    let prepared = prepare_launch(
      &fixture.target(),
      &Ecosystem::bundled(),
      &context(
        HostOs::Windows,
        StorePlatform::SteamDirect,
        LaunchMode::Modded,
      ),
    )
    .unwrap();

    assert!(prepared.staged.is_empty());
    assert_eq!(prepared.replaced, vec![own.clone()]);
    assert_eq!(std::fs::read(&own).unwrap(), b"proxy");
  }
}
