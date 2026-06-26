//! Export -> import round trip for a tracked mod set, fully offline.

use std::io::{Cursor, Write};

use mockito::Server;
use tempfile::tempdir;
use thunderstore_engine::client::ThunderstoreClient;
use thunderstore_engine::ecosystem::Ecosystem;
use thunderstore_engine::profile;
use tokio::runtime::Runtime;
use zip::write::{FileOptions, ZipWriter};

fn build_mod_zip() -> Vec<u8> {
  let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
  let opts: FileOptions<'_, ()> =
    FileOptions::default().compression_method(zip::CompressionMethod::Stored);

  for (name, body) in [
    ("manifest.json", &b"{\"version_number\":\"1.0.0\"}"[..]),
    ("plugins/CoolMod.dll", &b"dll-bytes"[..]),
  ] {
    zip.start_file(name, opts).unwrap();
    zip.write_all(body).unwrap();
  }

  zip.finish().unwrap().into_inner()
}

fn index_json(server_url: &str) -> String {
  format!(
    r#"[{{"name":"CoolMod","full_name":"Author-CoolMod","owner":"Author","package_url":"https://example.com/CoolMod","date_created":"2024-01-01T12:00:00Z","date_updated":"2024-01-02T12:00:00Z","uuid4":"pkg","rating_score":1,"is_pinned":false,"is_deprecated":false,"has_nsfw_content":false,"categories":[],"versions":[{{"name":"CoolMod","full_name":"Author-CoolMod","description":"A cool mod","icon":"icon.png","version_number":"1.0.0","dependencies":[],"download_url":"{server_url}/dl/mod.zip","downloads":1,"date_created":"2024-01-01T12:00:00Z","website_url":"","is_active":true,"uuid4":"ver","file_size":1024}}]}}]"#
  )
}

#[test]
fn export_then_import_round_trips_into_a_new_target() {
  let mut server = Server::new();

  let dl = server
    .mock("GET", "/dl/mod.zip")
    .with_status(200)
    .with_body(build_mod_zip())
    .expect_at_least(1)
    .create();
  let idx = server
    .mock("GET", "/pkg/")
    .with_status(200)
    .with_header("Content-Type", "application/json")
    .with_header("Last-Modified", "Wed, 21 Feb 2024 15:30:45 GMT")
    .with_body(index_json(&server.url()))
    .create();

  let cache = tempdir().unwrap();
  let base = tempdir().unwrap();

  let client = ThunderstoreClient::builder()
    .package_index_url(format!("{}/pkg/", server.url()))
    .base_url(server.url())
    .cache_dir(cache.path())
    .build()
    .unwrap();

  let rt = Runtime::new().unwrap();
  let eco = Ecosystem::bundled();
  let index = rt.block_on(client.get_manifest()).unwrap();

  // Populate a source target and give it a config file worth carrying.
  let source = base.path().join("source");
  rt.block_on(profile::install_mod_in(
    &source,
    base.path(),
    &eco,
    &index,
    &client,
    "valheim",
    "Author-CoolMod",
    123,
  ))
  .unwrap();

  std::fs::create_dir_all(source.join("BepInEx/config")).unwrap();
  std::fs::write(source.join("BepInEx/config/Cool.cfg"), b"tuned").unwrap();

  let r2z = profile::portability::build_r2z_in(&source, "Shared").unwrap();

  // Import into a fresh target.
  let target = base.path().join("target");
  let installed = rt
    .block_on(profile::portability::import_r2z_in(
      &target,
      base.path(),
      &eco,
      &index,
      &client,
      "valheim",
      &r2z,
      456,
    ))
    .unwrap();

  assert_eq!(installed, vec!["Author-CoolMod".to_string()]);
  assert!(
    target
      .join("BepInEx/plugins/Author-CoolMod/CoolMod.dll")
      .exists()
  );
  assert!(target.join("BepInEx/config/Cool.cfg").exists());
  assert_eq!(profile::modlist::read(&target).unwrap().len(), 1);

  dl.assert();
  idx.assert();
}

