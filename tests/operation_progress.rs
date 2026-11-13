//! Per-operation progress, cancellation, and enabled-state reporting for an
//! install batch, offline against the published mock Thunderstore.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::{TempDir, tempdir};
use thunderstore_engine::client::ThunderstoreClient;
use thunderstore_engine::ecosystem::Ecosystem;
use thunderstore_engine::models::PackageIndex;
use thunderstore_engine::profile::{self, InstallBatch, modlist};
use thunderstore_engine::progress::{Operation, OperationId, ProgressEvent, Stage, StageUpdate};
use thunderstore_engine::test_support::MockThunderstore;
use tokio::runtime::Runtime;

struct Fixture {
  _mock: MockThunderstore,
  _cache: TempDir,
  target: TempDir,
  client: ThunderstoreClient,
  index: PackageIndex,
  eco: Ecosystem,
}

impl Fixture {
  fn new(rt: &Runtime) -> Self {
    let mock = MockThunderstore::start();
    let cache = tempdir().unwrap();
    let target = tempdir().unwrap();
    let client = mock
      .client_builder()
      .cache_dir(cache.path())
      .build()
      .unwrap();
    let index = rt.block_on(client.get_manifest()).unwrap();

    Self {
      _mock: mock,
      _cache: cache,
      target,
      client,
      index,
      eco: Ecosystem::bundled(),
    }
  }

  fn dir(&self) -> &Path {
    self.target.path()
  }

  fn cache_base(&self) -> std::path::PathBuf {
    self.dir().join(".cache")
  }

  fn run(
    &self,
    rt: &Runtime,
    batch: &InstallBatch,
    operation: &Operation,
  ) -> profile::BatchOutcome {
    rt.block_on(profile::install_batch_with(
      self.dir(),
      &self.cache_base(),
      &self.eco,
      &self.index,
      &self.client,
      "valheim",
      batch,
      1,
      operation,
    ))
    .unwrap()
  }

  fn enabled(&self, full_name: &str) -> bool {
    let recorded = modlist::read(self.dir()).unwrap();

    modlist::find(&recorded, full_name).unwrap().enabled
  }
}

fn recording(id: u64) -> (Operation, Arc<Mutex<Vec<ProgressEvent>>>) {
  let events = Arc::new(Mutex::new(Vec::new()));
  let sink = events.clone();
  let operation = Operation::new(OperationId(id))
    .with_progress(move |event: ProgressEvent| sink.lock().unwrap().push(event));

  (operation, events)
}

fn names(list: &[&str]) -> Vec<String> {
  list.iter().map(|name| name.to_string()).collect()
}

/// An operation that records its events and cancels itself once the first
/// `stage` finishes.
fn cancelling_after_first(stage: Stage) -> (Operation, Arc<Mutex<Vec<ProgressEvent>>>) {
  let events = Arc::new(Mutex::new(Vec::new()));
  let sink = events.clone();
  let operation = Operation::new(OperationId(3));
  let cancel = operation.cancel_token();
  let operation = operation.with_progress(move |event: ProgressEvent| {
    if event.stage == stage && event.update == StageUpdate::Finished {
      cancel.cancel();
    }

    sink.lock().unwrap().push(event);
  });

  (operation, events)
}

/// Each event's package, stage, and update.
fn summary(events: &Mutex<Vec<ProgressEvent>>) -> Vec<(String, Stage, StageUpdate)> {
  events
    .lock()
    .unwrap()
    .iter()
    .map(|event| (event.package.clone(), event.stage, event.update))
    .collect()
}

fn recorded_names(dir: &Path) -> Vec<String> {
  modlist::read(dir)
    .unwrap()
    .into_iter()
    .map(|entry| entry.name)
    .collect()
}

