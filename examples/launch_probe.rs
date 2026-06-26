//! Manual verification tool for `profile::launch` against a real install.
//!
//! Prints the computed launch plan for a target directory. Read-only by
//! default: it never writes anything and never starts a process unless you
//! ask it to.
//!
//! ```text
//! cargo run --example launch_probe -- --target <PROFILE_DIR> [options]
//!
//!   --target <DIR>     profile / target directory (required)
//!   --game <SLUG>      ecosystem game slug          [default: valheim]
//!   --os <OS>          windows | linux | macos      [default: linux]
//!   --store <STORE>    steam | steam-direct | other [default: steam-direct]
//!   --runtime <RT>     native | proton              [default: native]
//!   --mode <MODE>      modded | vanilla             [default: modded]
//!   --game-dir <DIR>   game directory, for --stage
//!   --stage            WRITES loader files into --game-dir
//!   --wrapper <FILE>   WRITES the Steam wrapper script to FILE
//! ```
//!
//! The wrapper is written once per game and never regenerated. It names no
//! profile: it reads its target from the plan's arguments at run time, and a bare
//! Steam launch runs the game unmodded.

use std::path::{Path, PathBuf};
use thunderstore_engine::ecosystem::Ecosystem;
use thunderstore_engine::profile::launch::{
  HostOs, LaunchContext, LaunchMode, LaunchProgram, Runtime, StorePlatform, launch_plan_in,
  stage_for_launch_in, steam_launch_options, wrapper_target_args, write_wrapper_script,
};

fn main() {
  let args: Vec<String> = std::env::args().skip(1).collect();

  if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
    eprintln!("{}", USAGE);
    std::process::exit(if args.is_empty() { 2 } else { 0 });
  }

  let opt = |name: &str| -> Option<String> {
    args
      .iter()
      .position(|a| a == name)
      .and_then(|i| args.get(i + 1))
      .cloned()
  };
  let flag = |name: &str| args.iter().any(|a| a == name);

  let Some(target) = opt("--target").map(PathBuf::from) else {
    eprintln!("error: --target is required\n\n{USAGE}");
    std::process::exit(2);
  };

  if !target.is_dir() {
    eprintln!("error: --target is not a directory: {}", target.display());
    std::process::exit(2);
  }

  let game = opt("--game").unwrap_or_else(|| "valheim".to_string());

  let os = match opt("--os").as_deref().unwrap_or("linux") {
    "windows" => HostOs::Windows,
    "linux" => HostOs::Linux,
    "macos" => HostOs::MacOs,
    other => fail("--os", other, "windows | linux | macos"),
  };

  let store = match opt("--store").as_deref().unwrap_or("steam-direct") {
    "steam" => StorePlatform::Steam,
    "steam-direct" => StorePlatform::SteamDirect,
    "other" => StorePlatform::Other,
    other => fail("--store", other, "steam | steam-direct | other"),
  };

  let runtime = match opt("--runtime").as_deref().unwrap_or("native") {
    "native" => Runtime::Native,
    "proton" => Runtime::Proton,
    other => fail("--runtime", other, "native | proton"),
  };

  let mode = match opt("--mode").as_deref().unwrap_or("modded") {
    "modded" => LaunchMode::Modded,
    "vanilla" => LaunchMode::Vanilla,
    other => fail("--mode", other, "modded | vanilla"),
  };

  let eco = Ecosystem::bundled();
  let ctx = LaunchContext::new(os, store, runtime, mode);

  println!("target : {}", target.display());
  println!("game   : {game}");
  println!("context: {os:?} / {store:?} / {runtime:?} / {mode:?}");
  println!();

  report_inputs(&target);

  let plan = match launch_plan_in(&target, &eco, &game, &ctx) {
    Ok(plan) => plan,
    Err(e) => {
      println!("launch_plan_in ERROR:\n  {e}");
      std::process::exit(1);
    }
  };

  println!("== plan ==");

  match &plan.program {
    LaunchProgram::Steam { steam_app_id } => println!("program  : Steam -applaunch {steam_app_id}"),
    LaunchProgram::GameExe { exe_names } => println!("program  : GameExe {exe_names:?}"),
    LaunchProgram::ProfileScript { path, exe_names } => {
      println!("program  : ProfileScript {}", path.display());
      println!("           exe_names {exe_names:?}");
    }
    other => println!("program  : {other:?}"),
  }

  println!("args     : {:?}", plan.args);
  println!("env      : {:?}", plan.env);
  println!("wrapper  : {:?}", plan.steam_wrapper);
  println!();
  println!("shell-equivalent (for eyeballing only, NOT how it is spawned):");
  println!("  {}", shell_preview(&plan.program, &plan.args, &plan.env));
  println!();

  if let Some(w) = &plan.steam_wrapper {
    println!("This combination needs a Steam wrapper script.");
    println!("  write it once with --wrapper <FILE>, then paste into launch options:");
    println!("  {}", steam_launch_options(Path::new("<FILE>")));
    println!(
      "  the target reaches it at run time via {:?}, already in `args` above",
      wrapper_target_args(&w.target_dir)
    );
    println!();
  }

  if let Some(script) = opt("--wrapper") {
    let script = PathBuf::from(script);

    match write_wrapper_script(&script) {
      Ok(()) => {
        println!("wrote wrapper: {}", script.display());
        println!("  names no profile; a bare Steam launch runs the game unmodded");
        println!(
          "steam launch options (paste once; this script never needs rewriting):\n  {}",
          steam_launch_options(&script)
        );
      }
      Err(e) => println!("write_wrapper_script ERROR: {e}"),
    }

    println!();
  }

  if flag("--stage") {
    let Some(game_dir) = opt("--game-dir").map(PathBuf::from) else {
      eprintln!("error: --stage requires --game-dir");
      std::process::exit(2);
    };

    println!("== staging into {} ==", game_dir.display());

    match stage_for_launch_in(&target, &eco, &game, &game_dir, &ctx) {
      Ok(written) if written.is_empty() => {
        println!("nothing staged (expected for a Linux native script launch)")
      }
      Ok(written) => {
        for path in &written {
          println!("  wrote {}", path.display());
        }
      }
      Err(e) => println!("stage_for_launch_in ERROR: {e}"),
    }
  } else {
    println!("(dry run — pass --stage --game-dir <DIR> to actually copy loader files)");
  }
}

