//! End-to-end profile install pipeline, offline (HTTP served by the published
//! mock Thunderstore).

use tempfile::tempdir;
use thunderstore_engine::ecosystem::Ecosystem;
use thunderstore_engine::profile;
use thunderstore_engine::test_support::MockThunderstore;
use tokio::runtime::Runtime;

#[test]
fn install_mod_pipeline_populates_profile() {
  let mock = MockThunderstore::start();
  let cache = tempdir().unwrap();
  let base = tempdir().unwrap();
  let client = mock
    .client_builder()
    .cache_dir(cache.path())
    .build()
    .unwrap();

  let rt = Runtime::new().unwrap();
  let eco = Ecosystem::bundled();
  let index = rt.block_on(client.get_manifest()).unwrap();

  profile::layout::create(base.path(), "valheim", "Main").unwrap();

  let mut installed = rt
    .block_on(profile::install_mod(
      base.path(),
      &eco,
      &index,
      &client,
      "valheim",
      "Main",
      "Owner-ModC",
      123,
    ))
    .unwrap();

  installed.sort();

  assert_eq!(installed, ["Owner-ModA", "Owner-ModC"]);

  let profile_dir = profile::layout::profile_dir(base.path(), "valheim", "Main");

  assert!(
    profile_dir
      .join("BepInEx/plugins/Owner-ModC/ModC.dll")
      .exists()
  );
  assert!(
    profile_dir
      .join("BepInEx/plugins/Owner-ModA/ModA.dll")
      .exists()
  );

  let mut mods: Vec<String> = profile::modlist::read(&profile_dir)
    .unwrap()
    .into_iter()
    .map(|entry| entry.name)
    .collect();

  mods.sort();

  assert_eq!(mods, ["Owner-ModA", "Owner-ModC"]);
}