#[test]
fn every_package_reports_each_stage_under_the_operation() {
  let rt = Runtime::new().unwrap();
  let fixture = Fixture::new(&rt);
  let (operation, events) = recording(7);
  let batch = InstallBatch::new(names(&["Owner-ModC"]), Vec::new());

  let outcome = fixture.run(&rt, &batch, &operation);

  assert!(outcome.is_ok(), "{:?}", outcome.failed);

  let events = events.lock().unwrap();

  assert!(events.iter().all(|event| event.operation == OperationId(7)));

  for package in ["Owner-ModA", "Owner-ModC"] {
    let stages: Vec<(Stage, StageUpdate)> = events
      .iter()
      .filter(|event| event.package == package)
      .filter(|event| !matches!(event.update, StageUpdate::Advanced { .. }))
      .map(|event| (event.stage, event.update))
      .collect();

    assert_eq!(
      stages,
      [
        (Stage::Downloading, StageUpdate::Started { total: None }),
        (Stage::Downloading, StageUpdate::Finished),
        (Stage::Extracting, StageUpdate::Started { total: Some(2) }),
        (Stage::Extracting, StageUpdate::Finished),
        (Stage::Installing, StageUpdate::Started { total: Some(2) }),
        (Stage::Installing, StageUpdate::Finished),
      ],
      "{package}"
    );

    for stage in [Stage::Extracting, Stage::Installing] {
      let last = events
        .iter()
        .filter(|event| event.package == package && event.stage == stage)
        .filter_map(|event| match event.update {
          StageUpdate::Advanced { done, total } => Some((done, total)),
          _ => None,
        })
        .next_back();

      assert_eq!(last, Some((2, Some(2))), "{package} {stage:?}");
    }
  }
}

