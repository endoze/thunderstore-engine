//! End-to-end profile install pipeline, offline (HTTP stubbed with `mockito`).

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
fn install_mod_pipeline_populates_profile() {
  let mut server = Server::new();
  let zip_bytes = build_mod_zip();

  let dl = server
    .mock("GET", "/dl/mod.zip")
    .with_status(200)
    .with_header("Content-Type", "application/zip")
    .with_body(zip_bytes)
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
    .cache_dir(cache.path())
    .build()
    .unwrap();

  let rt = Runtime::new().unwrap();
  let eco = Ecosystem::bundled();
  let index = rt.block_on(client.get_manifest()).unwrap();

  profile::layout::create(base.path(), "valheim", "Main").unwrap();

  let installed = rt
    .block_on(profile::install_mod(
      base.path(),
      &eco,
      &index,
      &client,
      "valheim",
      "Main",
      "Author-CoolMod",
      123,
    ))
    .unwrap();

  assert_eq!(installed, vec!["Author-CoolMod".to_string()]);

  let profile_dir = profile::layout::profile_dir(base.path(), "valheim", "Main");

  assert!(
    profile_dir
      .join("BepInEx/plugins/Author-CoolMod/CoolMod.dll")
      .exists()
  );

  let mods = profile::modlist::read(&profile_dir).unwrap();

  assert_eq!(mods.len(), 1);
  assert_eq!(mods[0].name, "Author-CoolMod");

  dl.assert();
  idx.assert();
}
