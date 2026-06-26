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