/// `import_in` has to route one source string to the same place a caller
/// classifying it by hand would, and read the archive itself rather than being
/// handed its bytes. Both routes are driven here from the string alone.
#[test]
fn import_in_routes_a_file_path_and_a_directory_from_the_source_string_alone() {
  let mut server = Server::new();

  let dl = server
    .mock("GET", "/dl/mod.zip")
    .with_status(200)
    .with_body(build_mod_zip())
    .expect_at_least(1)
    .create();
  let idx = server
    .mock("GET", "/pkg/")
    .with_status(200)
    .with_header("Content-Type", "application/json")
    .with_header("Last-Modified", "Wed, 21 Feb 2024 15:30:45 GMT")
    .with_body(index_json(&server.url()))
    .create();

  let cache = tempdir().unwrap();
  let base = tempdir().unwrap();

  let client = ThunderstoreClient::builder()
    .package_index_url(format!("{}/pkg/", server.url()))
    .base_url(server.url())
    .cache_dir(cache.path())
    .build()
    .unwrap();

  let rt = Runtime::new().unwrap();
  let eco = Ecosystem::bundled();
  let index = rt.block_on(client.get_manifest()).unwrap();
  let source = base.path().join("source");

  rt.block_on(profile::install_mod_in(
    &source,
    base.path(),
    &eco,
    &index,
    &client,
    "valheim",
    "Author-CoolMod",
    123,
  ))
  .unwrap();

  // The archive route: written to disk and named by path, so nothing but
  // `import_in` ever holds its bytes.
  let r2z_path = base.path().join("Shared.r2z");

  std::fs::write(
    &r2z_path,
    profile::portability::build_r2z_in(&source, "Shared").unwrap(),
  )
  .unwrap();

  let from_file = base.path().join("from-file");
  let outcome = rt
    .block_on(profile::portability::import_in(
      &from_file,
      base.path(),
      &eco,
      &client,
      "valheim",
      r2z_path.to_str().unwrap(),
      456,
    ))
    .unwrap();

  assert_eq!(
    outcome.source,
    profile::portability::ImportSource::Archive(r2z_path.clone())
  );
  assert_eq!(outcome.installed, vec!["Author-CoolMod".to_string()]);
  assert!(outcome.reinstalled.is_empty());
  assert!(!outcome.source.preserves_versions());
  assert!(
    from_file
      .join("BepInEx/plugins/Author-CoolMod/CoolMod.dll")
      .exists()
  );

  // The directory route: the same call, a different string, and an adoption
  // rather than an install.
  let from_dir = base.path().join("from-dir");
  let outcome = rt
    .block_on(profile::portability::import_in(
      &from_dir,
      base.path(),
      &eco,
      &client,
      "valheim",
      source.to_str().unwrap(),
      789,
    ))
    .unwrap();

  assert_eq!(
    outcome.source,
    profile::portability::ImportSource::R2modmanDir(source.clone())
  );
  assert_eq!(outcome.installed, vec!["Author-CoolMod".to_string()]);
  assert!(outcome.source.preserves_versions());
  assert!(
    from_dir
      .join("BepInEx/plugins/Author-CoolMod/CoolMod.dll")
      .exists()
  );

  dl.assert();
  idx.assert();
}

/// The disabled-state restore in `install_batch` must run even when the batch
/// fails partway through, not only on a batch where every item succeeds.
///
/// `import_r2z_in_keeps_a_disabled_entry_disabled_despite_a_later_dependency_install`,
/// the only other test that drives `install_batch` through the full pipeline,
/// has every item succeed, so it cannot prove the restore still runs after a
/// failure. This test disables an installed mod, then runs a batch naming that
/// mod alongside a name the index cannot resolve, so the batch fails on the
/// second item after the first has already been reinstalled (and, absent the
/// restore, silently re-enabled).
#[test]
fn install_batch_reapplies_disabled_state_despite_a_mid_batch_failure() {
  let mut server = Server::new();

  let dl = server
    .mock("GET", "/dl/mod.zip")
    .with_status(200)
    .with_body(build_mod_zip())
    .expect_at_least(1)
    .create();
  let idx = server
    .mock("GET", "/pkg/")
    .with_status(200)
    .with_header("Content-Type", "application/json")
    .with_header("Last-Modified", "Wed, 21 Feb 2024 15:30:45 GMT")
    .with_body(index_json(&server.url()))
    .create();

  let cache = tempdir().unwrap();
  let base = tempdir().unwrap();
  let client = ThunderstoreClient::builder()
    .package_index_url(format!("{}/pkg/", server.url()))
    .base_url(server.url())
    .cache_dir(cache.path())
    .build()
    .unwrap();

  let rt = Runtime::new().unwrap();
  let eco = Ecosystem::bundled();
  let index = rt.block_on(client.get_manifest()).unwrap();
  let target = base.path().join("target");

  rt.block_on(profile::install_mod_in(
    &target,
    base.path(),
    &eco,
    &index,
    &client,
    "valheim",
    "Author-CoolMod",
    1,
  ))
  .unwrap();

  profile::set_enabled_in(&target, &eco, "valheim", "Author-CoolMod", false).unwrap();

  assert!(
    target
      .join("BepInEx/plugins/Author-CoolMod/CoolMod.dll.old")
      .exists(),
    "the disable must have suffixed the file, or this test proves nothing"
  );

  // "Author-Missing" resolves to nothing in the index, so the batch fails on
  // it after Author-CoolMod has already been reinstalled.
  let desired = vec!["Author-CoolMod".to_string(), "Author-Missing".to_string()];
  let batch = profile::plan_install_batch(&target, &desired, &[]).unwrap();

  assert_eq!(batch.protect_disabled, vec!["Author-CoolMod".to_string()]);

  let outcome = rt
    .block_on(profile::install_batch(
      &target,
      base.path(),
      &eco,
      &index,
      &client,
      "valheim",
      &batch,
      2,
    ))
    .unwrap();

  assert!(
    !outcome.is_ok(),
    "Author-Missing is unresolvable, so this must be the mid-batch failure case"
  );
  assert_eq!(outcome.succeeded, vec!["Author-CoolMod".to_string()]);
  assert_eq!(outcome.failed.len(), 1);
  assert_eq!(outcome.failed[0].0, "Author-Missing");

  let mods = profile::modlist::read(&target).unwrap();
  let cool_mod = mods.iter().find(|m| m.name == "Author-CoolMod").unwrap();

  assert!(
    !cool_mod.enabled,
    "a mid-batch failure must not leave a previously-disabled mod re-enabled"
  );
  assert!(
    target
      .join("BepInEx/plugins/Author-CoolMod/CoolMod.dll.old")
      .exists(),
    "the disabled mod's files must stay in their disabled (.old) state"
  );

  dl.assert();
  idx.assert();
}

