//! A mock Thunderstore for tests: an HTTP server that serves a small package
//! index and real package archives, so the install pipeline runs end to end
//! with no network access.
//!
//! The index has five packages, each downloaded from `/dl/<archive>.zip`:
//!
//! - `Owner-ModA` and `Owner-ModB` 1.0.0, with no dependencies.
//! - `Owner-ModC` 1.0.0, which depends on `Owner-ModA-1.0.0`.
//! - `Owner-ModD` at 1.0.0 and a newer 2.0.0, from `ModD-1.0.0.zip` and
//!   `ModD-2.0.0.zip`.
//! - `denikson-BepInExPack_Valheim` 5.4.2200, a real mod-loader identifier
//!   whose archive is shaped by [`loader_zip`](crate::test_support::loader_zip).
//!
//! Each mod archive ships a DLL named after its package (`ModA.dll`), so a test
//! can tell whose files landed where.
//!
//! Every version has a [`readme`] except `Owner-ModB`, which answers `404` like
//! a package Thunderstore has no readme for.
//!
//! ```no_run
//! use thunderstore_engine::test_support::MockThunderstore;
//!
//! # async fn demo() -> thunderstore_engine::Result<()> {
//! let mock = MockThunderstore::start_async().await;
//! let client = mock.client_builder().cache_dir("/tmp/cache").build()?;
//! let index = client.get_manifest().await?;
//!
//! assert_eq!(index.len(), 5);
//! # Ok(())
//! # }
//! ```

use std::io::{Cursor, Write};

use zip::write::{FileOptions, ZipWriter};

use crate::client::{ThunderstoreClient, ThunderstoreClientBuilder};

/// The `mockito` version the fixture is built on, so a test can add its own
/// mocks to [`MockThunderstore::server_mut`] without a second copy.
pub use mockito;

/// Path the package index is served from.
pub const PACKAGE_INDEX_PATH: &str = "/pkg/";

/// A running mock Thunderstore serving [`index_json`] and every archive it
/// names.
pub struct MockThunderstore {
  server: mockito::ServerGuard,
}

impl MockThunderstore {
  /// Starts the mock outside an async runtime.
  ///
  /// # Panics
  ///
  /// Panics inside an async runtime (use [`start_async`](Self::start_async))
  /// or if the server cannot start.
  pub fn start() -> Self {
    Self::serve(mockito::Server::new())
  }

  /// Starts the mock from async code, such as a `#[tokio::test]`.
  ///
  /// # Panics
  ///
  /// Panics if the server cannot start.
  pub async fn start_async() -> Self {
    Self::serve(mockito::Server::new_async().await)
  }