/// Pauses the file work at the start of extraction and of installing until a
/// task on the same single-threaded runtime answers. Work done on the
/// runtime's own thread would starve that task, and the wait would time out.
#[test]
fn file_work_leaves_the_runtime_free_for_other_tasks() {
  let rt = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .unwrap();
  let fixture = Fixture::new(&rt);
  let (ping_tx, mut ping_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
  let (pong_tx, pong_rx) = std::sync::mpsc::channel::<()>();
  let pong_rx = Mutex::new(pong_rx);
  let answered = Arc::new(Mutex::new(Vec::new()));
  let record = answered.clone();
  let operation = Operation::new(OperationId(1)).with_progress(move |event: ProgressEvent| {
    let file_work = matches!(event.stage, Stage::Extracting | Stage::Installing);

    if file_work && matches!(event.update, StageUpdate::Started { .. }) {
      ping_tx.send(()).unwrap();

      let answer = pong_rx.lock().unwrap().recv_timeout(Duration::from_secs(5));

      record.lock().unwrap().push((event.stage, answer.is_ok()));
    }
  });

  rt.spawn(async move {
    while ping_rx.recv().await.is_some() {
      let _ = pong_tx.send(());
    }
  });

  let outcome = fixture.run(
    &rt,
    &InstallBatch::new(names(&["Owner-ModA"]), Vec::new()),
    &operation,
  );

  assert!(outcome.is_ok(), "{:?}", outcome.failed);
  assert_eq!(
    *answered.lock().unwrap(),
    [(Stage::Extracting, true), (Stage::Installing, true)]
  );
}

#[test]
fn cancel_stops_after_the_current_package_and_keeps_disabled_mods_disabled() {
  let rt = Runtime::new().unwrap();
  let fixture = Fixture::new(&rt);

  fixture.run(
    &rt,
    &InstallBatch::new(names(&["Owner-ModB"]), Vec::new()),
    &Operation::default(),
  );
  profile::set_enabled_in(fixture.dir(), &fixture.eco, "valheim", "Owner-ModB", false).unwrap();

  let desired = names(&["Owner-ModB", "Owner-ModD"]);
  let mut batch = profile::plan_install_batch(fixture.dir(), &desired, &[]).unwrap();

  batch.force = true;

  let operation = Operation::new(OperationId(1));
  let cancel = operation.cancel_token();
  let operation = operation.with_progress(move |event: ProgressEvent| {
    if event.package == "Owner-ModB"
      && event.stage == Stage::Installing
      && event.update == StageUpdate::Finished
    {
      cancel.cancel();
    }
  });

  let outcome = fixture.run(&rt, &batch, &operation);

  assert!(outcome.failed.is_empty(), "{:?}", outcome.failed);
  assert!(!outcome.is_ok(), "a cancelled batch did not finish");
  assert_eq!(outcome.succeeded, ["Owner-ModB"]);
  assert_eq!(outcome.cancelled, ["Owner-ModD"]);
  assert_eq!(outcome.kept_disabled, ["Owner-ModB"]);
  assert!(!fixture.enabled("Owner-ModB"));
  assert!(
    modlist::find(&modlist::read(fixture.dir()).unwrap(), "Owner-ModD").is_none(),
    "a cancelled package is not installed"
  );
}

#[test]
fn cancel_stops_after_a_dependency_and_before_the_mod_that_needs_it() {
  let rt = Runtime::new().unwrap();
  let fixture = Fixture::new(&rt);
  let operation = Operation::new(OperationId(1));
  let cancel = operation.cancel_token();
  let operation = operation.with_progress(move |event: ProgressEvent| {
    if event.stage == Stage::Installing && event.update == StageUpdate::Finished {
      cancel.cancel();
    }
  });

  let outcome = fixture.run(
    &rt,
    &InstallBatch::new(names(&["Owner-ModC"]), Vec::new()),
    &operation,
  );
  let recorded = modlist::read(fixture.dir()).unwrap();

  assert!(outcome.failed.is_empty(), "{:?}", outcome.failed);
  assert_eq!(
    outcome.succeeded,
    ["Owner-ModA"],
    "the dependency goes first"
  );
  assert_eq!(outcome.cancelled, ["Owner-ModC"]);
  assert!(modlist::find(&recorded, "Owner-ModA").is_some());
  assert!(modlist::find(&recorded, "Owner-ModC").is_none());
}

#[test]
fn cancel_inside_a_dependency_closure_still_restores_disabled_mods() {
  let rt = Runtime::new().unwrap();
  let fixture = Fixture::new(&rt);

  fixture.run(
    &rt,
    &InstallBatch::new(names(&["Owner-ModC"]), Vec::new()),
    &Operation::default(),
  );
  profile::set_enabled_in(fixture.dir(), &fixture.eco, "valheim", "Owner-ModA", false).unwrap();
  std::fs::remove_dir_all(fixture.dir().join("BepInEx/plugins/Owner-ModA")).unwrap();

  let desired = names(&["Owner-ModC"]);
  let mut batch = profile::plan_install_batch(fixture.dir(), &desired, &desired).unwrap();

  batch.force = true;

  let operation = Operation::new(OperationId(1));
  let cancel = operation.cancel_token();
  let operation = operation.with_progress(move |event: ProgressEvent| {
    if event.stage == Stage::Installing && event.update == StageUpdate::Finished {
      cancel.cancel();
    }
  });

  let outcome = fixture.run(&rt, &batch, &operation);

  assert!(outcome.failed.is_empty(), "{:?}", outcome.failed);
  assert_eq!(
    outcome.succeeded,
    ["Owner-ModA"],
    "the repair reinstalled it"
  );
  assert_eq!(outcome.cancelled, ["Owner-ModC"]);
  assert_eq!(outcome.kept_disabled, ["Owner-ModA"]);
  assert!(!fixture.enabled("Owner-ModA"));
}

#[test]
fn an_uninstall_reports_each_package_and_stops_after_the_current_one() {
  let rt = Runtime::new().unwrap();
  let fixture = Fixture::new(&rt);
  let all = names(&["Owner-ModA", "Owner-ModB", "Owner-ModD"]);

  fixture.run(
    &rt,
    &InstallBatch::new(all.clone(), Vec::new()),
    &Operation::default(),
  );

  let batch =
    profile::plan_uninstall_batch(fixture.dir(), &fixture.eco, "valheim", Some(&all)).unwrap();
  let (operation, events) = cancelling_after_first(Stage::Uninstalling);

  let outcome =
    profile::uninstall_batch_with(fixture.dir(), &fixture.eco, "valheim", &batch, &operation)
      .unwrap();

  assert!(outcome.failed.is_empty(), "{:?}", outcome.failed);
  assert_eq!(outcome.succeeded, ["Owner-ModA"]);
  assert_eq!(outcome.cancelled, ["Owner-ModB", "Owner-ModD"]);
  assert_eq!(recorded_names(fixture.dir()), ["Owner-ModB", "Owner-ModD"]);
  assert_eq!(
    summary(&events),
    [
      (
        "Owner-ModA".to_string(),
        Stage::Uninstalling,
        StageUpdate::Started { total: None }
      ),
      (
        "Owner-ModA".to_string(),
        Stage::Uninstalling,
        StageUpdate::Finished
      ),
    ]
  );
}

#[test]
fn a_cancelled_uninstall_keeps_confirmed_untracked_folders() {
  let rt = Runtime::new().unwrap();
  let fixture = Fixture::new(&rt);
  let all = names(&["Owner-ModA", "Owner-ModB"]);
  let stray = fixture.dir().join("BepInEx/plugins/Stray-Folder");

  fixture.run(
    &rt,
    &InstallBatch::new(all.clone(), Vec::new()),
    &Operation::default(),
  );
  std::fs::create_dir_all(&stray).unwrap();
  std::fs::write(stray.join("Stray.dll"), b"stray").unwrap();

  let mut batch =
    profile::plan_uninstall_batch(fixture.dir(), &fixture.eco, "valheim", Some(&all)).unwrap();

  batch.confirm_untracked();

  let untracked: Vec<String> = batch
    .untracked
    .iter()
    .map(|path| path.display().to_string())
    .collect();
  let (operation, _) = cancelling_after_first(Stage::Uninstalling);

  let outcome =
    profile::uninstall_batch_with(fixture.dir(), &fixture.eco, "valheim", &batch, &operation)
      .unwrap();

  assert_eq!(untracked.len(), 1, "{untracked:?}");
  assert_eq!(
    outcome.cancelled,
    ["Owner-ModB".to_string(), untracked[0].clone()]
  );
  assert!(stray.is_dir());
}

#[test]
fn a_toggle_reports_each_package_and_stops_after_the_current_one() {
  let rt = Runtime::new().unwrap();
  let fixture = Fixture::new(&rt);
  let all = names(&["Owner-ModA", "Owner-ModB", "Owner-ModD"]);

  fixture.run(
    &rt,
    &InstallBatch::new(all.clone(), Vec::new()),
    &Operation::default(),
  );

  let disable = profile::plan_set_enabled_batch(fixture.dir(), &fixture.eco, None, false).unwrap();
  let (operation, events) = cancelling_after_first(Stage::Disabling);

  let outcome =
    profile::set_enabled_batch_with(fixture.dir(), &fixture.eco, "valheim", &disable, &operation)
      .unwrap();

  assert!(outcome.failed.is_empty(), "{:?}", outcome.failed);
  assert_eq!(outcome.succeeded, ["Owner-ModA"]);
  assert_eq!(outcome.cancelled, ["Owner-ModB", "Owner-ModD"]);
  assert!(!fixture.enabled("Owner-ModA"));
  assert!(fixture.enabled("Owner-ModB"));
  assert!(fixture.enabled("Owner-ModD"));
  assert_eq!(
    summary(&events),
    [
      (
        "Owner-ModA".to_string(),
        Stage::Disabling,
        StageUpdate::Started { total: None }
      ),
      (
        "Owner-ModA".to_string(),
        Stage::Disabling,
        StageUpdate::Finished
      ),
    ]
  );

  let enable = profile::plan_set_enabled_batch(fixture.dir(), &fixture.eco, None, true).unwrap();
  let (operation, events) = recording(4);

  let outcome =
    profile::set_enabled_batch_with(fixture.dir(), &fixture.eco, "valheim", &enable, &operation)
      .unwrap();

  assert!(outcome.is_ok(), "{:?}", outcome.failed);
  assert_eq!(outcome.succeeded, ["Owner-ModA"]);
  assert!(fixture.enabled("Owner-ModA"));
  assert_eq!(
    summary(&events),
    [
      (
        "Owner-ModA".to_string(),
        Stage::Enabling,
        StageUpdate::Started { total: None }
      ),
      (
        "Owner-ModA".to_string(),
        Stage::Enabling,
        StageUpdate::Finished
      ),
    ]
  );
}

#[test]
fn the_outcome_names_mods_re_enabled_and_kept_disabled() {
  let rt = Runtime::new().unwrap();
  let fixture = Fixture::new(&rt);
  let all = names(&["Owner-ModA", "Owner-ModB", "Owner-ModD"]);

  fixture.run(
    &rt,
    &InstallBatch::new(all.clone(), Vec::new()),
    &Operation::default(),
  );

  for full_name in ["Owner-ModA", "Owner-ModB", "Owner-ModD"] {
    profile::set_enabled_in(fixture.dir(), &fixture.eco, "valheim", full_name, false).unwrap();
  }

  let named = names(&["Owner-ModA", "Owner-ModB"]);
  let mut batch =
    profile::plan_install_batch(fixture.dir(), &named, &names(&["Owner-ModA"])).unwrap();

  batch.force = true;

  let outcome = fixture.run(&rt, &batch, &Operation::default());

  assert!(outcome.is_ok(), "{:?}", outcome.failed);
  assert_eq!(outcome.re_enabled, ["Owner-ModA"]);
  assert_eq!(outcome.kept_disabled, ["Owner-ModB"]);
  assert!(fixture.enabled("Owner-ModA"));
  assert!(!fixture.enabled("Owner-ModB"));
  assert!(!fixture.enabled("Owner-ModD"));
}

#[test]
fn naming_a_current_disabled_mod_reports_it_re_enabled() {
  let rt = Runtime::new().unwrap();
  let fixture = Fixture::new(&rt);
  let named = names(&["Owner-ModA"]);

  fixture.run(
    &rt,
    &InstallBatch::new(named.clone(), Vec::new()),
    &Operation::default(),
  );
  profile::set_enabled_in(fixture.dir(), &fixture.eco, "valheim", "Owner-ModA", false).unwrap();

  let batch = profile::plan_install_batch(fixture.dir(), &named, &named).unwrap();
  let outcome = fixture.run(&rt, &batch, &Operation::default());

  assert_eq!(outcome.unchanged, ["Owner-ModA"]);
  assert_eq!(outcome.re_enabled, ["Owner-ModA"]);
  assert!(outcome.kept_disabled.is_empty());
}

#[test]
fn an_unreadable_mods_yml_before_the_batch_fails_each_item() {
  let rt = Runtime::new().unwrap();
  let fixture = Fixture::new(&rt);

  std::fs::write(fixture.dir().join("mods.yml"), "not: [a mod list").unwrap();

  let batch = InstallBatch::new(names(&["Owner-ModA", "Owner-ModB"]), Vec::new());
  let outcome = fixture.run(&rt, &batch, &Operation::default());
  let failed: Vec<&str> = outcome
    .failed
    .iter()
    .map(|(name, _)| name.as_str())
    .collect();

  assert_eq!(failed, ["Owner-ModA", "Owner-ModB"]);
  assert!(outcome.succeeded.is_empty());
  assert!(outcome.re_enabled.is_empty());
  assert!(outcome.kept_disabled.is_empty());
}

#[test]
fn an_unreadable_mods_yml_after_the_installs_keeps_the_outcome() {
  let rt = Runtime::new().unwrap();
  let fixture = Fixture::new(&rt);

  fixture.run(
    &rt,
    &InstallBatch::new(names(&["Owner-ModB"]), Vec::new()),
    &Operation::default(),
  );
  profile::set_enabled_in(fixture.dir(), &fixture.eco, "valheim", "Owner-ModB", false).unwrap();

  let mut batch = profile::plan_install_batch(fixture.dir(), &names(&["Owner-ModB"]), &[]).unwrap();

  batch.force = true;

  let mods_yml = fixture.dir().join("mods.yml");
  let operation = Operation::new(OperationId(1)).with_progress(move |event: ProgressEvent| {
    if event.stage == Stage::Installing && event.update == StageUpdate::Finished {
      std::fs::write(&mods_yml, "not: [a mod list").unwrap();
    }
  });

  let outcome = fixture.run(&rt, &batch, &operation);
  let failed: Vec<&str> = outcome
    .failed
    .iter()
    .map(|(name, _)| name.as_str())
    .collect();

  assert_eq!(outcome.succeeded, ["Owner-ModB"]);
  assert_eq!(
    failed,
    ["Owner-ModB"],
    "the reinstall enabled it and it could not be disabled again"
  );
  assert!(outcome.re_enabled.is_empty());
  assert!(outcome.kept_disabled.is_empty());
}