/// The sweep must run only once every desired name is adopted. Sweeping against
/// a partial record would delete mods that simply have not been reached yet.
#[test]
fn adoption_does_not_sweep_until_every_name_is_adopted() {
  let mut server = Server::new();

  let dl = server
    .mock("GET", "/dl/mod.zip")
    .with_status(200)
    .with_body(build_mod_zip())
    .expect_at_least(1)
    .create();
  let idx = server
    .mock("GET", "/pkg/")
    .with_status(200)
    .with_header("Content-Type", "application/json")
    .with_header("Last-Modified", "Wed, 21 Feb 2024 15:30:45 GMT")
    .with_body(index_json(&server.url()))
    .create();

  let cache = tempdir().unwrap();
  let base = tempdir().unwrap();
  let client = ThunderstoreClient::builder()
    .package_index_url(format!("{}/pkg/", server.url()))
    .base_url(server.url())
    .cache_dir(cache.path())
    .build()
    .unwrap();

  let rt = Runtime::new().unwrap();
  let eco = Ecosystem::bundled();
  let index = rt.block_on(client.get_manifest()).unwrap();
  let target = base.path().join("target");

  // A folder left behind by an older installer that the record does not name.
  std::fs::create_dir_all(target.join("BepInEx/plugins/Legacy-Leftover")).unwrap();
  std::fs::write(
    target.join("BepInEx/plugins/Legacy-Leftover/old.dll"),
    b"dll",
  )
  .unwrap();

  // One name resolves, one does not, so adoption is incomplete.
  let desired = vec!["Author-CoolMod".to_string(), "Author-Missing".to_string()];

  assert!(profile::portability::needs_adoption(&target, &desired).unwrap());

  let outcome = rt
    .block_on(profile::portability::adopt_names_in(
      &target,
      base.path(),
      &eco,
      &index,
      &client,
      "valheim",
      &desired,
      1,
    ))
    .unwrap();

  assert_eq!(outcome.adopted, vec!["Author-CoolMod".to_string()]);
  assert_eq!(outcome.remaining, vec!["Author-Missing".to_string()]);
  assert!(
    outcome.swept.is_empty(),
    "an incomplete adoption must not sweep, got {:?}",
    outcome.swept
  );
  assert!(
    target
      .join("BepInEx/plugins/Legacy-Leftover/old.dll")
      .exists(),
    "the leftover must survive an incomplete adoption"
  );

  // Adoption is keyed on missing entries, not a missing record, so a retry is
  // still reported as needed even though mods.yml now exists.
  assert!(profile::portability::needs_adoption(&target, &desired).unwrap());

  dl.assert();
  idx.assert();
}

/// A `mods.yml` naming one package, shaped the way r2modman actually leaves one
/// on disk. Deliberately written as text rather than through `modlist::write`, so
/// no package index is needed to build a source profile and the offline test
/// below can keep its zero-expectation mocks.
fn adopted_mods_yml(full_name: &str, owner: &str, display: &str, version: &str) -> String {
  let (major, minor, patch) = match version.split('.').collect::<Vec<_>>()[..] {
    [major, minor, patch] => (major, minor, patch),
    _ => panic!("version must be major.minor.patch, got {version:?}"),
  };

  format!(
    "- manifestVersion: 1\n  name: {full_name}\n  authorName: {owner}\n  \
     websiteUrl: ''\n  displayName: {display}\n  description: A mod\n  \
     gameVersion: '0'\n  networkMode: both\n  packageType: other\n  \
     installMode: managed\n  installedAtTime: 1700000000000\n  loaders: []\n  \
     dependencies: []\n  incompatibilities: []\n  optionalDependencies: []\n  \
     versionNumber: {{major: {major}, minor: {minor}, patch: {patch}}}\n  \
     enabled: true\n  onlineSource: true\n  trustedPackage: false\n"
  )
}

