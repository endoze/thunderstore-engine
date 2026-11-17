//! The published mock Thunderstore, used the way a client crate would use it.

use tempfile::tempdir;
use thunderstore_engine::Error;
use thunderstore_engine::RetryPolicy;
use thunderstore_engine::client::ThunderstoreClient;
use thunderstore_engine::extract::extract_zip;
use thunderstore_engine::models::DependencyGraph;
use thunderstore_engine::test_support::MockThunderstore;
use tokio::runtime::Runtime;

#[test]
fn a_client_gets_the_index_and_downloads_archives_from_the_mock() {
  let mock = MockThunderstore::start();
  let cache = tempdir().unwrap();
  let client = mock
    .client_builder()
    .cache_dir(cache.path())
    .build()
    .unwrap();
  let rt = Runtime::new().unwrap();

  let index = rt.block_on(client.get_manifest()).unwrap();

  let mut names: Vec<&str> = (0..index.len())
    .filter_map(|idx| index.full_name_at(idx))
    .collect();

  names.sort();

  assert_eq!(
    names,
    [
      "Owner-ModA",
      "Owner-ModB",
      "Owner-ModC",
      "Owner-ModD",
      "denikson-BepInExPack_Valheim",
    ]
  );

  let urls = DependencyGraph::new(vec!["Owner-ModC".to_string()]).resolve(&index);

  rt.block_on(client.download_files(urls)).unwrap();

  let downloads = cache.path().join("downloads");
  let extracted = cache.path().join("extract");

  extract_zip(&downloads.join("Owner-ModC-1.0.0.zip"), &extracted).unwrap();
  extract_zip(&downloads.join("Owner-ModA-1.0.0.zip"), &extracted).unwrap();

  assert!(extracted.join("plugins/ModC.dll").exists());
  assert!(extracted.join("plugins/ModA.dll").exists());
  assert_eq!(
    std::fs::read_to_string(extracted.join("manifest.json")).unwrap(),
    r#"{"version_number":"1.0.0"}"#
  );
}

#[tokio::test]
async fn the_mock_starts_inside_an_async_test() {
  let mock = MockThunderstore::start_async().await;
  let cache = tempdir().unwrap();
  let client = mock
    .client_builder()
    .cache_dir(cache.path())
    .build()
    .unwrap();

  let index = client.get_manifest().await.unwrap();

  assert_eq!(index.len(), 5);
}

#[tokio::test]
async fn a_client_gets_a_version_readme_from_the_mock() {
  let mock = MockThunderstore::start_async().await;
  let cache = tempdir().unwrap();
  let client = mock
    .client_builder()
    .cache_dir(cache.path())
    .build()
    .unwrap();

  let readme = client.get_readme("Owner", "ModD", "2.0.0").await.unwrap();

  assert_eq!(readme.as_deref(), Some("# ModD 2.0.0\n\nA mod."));
}

#[tokio::test]
async fn a_package_without_a_readme_is_none_not_an_error() {
  let mock = MockThunderstore::start_async().await;
  let cache = tempdir().unwrap();
  let client = mock
    .client_builder()
    .cache_dir(cache.path())
    .build()
    .unwrap();

  let readme = client.get_readme("Owner", "ModB", "1.0.0").await.unwrap();

  assert_eq!(readme, None);
}

#[tokio::test]
async fn a_readme_request_that_cannot_reach_the_host_is_an_http_error() {
  let cache = tempdir().unwrap();
  // Nothing listens on loopback port 1, so the connection is refused.
  let client = ThunderstoreClient::builder()
    .base_url("http://127.0.0.1:1")
    .community("valheim")
    .cache_dir(cache.path())
    .retry(RetryPolicy::none())
    .build()
    .unwrap();

  let result = client.get_readme("Owner", "ModA", "1.0.0").await;

  assert!(
    matches!(
      &result,
      Err(Error::Http(error))
        if std::error::Error::source(error)
          .and_then(|source| source.downcast_ref::<reqwest::Error>())
          .is_some_and(reqwest::Error::is_connect)
    ),
    "{result:?}"
  );
}
