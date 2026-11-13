//! Guards that the client's public async methods return `Send` futures, so
//! consumers can drive them from a multi-threaded runtime (e.g. `tokio::spawn`).
//! These are compile-only checks — the futures are constructed but never awaited.

use std::collections::HashMap;
use thunderstore_engine::client::ThunderstoreClient;

fn assert_send<T: Send>(_: &T) {}

#[test]
fn public_async_futures_are_send() {
  let client = ThunderstoreClient::builder()
    .package_index_url("https://example.com/pkg/")
    .cache_dir(std::env::temp_dir())
    .build()
    .unwrap();

  assert_send(&client.get_manifest());
  assert_send(&client.refresh_index());
  assert_send(&client.download_files(HashMap::new()));
}

/// Never called: building the future is enough for the compiler to check it.
#[allow(dead_code)]
fn install_batch_with_is_send(
  client: &ThunderstoreClient,
  index: &thunderstore_engine::models::PackageIndex,
  eco: &thunderstore_engine::ecosystem::Ecosystem,
) {
  let batch = thunderstore_engine::profile::InstallBatch::default();
  let operation = thunderstore_engine::progress::Operation::default();

  assert_send(&thunderstore_engine::profile::install_batch_with(
    std::path::Path::new("target"),
    std::path::Path::new("cache"),
    eco,
    index,
    client,
    "valheim",
    &batch,
    0,
    &operation,
  ));
}
