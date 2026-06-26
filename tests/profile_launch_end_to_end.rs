//! Install a mod into a profile, then compute a launch plan and stage the loader.

use tempfile::tempdir;
use thunderstore_engine::ecosystem::{Ecosystem, LoaderKind};
use thunderstore_engine::profile;
use thunderstore_engine::profile::launch::{
  HostOs, LaunchContext, LaunchMode, LaunchProgram, Runtime, StorePlatform,
};

#[test]
fn profile_to_launch_plan_and_staging() {
  let base = tempdir().unwrap();
  let game_dir = tempdir().unwrap();
  let eco = Ecosystem::bundled();

  // A profile that looks like a real BepInEx install.
  let profile_dir = profile::layout::create(base.path(), "valheim", "Main").unwrap();
  let core = profile_dir.join("BepInEx/core");

  std::fs::create_dir_all(&core).unwrap();
  std::fs::write(core.join("BepInEx.Preloader.dll"), b"preloader").unwrap();
  std::fs::write(profile_dir.join("winhttp.dll"), b"proxy").unwrap();
  std::fs::write(profile_dir.join("doorstop_config.ini"), b"[UnityDoorstop]").unwrap();

  // `LaunchContext` is `#[non_exhaustive]`, so a struct literal is not allowed
  // here (this test file compiles as a separate crate) — build it through the
  // constructor and builder instead.
  let ctx = LaunchContext::new(
    HostOs::Windows,
    StorePlatform::Steam,
    Runtime::Native,
    LaunchMode::Modded,
  )
  .with_extra_args(vec!["-window-mode".to_string(), "exclusive".to_string()]);

  // 1. Compute the plan against the layout profile.
  let plan = profile::launch::launch_plan(base.path(), "valheim", "Main", &eco, &ctx).unwrap();

  assert!(matches!(plan.program, LaunchProgram::Steam { .. }));
  assert_eq!(plan.args[0], "--doorstop-enable");
  assert!(
    plan
      .args
      .iter()
      .any(|a| a.ends_with("BepInEx.Preloader.dll"))
  );
  assert_eq!(plan.args.last().unwrap(), "exclusive");
  // Argv is unquoted: the caller spawns without a shell.
  assert!(plan.args.iter().all(|a| !a.contains('"')));

  // 2. Stage the loader files into the game directory.
  let written =
    profile::launch::stage_for_launch(base.path(), "valheim", "Main", &eco, game_dir.path(), &ctx)
      .unwrap();

  assert!(game_dir.path().join("winhttp.dll").exists());
  assert!(game_dir.path().join("doorstop_config.ini").exists());
  assert!(!game_dir.path().join("mods.yml").exists());
  assert_eq!(written.len(), 2);

  // 3. A vanilla plan disables the loader instead. `LaunchContext` is
  // `#[non_exhaustive]`, so `..ctx` functional-update syntax is also unavailable
  // here — build the vanilla context fresh through the constructor instead.
  let vanilla_ctx = LaunchContext::new(ctx.os, ctx.store, ctx.runtime, LaunchMode::Vanilla);
  let vanilla =
    profile::launch::launch_plan(base.path(), "valheim", "Main", &eco, &vanilla_ctx).unwrap();

  assert_eq!(
    vanilla.args,
    vec!["--doorstop-enable".to_string(), "false".to_string()]
  );
}

/// A Linux native Steam launch is the one combination a plan cannot express on its
/// own. This exercises the whole route from a downstream crate's point of view: the
/// requirement is discoverable on the plan, and it carries everything the two
/// wrapper functions need — no reimplementing the platform rule, no second
/// ecosystem lookup.
#[test]
fn linux_native_steam_plan_hands_the_caller_the_wrapper_it_needs() {
  let base = tempdir().unwrap();
  let game_dir = tempdir().unwrap();
  let eco = Ecosystem::bundled();

  let profile_dir = profile::layout::create(base.path(), "valheim", "Main").unwrap();
  let core = profile_dir.join("BepInEx/core");

  std::fs::create_dir_all(&core).unwrap();
  std::fs::write(core.join("BepInEx.Preloader.dll"), b"preloader").unwrap();
  std::fs::write(profile_dir.join("winhttp.dll"), b"proxy").unwrap();
  // The loader package ships its own launcher script for a Linux native install.
  std::fs::write(profile_dir.join("run_bepinex.sh"), b"#!/bin/sh\n").unwrap();

  let ctx = LaunchContext::new(
    HostOs::Linux,
    StorePlatform::Steam,
    Runtime::Native,
    LaunchMode::Modded,
  );

  let plan = profile::launch::launch_plan(base.path(), "valheim", "Main", &eco, &ctx).unwrap();

  // Steam takes `-applaunch`, so the script cannot be the program here.
  assert!(matches!(plan.program, LaunchProgram::Steam { .. }));

  let wrapper = plan
    .steam_wrapper
    .clone()
    .expect("linux native Steam needs a wrapper script");

  assert_eq!(wrapper.target_dir, profile_dir);
  assert_eq!(wrapper.loader, LoaderKind::BepInEx);

  let script_path = base.path().join("launch_wrapper.sh");

  profile::launch::write_wrapper_script(&script_path).unwrap();

  let options = profile::launch::steam_launch_options(&script_path);
  let contents = std::fs::read_to_string(&script_path).unwrap();

  assert!(options.ends_with("%command%"));
  assert!(options.contains(&script_path.display().to_string()));
  assert!(contents.contains("run_bepinex.sh"));
  // The script is profile-agnostic; the profile reaches it through the plan's args.
  assert!(!contents.contains(&profile_dir.display().to_string()));
  assert!(
    plan
      .args
      .windows(2)
      .any(|pair| pair == profile::launch::wrapper_target_args(&profile_dir).as_slice())
  );

  // Injection is script-based, so nothing is copied into the game directory.
  let written =
    profile::launch::stage_for_launch(base.path(), "valheim", "Main", &eco, game_dir.path(), &ctx)
      .unwrap();

  assert!(written.is_empty());
  assert!(!game_dir.path().join("winhttp.dll").exists());
}