/// A loader archive shaped like `denikson-BepInExPack_Valheim`: the payload nests
/// under the `rootFolder` the ecosystem snapshot names, so the install strips the
/// wrapper and the contents land at the target root.
///
/// `BepInEx/core/` matters specifically: it is a declared namespaced route, so a
/// loader file landing there directly rather than in an `<Owner-Name>/` folder is
/// the case that makes attributing adopted files by shape unsafe, and therefore
/// the reason a reinstall is the only way to get an exact file list.
fn build_loader_zip() -> Vec<u8> {
  let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
  let opts: FileOptions<'_, ()> =
    FileOptions::default().compression_method(zip::CompressionMethod::Stored);

  for (name, body) in [
    ("manifest.json", &b"{\"version_number\":\"5.4.2200\"}"[..]),
    ("BepInExPack_Valheim/winhttp.dll", &b"proxy"[..]),
    ("BepInExPack_Valheim/BepInEx/core/BepInEx.dll", &b"core"[..]),
  ] {
    zip.start_file(name, opts).unwrap();
    zip.write_all(body).unwrap();
  }

  zip.finish().unwrap().into_inner()
}

/// An index serving the real `denikson-BepInExPack_Valheim` identifier, so
/// `Ecosystem::modloader_package` recognises it as a loader.
fn loader_index_json(server_url: &str) -> String {
  format!(
    r#"[{{"name":"BepInExPack_Valheim","full_name":"denikson-BepInExPack_Valheim","owner":"denikson","package_url":"https://example.com/BepInExPack_Valheim","date_created":"2024-01-01T12:00:00Z","date_updated":"2024-01-02T12:00:00Z","uuid4":"pkg-loader","rating_score":1,"is_pinned":false,"is_deprecated":false,"has_nsfw_content":false,"categories":[],"versions":[{{"name":"BepInExPack_Valheim","full_name":"denikson-BepInExPack_Valheim","description":"The loader","icon":"icon.png","version_number":"5.4.2200","dependencies":[],"download_url":"{server_url}/dl/loader.zip","downloads":1,"date_created":"2024-01-01T12:00:00Z","website_url":"","is_active":true,"uuid4":"ver-loader","file_size":1024}}]}}]"#
  )
}

#[test]
fn adopting_a_directory_that_names_no_loader_downloads_nothing() {
  let mut server = Server::new();

  // Zero-expectation mocks on both endpoints an adoption could reach, so
  // `.assert()` fails loudly if this path touches the network at all.
  let idx = server.mock("GET", "/pkg/").expect(0).create();
  let dl = server.mock("GET", "/dl/loader.zip").expect(0).create();

  let cache = tempdir().unwrap();
  let base = tempdir().unwrap();

  let client = ThunderstoreClient::builder()
    .package_index_url(format!("{}/pkg/", server.url()))
    .base_url(server.url())
    .cache_dir(cache.path())
    .build()
    .unwrap();

  let source = base.path().join("source");

  std::fs::create_dir_all(source.join("BepInEx/plugins/Author-CoolMod")).unwrap();
  std::fs::write(
    source.join("mods.yml"),
    adopted_mods_yml("Author-CoolMod", "Author", "CoolMod", "1.0.0"),
  )
  .unwrap();
  std::fs::write(
    source.join("BepInEx/plugins/Author-CoolMod/CoolMod.dll"),
    b"dll",
  )
  .unwrap();

  let target = base.path().join("target");
  let rt = Runtime::new().unwrap();
  let eco = Ecosystem::bundled();

  let result = rt
    .block_on(profile::portability::adopt_r2modman_dir_in(
      &source,
      &target,
      base.path(),
      &eco,
      &client,
      "valheim",
      456,
    ))
    .unwrap();

  assert_eq!(result.adopted, vec!["Author-CoolMod".to_string()]);
  assert!(
    result.reinstalled.is_empty(),
    "an ordinary mod needs no install record, so nothing should be reinstalled"
  );
  // The adoption is still a copy: the mod's files came across as they were.
  assert!(
    target
      .join("BepInEx/plugins/Author-CoolMod/CoolMod.dll")
      .exists()
  );

  // The index is fetched only once something actually needs reinstalling, so an
  // adoption with nothing to fix never reaches the network.
  idx.assert();
  dl.assert();
}