  fn serve(mut server: mockito::ServerGuard) -> Self {
    let url = server.url();

    server
      .mock("GET", PACKAGE_INDEX_PATH)
      .with_status(200)
      .with_header("Content-Type", "application/json")
      .with_header("Last-Modified", "Wed, 21 Feb 2024 15:30:45 GMT")
      .with_body(index_json(&url))
      .create();

    for (archive, body) in [
      ("ModA", mod_zip_with("1.0.0", "ModA.dll")),
      ("ModB", mod_zip_with("1.0.0", "ModB.dll")),
      ("ModC", mod_zip_with("1.0.0", "ModC.dll")),
      ("ModD-1.0.0", mod_zip_with("1.0.0", "ModD.dll")),
      ("ModD-2.0.0", mod_zip_with("2.0.0", "ModD.dll")),
      ("BepInExPack_Valheim", loader_zip()),
    ] {
      server
        .mock("GET", format!("/dl/{archive}.zip").as_str())
        .with_status(200)
        .with_header("Content-Type", "application/zip")
        .with_body(body)
        .create();
    }

    for (owner, name, version) in [
      ("Owner", "ModA", "1.0.0"),
      ("Owner", "ModC", "1.0.0"),
      ("Owner", "ModD", "1.0.0"),
      ("Owner", "ModD", "2.0.0"),
      ("denikson", "BepInExPack_Valheim", "5.4.2200"),
    ] {
      let body = serde_json::json!({ "markdown": readme(name, version) });

      server
        .mock("GET", readme_path(owner, name, version).as_str())
        .with_status(200)
        .with_header("Content-Type", "application/json")
        .with_body(body.to_string())
        .create();
    }

    server
      .mock("GET", readme_path("Owner", "ModB", "1.0.0").as_str())
      .with_status(404)
      .with_header("Content-Type", "application/json")
      .with_body(r#"{"detail":"Not found."}"#)
      .create();

    Self { server }
  }

  /// The server's base URL, such as `http://127.0.0.1:1234`.
  pub fn url(&self) -> String {
    self.server.url()
  }

  /// The full URL of the package index.
  pub fn package_index_url(&self) -> String {
    format!("{}{PACKAGE_INDEX_PATH}", self.url())
  }

  /// The underlying server, for adding mocks such as a profile-code endpoint
  /// or a failing download.
  pub fn server_mut(&mut self) -> &mut mockito::ServerGuard {
    &mut self.server
  }

  /// A client builder with both the host and the package index pointed at
  /// this mock. Set a cache directory, then build.
  pub fn client_builder(&self) -> ThunderstoreClientBuilder {
    ThunderstoreClient::builder()
      .base_url(self.url())
      .package_index_url(self.package_index_url())
  }
}

/// The readme markdown the mock serves for `name` at `version`, such as
/// `"# ModA 1.0.0\n\nA mod."`.
pub fn readme(name: &str, version: &str) -> String {
  format!("# {name} {version}\n\nA mod.")
}

/// Path a version's readme is served from, as Thunderstore lays it out.
pub fn readme_path(owner: &str, name: &str, version: &str) -> String {
  format!("/api/experimental/package/{owner}/{name}/{version}/readme/")
}

/// A mod archive with a manifest at `version` and one DLL, `plugins/ModA.dll`,
/// which the install rules route to `BepInEx/plugins/<Owner-Name>/`.
pub fn mod_zip(version: &str) -> Vec<u8> {
  mod_zip_with(version, "ModA.dll")
}

/// [`mod_zip`] with the DLL named `dll_name`.
pub fn mod_zip_with(version: &str, dll_name: &str) -> Vec<u8> {
  let manifest = format!("{{\"version_number\":\"{version}\"}}");

  stored_zip(&[
    ("manifest.json", manifest.as_bytes()),
    (&format!("plugins/{dll_name}"), b"dll-bytes"),
  ])
}

/// A mod-loader archive shaped like `denikson-BepInExPack_Valheim`: its payload
/// sits under the `rootFolder` the bundled ecosystem names for that package, so
/// an install strips the wrapper and the files land at the target root.
///
/// It holds the doorstop proxy `winhttp.dll` and `BepInEx/core/BepInEx.dll`.
/// The second lands in a declared namespaced route without an `<Owner-Name>/`
/// folder, which is the case that makes attributing loader files by shape
/// unsafe.
pub fn loader_zip() -> Vec<u8> {
  stored_zip(&[
    ("manifest.json", b"{\"version_number\":\"5.4.2200\"}"),
    ("BepInExPack_Valheim/winhttp.dll", b"proxy"),
    ("BepInExPack_Valheim/BepInEx/core/BepInEx.dll", b"core"),
  ])
}

fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
  let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
  let options: FileOptions<'_, ()> =
    FileOptions::default().compression_method(zip::CompressionMethod::Stored);

  for (name, body) in entries {
    zip.start_file(*name, options).expect("start a zip entry");
    zip.write_all(body).expect("write a zip entry");
  }

  zip.finish().expect("finish an in-memory zip").into_inner()
}

/// The package index described in the [module docs](self), with every
/// download URL under `server_url`.
pub fn index_json(server_url: &str) -> String {
  let version =
    |owner: &str, name: &str, number: &str, dependencies: &str, archive: &str, created: &str| {
      format!(
        r#"{{"name":"{name}","full_name":"{owner}-{name}",
      "description":"A mod","icon":"icon.png","version_number":"{number}",
      "dependencies":[{dependencies}],"download_url":"{server_url}/dl/{archive}.zip",
      "downloads":1,"date_created":"{created}",
      "website_url":"","is_active":true,"uuid4":"ver-{archive}","file_size":1024}}"#
      )
    };
  let package = |owner: &str, name: &str, versions: &[String]| {
    format!(
      r#"{{"name":"{name}","full_name":"{owner}-{name}","owner":"{owner}",
      "package_url":"https://example.com/{name}",
      "date_created":"2024-01-01T12:00:00Z","date_updated":"2024-01-02T12:00:00Z",
      "uuid4":"pkg-{name}","rating_score":1,"is_pinned":false,
      "is_deprecated":false,"has_nsfw_content":false,"categories":[],
      "versions":[{}]}}"#,
      versions.join(",")
    )
  };
  let single = |owner: &str, name: &str, number: &str, dependencies: &str| {
    package(
      owner,
      name,
      &[version(
        owner,
        name,
        number,
        dependencies,
        name,
        "2024-01-01T12:00:00Z",
      )],
    )
  };

  format!(
    "[{},{},{},{},{}]",
    single("Owner", "ModA", "1.0.0", ""),
    single("Owner", "ModB", "1.0.0", ""),
    single("Owner", "ModC", "1.0.0", "\"Owner-ModA-1.0.0\""),
    // The engine takes the latest version by `date_created`, so 2.0.0 must be
    // the later of the two.
    package(
      "Owner",
      "ModD",
      &[
        version(
          "Owner",
          "ModD",
          "2.0.0",
          "",
          "ModD-2.0.0",
          "2024-02-01T12:00:00Z",
        ),
        version(
          "Owner",
          "ModD",
          "1.0.0",
          "",
          "ModD-1.0.0",
          "2024-01-01T12:00:00Z",
        ),
      ],
    ),
    single("denikson", "BepInExPack_Valheim", "5.4.2200", "")
  )
}