/// The promise the wrapper exists to keep: Steam stores one launch-options string
/// per game, so the script it names is written **once** and must then launch
/// whichever profile the manager asks for — without being rewritten, and without
/// the pasted string ever going stale.
///
/// This runs the real script against two profiles in turn, which is the only way
/// to observe that promise end to end; a stale-path bug looks perfectly correct in
/// the plan and only surfaces when the script actually resolves a target.
#[cfg(unix)]
#[test]
fn one_wrapper_written_once_launches_whichever_profile_the_plan_names() {
  use std::os::unix::fs::PermissionsExt;

  let base = tempdir().unwrap();
  let eco = Ecosystem::bundled();
  let log = base.path().join("argv.log");

  // A loader script that records the argv it received, so we can tell which
  // profile actually got chainloaded.
  let install_profile = |name: &str| -> std::path::PathBuf {
    let profile_dir = profile::layout::create(base.path(), "valheim", name).unwrap();
    let core = profile_dir.join("BepInEx/core");

    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(core.join("BepInEx.Preloader.dll"), b"preloader").unwrap();

    let script = profile_dir.join("run_bepinex.sh");

    std::fs::write(
      &script,
      format!(
        "#!/bin/sh\nprintf '%s\\n' \"$0\" > \"{log}\"\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> \"{log}\"; done\n",
        log = log.display()
      ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    profile_dir
  };

  let main = install_profile("Main");
  let modded = install_profile("Heavily Modded");

  let ctx = LaunchContext::new(
    HostOs::Linux,
    StorePlatform::Steam,
    Runtime::Native,
    LaunchMode::Modded,
  );

  // Written exactly once, naming no profile at all, and never touched again below.
  let wrapper = base.path().join("steam-wrapper.sh");
  let first = profile::launch::launch_plan(base.path(), "valheim", "Main", &eco, &ctx).unwrap();

  first.steam_wrapper.as_ref().expect("wrapper requirement");

  profile::launch::write_wrapper_script(&wrapper).unwrap();

  let pasted_into_steam = profile::launch::steam_launch_options(&wrapper);

  // Each profile in turn: compute a plan, spawn the *same* wrapper with that
  // plan's args, and confirm the profile's own loader script ran.
  for (name, profile_dir) in [("Main", &main), ("Heavily Modded", &modded)] {
    let plan = profile::launch::launch_plan(base.path(), "valheim", name, &eco, &ctx).unwrap();

    // Steam is handed `-applaunch <id> <args>`; the args reach the wrapper's "$@"
    // after `%command%`, which is the game binary Steam substitutes in.
    let output = std::process::Command::new(&wrapper)
      .arg("/games/valheim/valheim.x86_64")
      .args(&plan.args)
      .output()
      .unwrap();

    assert!(
      output.status.success(),
      "wrapper failed for {name}: {}",
      String::from_utf8_lossy(&output.stderr)
    );

    let lines: Vec<String> = std::fs::read_to_string(&log)
      .unwrap()
      .lines()
      .map(str::to_string)
      .collect();

    assert_eq!(
      lines[0],
      profile_dir.join("run_bepinex.sh").to_string_lossy(),
      "wrapper chainloaded the wrong profile for {name}"
    );

    // The loader script sees the game binary first, then the Doorstop arguments —
    // and never the wrapper's own target flag.
    assert_eq!(lines[1], "/games/valheim/valheim.x86_64");
    assert!(lines.contains(&"--doorstop-enabled".to_string()));
    assert!(
      !lines.contains(&profile::launch::WRAPPER_TARGET_FLAG.to_string()),
      "the target flag leaked into the game's argv for {name}"
    );
  }

  // The string the user pasted is still the one that is correct, unchanged, after
  // launching two different profiles.
  assert_eq!(
    pasted_into_steam,
    profile::launch::steam_launch_options(&wrapper)
  );
  assert_eq!(
    std::fs::read_to_string(&wrapper).unwrap(),
    profile::launch::wrapper_script_contents(),
    "the wrapper must never need rewriting"
  );

  // And it mentions neither profile, so "launch from Steam" can only ever mean an
  // unmodded launch — never whichever profile happened to be current.
  let contents = std::fs::read_to_string(&wrapper).unwrap();

  assert!(!contents.contains(&main.display().to_string()));
  assert!(!contents.contains(&modded.display().to_string()));
}