/// Prints the on-disk facts the plan is derived from, so a surprising plan can
/// be traced back to its inputs without re-reading the module.
fn report_inputs(target: &Path) {
  let version = std::fs::read_to_string(target.join(".doorstop_version"))
    .map(|s| s.trim().to_string())
    .unwrap_or_else(|_| "(absent — treated as v3)".to_string());

  println!("== inputs ==");
  println!(".doorstop_version : {version}");

  let core = target.join("BepInEx").join("core");
  let preloaders: Vec<String> = std::fs::read_dir(&core)
    .map(|entries| {
      entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("BepInEx") && n.ends_with(".dll"))
        .collect()
    })
    .unwrap_or_default();

  println!("BepInEx/core      : {preloaders:?}");
  println!(
    "unstripped_corlib : {}",
    target.join("unstripped_corlib").is_dir()
  );

  let scripts: Vec<&str> = [
    "start_server_bepinex.sh",
    "run_bepinex.sh",
    "start_game_bepinex.sh",
  ]
  .into_iter()
  .filter(|n| target.join(n).is_file())
  .collect();

  println!("loader scripts    : {scripts:?}");
  println!();
}

/// A copy-pasteable approximation of the spawn, for human inspection only. The
/// real plan is an argv vector handed to `Command::args` with no shell involved.
fn shell_preview(program: &LaunchProgram, args: &[String], env: &[(String, String)]) -> String {
  let mut out = String::new();

  for (k, v) in env {
    out.push_str(&format!("{k}=\"{v}\" "));
  }

  match program {
    LaunchProgram::Steam { steam_app_id } => {
      out.push_str(&format!("steam -applaunch {steam_app_id}"))
    }
    LaunchProgram::GameExe { exe_names } => out.push_str(&format!(
      "<game-dir>/{}",
      exe_names.first().map_or("?", |s| s)
    )),
    LaunchProgram::ProfileScript { path, exe_names } => out.push_str(&format!(
      "{} <game-dir>/{}",
      path.display(),
      exe_names.first().map_or("?", |s| s)
    )),
    other => out.push_str(&format!("{other:?}")),
  }

  for a in args {
    out.push(' ');
    out.push_str(a);
  }

  out
}

fn fail(name: &str, got: &str, expected: &str) -> ! {
  eprintln!("error: bad value for {name}: {got:?} (expected {expected})");
  std::process::exit(2);
}

const USAGE: &str = "\
usage: cargo run --example launch_probe -- --target <PROFILE_DIR> [options]

  --target <DIR>     profile / target directory (required)
  --game <SLUG>      ecosystem game slug          [default: valheim]
  --os <OS>          windows | linux | macos      [default: linux]
  --store <STORE>    steam | steam-direct | other [default: steam-direct]
  --runtime <RT>     native | proton              [default: native]
  --mode <MODE>      modded | vanilla             [default: modded]
  --game-dir <DIR>   game directory, for --stage
  --stage            WRITES loader files into --game-dir
  --wrapper <FILE>   WRITES the Steam wrapper script to FILE

The wrapper is written once per game and never regenerated. It names no profile:
it reads its target from the plan's arguments at run time, and a bare Steam
launch runs the game unmodded.

Read-only unless --stage or --wrapper is passed. Never starts a process.";
