//! The package-index cache decode never allocates past its 1 GiB limit, even
//! for a crafted cache whose one nested list would need more.
//!
//! This is its own test binary because it installs a global allocator that
//! records the largest single allocation, and no other test may run beside it.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use thunderstore_engine::Error;
use thunderstore_engine::client::ThunderstoreClient;

struct LargestAllocation;

static LARGEST: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call forwards to `System` with the caller's arguments unchanged.
unsafe impl GlobalAlloc for LargestAllocation {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    LARGEST.fetch_max(layout.size(), Ordering::Relaxed);

    unsafe { System.alloc(layout) }
  }

  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    unsafe { System.dealloc(ptr, layout) }
  }

  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    LARGEST.fetch_max(new_size, Ordering::Relaxed);

    unsafe { System.realloc(ptr, layout, new_size) }
  }
}

#[global_allocator]
static ALLOCATOR: LargestAllocation = LargestAllocation;

const GIB: usize = 1024 * 1024 * 1024;

#[tokio::test]
async fn a_nested_list_larger_than_the_decode_limit_is_refused_before_it_is_allocated() {
  let cache = tempfile::tempdir().unwrap();
  let url = "http://127.0.0.1:9/pkg/";
  let client = ThunderstoreClient::builder()
    .package_index_url(url)
    .cache_dir(cache.path())
    .build()
    .unwrap();

  // Every table empty up to `categories`, which holds one inner list of
  // 2^28 + 1 `0u32`s: one byte each on disk, but 2 GiB of capacity once a
  // doubling `Vec` holds them all.
  let entries: usize = (1 << 28) + 1;
  let mut blob = vec![3u8];
  blob.extend([0u8; 12]);
  blob.push(1);
  let mut len = entries;
  while len >= 0x80 {
    blob.push((len as u8) | 0x80);
    len >>= 7;
  }
  blob.push(len as u8);
  blob.resize(blob.len() + entries, 0);

  let compressed = zstd::encode_all(blob.as_slice(), 1).unwrap();
  drop(blob);

  let index_dir = cache
    .path()
    .join("index")
    .join(format!("{:08x}", crc32fast::hash(url.as_bytes())));
  std::fs::create_dir_all(&index_dir).unwrap();
  std::fs::write(index_dir.join("api_manifest.postcard.zst"), compressed).unwrap();

  LARGEST.store(0, Ordering::Relaxed);

  let result = client.cached_index().await;

  assert!(
    matches!(&result, Err(Error::Cache(message)) if message.contains("allocation limit")),
    "{:?}",
    result.map(|index| index.is_some())
  );

  let largest = LARGEST.load(Ordering::Relaxed);

  assert!(
    largest < GIB,
    "largest single allocation was {} MiB",
    largest >> 20
  );
}