#[test]
fn adopting_a_directory_gives_an_unrecorded_loader_an_install_record() {
  let mut server = Server::new();

  let dl = server
    .mock("GET", "/dl/loader.zip")
    .with_status(200)
    .with_body(build_loader_zip())
    .expect_at_least(1)
    .create();
  let idx = server
    .mock("GET", "/pkg/")
    .with_status(200)
    .with_header("Content-Type", "application/json")
    .with_header("Last-Modified", "Wed, 21 Feb 2024 15:30:45 GMT")
    .with_body(loader_index_json(&server.url()))
    .create();

  let cache = tempdir().unwrap();
  let base = tempdir().unwrap();

  let client = ThunderstoreClient::builder()
    .package_index_url(format!("{}/pkg/", server.url()))
    .base_url(server.url())
    .cache_dir(cache.path())
    .build()
    .unwrap();

  // A source shaped the way r2modman leaves one: the loader named in mods.yml,
  // its files on disk, and no `_state` tracker, because `_state` is this
  // engine's own invention.
  let source = base.path().join("source");

  std::fs::create_dir_all(source.join("BepInEx/core")).unwrap();
  std::fs::write(
    source.join("mods.yml"),
    adopted_mods_yml(
      "denikson-BepInExPack_Valheim",
      "denikson",
      "BepInExPack_Valheim",
      "5.4.2200",
    ),
  )
  .unwrap();
  std::fs::write(source.join("winhttp.dll"), b"proxy").unwrap();

  let target = base.path().join("target");
  let rt = Runtime::new().unwrap();
  let eco = Ecosystem::bundled();

  let result = rt
    .block_on(profile::portability::adopt_r2modman_dir_in(
      &source,
      &target,
      base.path(),
      &eco,
      &client,
      "valheim",
      456,
    ))
    .unwrap();

  assert_eq!(
    result.reinstalled,
    vec!["denikson-BepInExPack_Valheim".to_string()]
  );

  // The record is the whole point: without it `ensure_removable` refuses, so the
  // adopted loader could never be uninstalled exactly.
  assert!(
    thunderstore_engine::install::state_file_path(&target, "denikson-BepInExPack_Valheim").exists(),
    "the adopted loader must end up with an install record"
  );
  assert!(
    profile::ensure_removable(&target, &eco, "denikson-BepInExPack_Valheim").is_ok(),
    "an adopted profile must be left in a state its own uninstall accepts"
  );
  assert_eq!(profile::modlist::read(&target).unwrap().len(), 1);

  dl.assert();
  idx.assert();
}

#[test]
fn adopting_a_directory_does_not_reinstall_a_loader_that_already_has_a_record() {
  let mut server = Server::new();

  let idx = server.mock("GET", "/pkg/").expect(0).create();
  let dl = server.mock("GET", "/dl/loader.zip").expect(0).create();

  let cache = tempdir().unwrap();
  let base = tempdir().unwrap();

  let client = ThunderstoreClient::builder()
    .package_index_url(format!("{}/pkg/", server.url()))
    .base_url(server.url())
    .cache_dir(cache.path())
    .build()
    .unwrap();

  let source = base.path().join("source");

  std::fs::create_dir_all(&source).unwrap();
  std::fs::write(
    source.join("mods.yml"),
    adopted_mods_yml(
      "denikson-BepInExPack_Valheim",
      "denikson",
      "BepInExPack_Valheim",
      "5.4.2200",
    ),
  )
  .unwrap();
  std::fs::write(source.join("winhttp.dll"), b"proxy").unwrap();

  // The target already carries a record for the loader. An empty file list is a
  // valid tracker and is enough to satisfy the precondition without claiming
  // ownership of any file, which keeps this test off the network entirely.
  let target = base.path().join("target");
  let record =
    thunderstore_engine::install::state_file_path(&target, "denikson-BepInExPack_Valheim");

  std::fs::create_dir_all(record.parent().unwrap()).unwrap();
  std::fs::write(
    &record,
    "modName: denikson-BepInExPack_Valheim\nfiles: []\n",
  )
  .unwrap();

  let rt = Runtime::new().unwrap();
  let eco = Ecosystem::bundled();

  let result = rt
    .block_on(profile::portability::adopt_r2modman_dir_in(
      &source,
      &target,
      base.path(),
      &eco,
      &client,
      "valheim",
      456,
    ))
    .unwrap();

  assert!(
    result.reinstalled.is_empty(),
    "a loader that already has a record must be left alone"
  );
  assert_eq!(
    result.adopted,
    vec!["denikson-BepInExPack_Valheim".to_string()]
  );

  idx.assert();
  dl.assert();
}

