//! End-to-end pipeline test, fully offline (HTTP stubbed with `mockito`).
//!
//! Exercises the whole chain wired together — fetch the community package index,
//! resolve what to download, download the archive, extract it, parse the
//! ecosystem, and generate + apply an install plan — and asserts the routing
//! instructions we produce for a real-shaped package are correct.

use std::io::{Cursor, Write};

use mockito::Server;
use tempfile::tempdir;
use thunderstore_engine::client::ThunderstoreClient;
use thunderstore_engine::ecosystem::Ecosystem;
use thunderstore_engine::extract::extract_zip;
use thunderstore_engine::install::{apply_install, plan_install};
use thunderstore_engine::models::DependencyGraph;
use tokio::runtime::Runtime;
use zip::write::{FileOptions, ZipWriter};

/// Builds a package zip mirroring a real Valheim BepInEx mod's layout, modeled on
/// `ValheimModding-Jotunn` 2.29.2: root metadata (`manifest.json`, `icon.png`,
/// `README.md`) plus a `plugins/` folder containing the DLL and its docs.
fn build_mod_zip() -> Vec<u8> {
  let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
  let opts: FileOptions<'_, ()> =
    FileOptions::default().compression_method(zip::CompressionMethod::Stored);

  let entries: [(&str, &[u8]); 5] = [
    ("manifest.json", br#"{"version_number":"1.0.0"}"#),
    ("icon.png", b"png-bytes"),
    ("README.md", b"# CoolMod"),
    ("plugins/CoolMod.dll", b"dll-bytes"),
    ("plugins/CoolMod.xml", b"<doc/>"),
  ];

  for (name, body) in entries {
    zip.start_file(name, opts).unwrap();
    zip.write_all(body).unwrap();
  }

  let cursor = zip.finish().unwrap();

  cursor.into_inner()
}

/// Builds the community package-index JSON with a single package whose download
/// URL points back at the mock server.
fn index_json(server_url: &str) -> String {
  format!(
    r#"[
      {{
        "name": "CoolMod",
        "full_name": "Author-CoolMod",
        "owner": "Author",
        "package_url": "https://example.com/CoolMod",
        "date_created": "2024-01-01T12:00:00Z",
        "date_updated": "2024-01-02T12:00:00Z",
        "uuid4": "pkg-uuid",
        "rating_score": 1,
        "is_pinned": false,
        "is_deprecated": false,
        "has_nsfw_content": false,
        "categories": [],
        "versions": [
          {{
            "name": "CoolMod",
            "full_name": "Author-CoolMod",
            "description": "A cool mod",
            "icon": "icon.png",
            "version_number": "1.0.0",
            "dependencies": [],
            "download_url": "{server_url}/dl/mod.zip",
            "downloads": 1,
            "date_created": "2024-01-01T12:00:00Z",
            "website_url": "",
            "is_active": true,
            "uuid4": "ver-uuid",
            "file_size": 1024
          }}
        ]
      }}
    ]"#
  )
}

#[test]
fn index_to_install_plan_end_to_end() {
  let mut server = Server::new();
  let zip_bytes = build_mod_zip();

  let download_mock = server
    .mock("GET", "/dl/mod.zip")
    .with_status(200)
    .with_header("Content-Type", "application/zip")
    .with_body(zip_bytes)
    .create();

  let index_mock = server
    .mock("GET", "/pkg/")
    .with_status(200)
    .with_header("Content-Type", "application/json")
    .with_header("Last-Modified", "Wed, 21 Feb 2024 15:30:45 GMT")
    .with_body(index_json(&server.url()))
    .create();

  let cache = tempdir().unwrap();

  let client = ThunderstoreClient::builder()
    .package_index_url(format!("{}/pkg/", server.url()))
    .cache_dir(cache.path())
    .build()
    .unwrap();

  let rt = Runtime::new().unwrap();

  // 1. Fetch the community package index (stubbed HTTP).
  let index = rt.block_on(client.get_manifest()).unwrap();

  assert_eq!(index.len(), 1);

  // 2. Resolve the requested package to a filename -> download-URL map.
  let urls = DependencyGraph::new(vec!["Author-CoolMod".to_string()]).resolve(&index);

  assert!(urls.contains_key("Author-CoolMod-1.0.0.zip"));

  // 3. Download the archive (stubbed HTTP), written atomically into the cache.
  rt.block_on(client.download_files(urls)).unwrap();

  let archive = cache
    .path()
    .join("downloads")
    .join("Author-CoolMod-1.0.0.zip");

  assert!(archive.exists());

  // 4. Extract it, with per-entry CRC verification.
  let extract_dir = cache.path().join("extract");

  extract_zip(&archive, &extract_dir).unwrap();

  assert!(extract_dir.join("plugins/CoolMod.dll").exists());

  // 5. Parse the ecosystem and route the package for Valheim.
  let eco = Ecosystem::bundled();
  let profile = eco.game("valheim").unwrap().profile().unwrap();
  let plan = plan_install(
    &eco,
    profile,
    "Author-CoolMod",
    &extract_dir,
    Some(&archive),
  )
  .unwrap();

  // 6. Verify the generated instructions: the plugin folder's contents are
  //    namespaced under BepInEx/plugins/<Owner-Name>/, structure preserved.
  let dests: Vec<String> = plan
    .entries
    .iter()
    .map(|e| e.dest_relative.to_string_lossy().replace('\\', "/"))
    .collect();

  assert!(dests.contains(&"BepInEx/plugins/Author-CoolMod/CoolMod.dll".to_string()));
  assert!(dests.contains(&"BepInEx/plugins/Author-CoolMod/CoolMod.xml".to_string()));

  // 7. Apply into a profile root and confirm files land and are recorded.
  let profile_root = tempdir().unwrap();
  let manifest = apply_install(&plan, profile_root.path()).unwrap();

  assert!(
    profile_root
      .path()
      .join("BepInEx/plugins/Author-CoolMod/CoolMod.dll")
      .exists()
  );
  assert!(manifest.written.iter().any(|p| p.ends_with("CoolMod.dll")));

  download_mock.assert();
  index_mock.assert();
}