fn build_mod_zip_named(dll_name: &str) -> Vec<u8> {
  let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
  let opts: FileOptions<'_, ()> =
    FileOptions::default().compression_method(zip::CompressionMethod::Stored);

  zip.start_file("manifest.json", opts).unwrap();
  zip.write_all(b"{\"version_number\":\"1.0.0\"}").unwrap();

  zip.start_file(format!("plugins/{dll_name}"), opts).unwrap();
  zip.write_all(b"dll-bytes").unwrap();

  zip.finish().unwrap().into_inner()
}

/// Two packages: `Author-Base` (no dependencies) and `Author-Top`, which
/// depends on `Author-Base`.
fn index_json_base_and_top(server_url: &str) -> String {
  format!(
    r#"[
      {{"name":"Base","full_name":"Author-Base","owner":"Author","package_url":"https://example.com/Author-Base","date_created":"2024-01-01T12:00:00Z","date_updated":"2024-01-02T12:00:00Z","uuid4":"pkg-base","rating_score":1,"is_pinned":false,"is_deprecated":false,"has_nsfw_content":false,"categories":[],"versions":[{{"name":"Base","full_name":"Author-Base","description":"Base mod","icon":"icon.png","version_number":"1.0.0","dependencies":[],"download_url":"{server_url}/dl/base.zip","downloads":1,"date_created":"2024-01-01T12:00:00Z","website_url":"","is_active":true,"uuid4":"ver-base","file_size":1024}}]}},
      {{"name":"Top","full_name":"Author-Top","owner":"Author","package_url":"https://example.com/Author-Top","date_created":"2024-01-01T12:00:00Z","date_updated":"2024-01-02T12:00:00Z","uuid4":"pkg-top","rating_score":1,"is_pinned":false,"is_deprecated":false,"has_nsfw_content":false,"categories":[],"versions":[{{"name":"Top","full_name":"Author-Top","description":"Top mod","icon":"icon.png","version_number":"1.0.0","dependencies":["Author-Base-1.0.0"],"download_url":"{server_url}/dl/top.zip","downloads":1,"date_created":"2024-01-01T12:00:00Z","website_url":"","is_active":true,"uuid4":"ver-top","file_size":1024}}]}}
    ]"#
  )
}

/// Regression guard for the property that an entry the export marked
/// disabled stays disabled even when a later mod pulls it in as a
/// dependency: the export lists a disabled entry (`Author-Base`) before an
/// enabled entry that depends on it (`Author-Top`). Installing `Author-Top`
/// re-installs `Author-Base` as its dependency, which upserts `mods.yml`
/// with `enabled: true`, so if the disabled state were not restored after
/// every install in the batch has completed, `Author-Top`'s later install
/// would silently flip `Author-Base` back to enabled.
#[test]
fn import_r2z_in_keeps_a_disabled_entry_disabled_despite_a_later_dependency_install() {
  let mut server = Server::new();

  let base_dl = server
    .mock("GET", "/dl/base.zip")
    .with_status(200)
    .with_body(build_mod_zip_named("BaseMod.dll"))
    .expect_at_least(1)
    .create();
  let top_dl = server
    .mock("GET", "/dl/top.zip")
    .with_status(200)
    .with_body(build_mod_zip_named("TopMod.dll"))
    .expect_at_least(1)
    .create();
  let idx = server
    .mock("GET", "/pkg/")
    .with_status(200)
    .with_header("Content-Type", "application/json")
    .with_header("Last-Modified", "Wed, 21 Feb 2024 15:30:45 GMT")
    .with_body(index_json_base_and_top(&server.url()))
    .create();

  let cache = tempdir().unwrap();
  let base = tempdir().unwrap();

  let client = ThunderstoreClient::builder()
    .package_index_url(format!("{}/pkg/", server.url()))
    .base_url(server.url())
    .cache_dir(cache.path())
    .build()
    .unwrap();

  let rt = Runtime::new().unwrap();
  let eco = Ecosystem::bundled();
  let index = rt.block_on(client.get_manifest()).unwrap();

  // Build a source `mods.yml` directly (no install pipeline involved), with
  // the disabled entry listed first so the export preserves that order.
  let base_version = index
    .get_package_by_full_name("Author-Base")
    .unwrap()
    .latest_version()
    .unwrap()
    .clone();
  let top_version = index
    .get_package_by_full_name("Author-Top")
    .unwrap()
    .latest_version()
    .unwrap()
    .clone();

  let mut base_entry = profile::mod_entry_from_version("Author-Base", "Author", &base_version, 1);
  base_entry.enabled = false;

  let top_entry = profile::mod_entry_from_version("Author-Top", "Author", &top_version, 1);

  let source = base.path().join("source");

  profile::modlist::write(&source, &[base_entry, top_entry]).unwrap();

  let r2z = profile::portability::build_r2z_in(&source, "Shared").unwrap();

  // Import into a fresh target, exercising the real install + dependency
  // resolution path.
  let target = base.path().join("target");
  let installed = rt
    .block_on(profile::portability::import_r2z_in(
      &target,
      base.path(),
      &eco,
      &index,
      &client,
      "valheim",
      &r2z,
      999,
    ))
    .unwrap();

  assert_eq!(
    installed,
    vec!["Author-Base".to_string(), "Author-Top".to_string()]
  );

  let mods = profile::modlist::read(&target).unwrap();
  let base_mod = mods.iter().find(|m| m.name == "Author-Base").unwrap();
  let top_mod = mods.iter().find(|m| m.name == "Author-Top").unwrap();

  assert!(
    !base_mod.enabled,
    "Author-Base was exported disabled and must stay disabled even though \
     Author-Top's later install re-installs it as a dependency"
  );
  assert!(top_mod.enabled);

  base_dl.assert();
  top_dl.assert();
  idx.assert();
}

/// Same package as [`build_mod_zip`], plus a per-mod config file the package
/// ships with a default value.
fn build_mod_zip_with_config() -> Vec<u8> {
  let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
  let opts: FileOptions<'_, ()> =
    FileOptions::default().compression_method(zip::CompressionMethod::Stored);

  for (name, body) in [
    ("manifest.json", &b"{\"version_number\":\"1.0.0\"}"[..]),
    ("plugins/CoolMod.dll", &b"dll-bytes"[..]),
    ("plugins/Cool.cfg", &b"PACKAGE-DEFAULT"[..]),
  ] {
    zip.start_file(name, opts).unwrap();
    zip.write_all(body).unwrap();
  }

  zip.finish().unwrap().into_inner()
}

/// Regression guard for `import_r2z_in`'s install-then-extract ordering: a
/// per-mod config under `BepInEx/plugins/<Owner-Name>/` is `Subdir`-tracked,
/// so (re)installing its owning mod always rewrites it with the package's
/// default. Extracting the export's configs before installing would let that
/// default clobber the user's exported config the moment its mod installs;
/// extracting after must leave the exported config in place.
#[test]
fn import_r2z_in_lets_an_exported_per_mod_config_survive_the_reinstall() {
  let mut server = Server::new();

  let dl = server
    .mock("GET", "/dl/mod.zip")
    .with_status(200)
    .with_body(build_mod_zip_with_config())
    .expect_at_least(1)
    .create();
  let idx = server
    .mock("GET", "/pkg/")
    .with_status(200)
    .with_header("Content-Type", "application/json")
    .with_header("Last-Modified", "Wed, 21 Feb 2024 15:30:45 GMT")
    .with_body(index_json(&server.url()))
    .create();

  let cache = tempdir().unwrap();
  let base = tempdir().unwrap();

  let client = ThunderstoreClient::builder()
    .package_index_url(format!("{}/pkg/", server.url()))
    .base_url(server.url())
    .cache_dir(cache.path())
    .build()
    .unwrap();

  let rt = Runtime::new().unwrap();
  let eco = Ecosystem::bundled();
  let index = rt.block_on(client.get_manifest()).unwrap();

  // Populate a source target, then simulate the user hand-tuning the mod's
  // own config away from the package default.
  let source = base.path().join("source");

  rt.block_on(profile::install_mod_in(
    &source,
    base.path(),
    &eco,
    &index,
    &client,
    "valheim",
    "Author-CoolMod",
    123,
  ))
  .unwrap();

  let config_path = source.join("BepInEx/plugins/Author-CoolMod/Cool.cfg");

  assert_eq!(
    std::fs::read_to_string(&config_path).unwrap(),
    "PACKAGE-DEFAULT"
  );
  std::fs::write(&config_path, b"USER-TUNED").unwrap();

  let r2z = profile::portability::build_r2z_in(&source, "Shared").unwrap();

  // Import into a fresh target, exercising the real install pipeline so the
  // package's own config is (re)written before the export is extracted.
  let target = base.path().join("target");

  rt.block_on(profile::portability::import_r2z_in(
    &target,
    base.path(),
    &eco,
    &index,
    &client,
    "valheim",
    &r2z,
    456,
  ))
  .unwrap();

  assert_eq!(
    std::fs::read_to_string(target.join("BepInEx/plugins/Author-CoolMod/Cool.cfg")).unwrap(),
    "USER-TUNED",
    "the exported per-mod config must survive the package's default install"
  );

  dl.assert();
  idx.assert();
}

/// `fs::copy(p, p)` opens the destination with `O_TRUNC` before reading the
/// source, so copying a tree onto itself zeroes every file and reports success on
/// each one. A zero-length file here is the signature of that bug.
#[test]
fn importing_a_directory_into_itself_is_refused_before_anything_is_truncated() {
  let dir = tempdir().unwrap();
  let target = dir.path().join("profile");

  std::fs::create_dir_all(target.join("BepInEx/config")).unwrap();
  std::fs::write(target.join("winhttp.dll"), b"loader-proxy-bytes").unwrap();
  std::fs::write(target.join("BepInEx/config/mod.cfg"), b"hand-tuned = true").unwrap();

  let result = profile::portability::import_r2modman_dir(&target, &target);

  assert!(
    result.is_err(),
    "importing a directory into itself must be refused"
  );

  for path in [
    target.join("winhttp.dll"),
    target.join("BepInEx/config/mod.cfg"),
  ] {
    let size = std::fs::metadata(&path).unwrap().len();

    assert!(size > 0, "{} was truncated to {size} bytes", path.display());
  }

  // A source inside the target, and a source containing it, are refused for the
  // same reason: the copy would read the tree it is writing.
  let nested = target.join("nested");

  std::fs::create_dir_all(&nested).unwrap();

  assert!(profile::portability::ensure_disjoint_trees(&nested, &target).is_err());
  assert!(profile::portability::ensure_disjoint_trees(&target, &nested).is_err());

  // A sibling outside the target is fine.
  let outside = dir.path().join("elsewhere");

  std::fs::create_dir_all(&outside).unwrap();

  assert!(profile::portability::ensure_disjoint_trees(&outside, &target).is_ok());
}

/// A not-yet-created path cannot be canonicalized whole, so `ensure_disjoint_trees`
/// must still resolve a symlinked ancestor further up the chain rather than
/// falling back to comparing the raw, unresolved spelling. Reached through the
/// symlink, `alias/shared/nested_will_be_created` really names a path inside
/// `real/profiles/shared` itself, even though `nested_will_be_created` has not
/// been created and `copy_tree` would be the one to create it.
#[test]
#[cfg(unix)]
fn ensure_disjoint_trees_resolves_a_symlinked_ancestor_even_when_the_leaf_does_not_exist_yet() {
  use std::os::unix::fs::symlink;

  let dir = tempdir().unwrap();
  let real_profiles = dir.path().join("real").join("profiles");
  let shared = real_profiles.join("shared");

  std::fs::create_dir_all(&shared).unwrap();

  let alias = dir.path().join("alias");

  symlink(&real_profiles, &alias).unwrap();

  let not_yet_created = alias.join("shared").join("nested_will_be_created");

  assert!(
    profile::portability::ensure_disjoint_trees(&shared, &not_yet_created).is_err(),
    "a not-yet-created path reached through a symlinked ancestor must still be \
     recognised as nested inside the source"
  );
}

/// A bare `.r2x` manifest (no zip wrapper) is a valid import: it just carries
/// no config files. `import_r2z_in` must install the listed mod rather than
/// hard-failing on the missing zip container.
#[test]
fn import_r2z_in_accepts_a_bare_r2x_manifest_with_no_configs() {
  let mut server = Server::new();

  let dl = server
    .mock("GET", "/dl/mod.zip")
    .with_status(200)
    .with_body(build_mod_zip())
    .expect_at_least(1)
    .create();
  let idx = server
    .mock("GET", "/pkg/")
    .with_status(200)
    .with_header("Content-Type", "application/json")
    .with_header("Last-Modified", "Wed, 21 Feb 2024 15:30:45 GMT")
    .with_body(index_json(&server.url()))
    .create();

  let cache = tempdir().unwrap();
  let base = tempdir().unwrap();

  let client = ThunderstoreClient::builder()
    .package_index_url(format!("{}/pkg/", server.url()))
    .base_url(server.url())
    .cache_dir(cache.path())
    .build()
    .unwrap();

  let rt = Runtime::new().unwrap();
  let eco = Ecosystem::bundled();
  let index = rt.block_on(client.get_manifest()).unwrap();

  // `ExportFormat`/`ExportMod` are `#[non_exhaustive]`, so build the manifest
  // through the public constructors rather than a struct literal.
  let version = index
    .get_package_by_full_name("Author-CoolMod")
    .unwrap()
    .latest_version()
    .unwrap()
    .clone();
  let entry = profile::mod_entry_from_version("Author-CoolMod", "Author", &version, 1);
  let export = profile::portability::export_format_from_mods("Bare", &[entry]);
  let yaml = profile::portability::export_r2x_to_string(&export).unwrap();

  let target = base.path().join("target");
  let installed = rt
    .block_on(profile::portability::import_r2z_in(
      &target,
      base.path(),
      &eco,
      &index,
      &client,
      "valheim",
      yaml.as_bytes(),
      789,
    ))
    .unwrap();

  assert_eq!(installed, vec!["Author-CoolMod".to_string()]);
  assert!(target.join("mods.yml").exists());
  assert!(
    target
      .join("BepInEx/plugins/Author-CoolMod/CoolMod.dll")
      .exists()
  );

  dl.assert();
  idx.assert();
}
