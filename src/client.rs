//! Async Thunderstore API client and package-index cache.
//!
//! [`ThunderstoreClient`] fetches a community's package index
//! (`/c/<community>/api/v1/package/`) and caches it on disk compressed with
//! `zstd` + `bincode`, keyed by a `Last-Modified` freshness check. It also
//! downloads package archives with bounded concurrency. All progress is
//! reported through [`crate::progress`] so the crate stays UI-agnostic.
//!
//! API reference: <https://thunderstore.io/api/docs/>

use crate::error::{Error, Result};
use crate::models::{
  InternedPackageManifest, Package, PackageManifest, SerializableInternedManifest,
};
use crate::progress::{NoopProgress, ProgressReporter};

use chrono::prelude::*;
use futures::stream::{FuturesUnordered, StreamExt};
use reqwest::Client;
use reqwest::header;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Default Thunderstore host used when no `base_url` is supplied.
const DEFAULT_BASE_URL: &str = "https://thunderstore.io";
const LAST_MODIFIED_FILENAME: &str = "last_modified";
const API_MANIFEST_FILENAME_V3: &str = "api_manifest_v3.bin.zst";
const API_MANIFEST_FILENAME_V2: &str = "api_manifest_v2.bin.zst";
const API_MANIFEST_FILENAME_V1: &str = "api_manifest.bin.zst";

/// An async client for a single Thunderstore community.
///
/// Build one with [`ThunderstoreClient::builder`].
pub struct ThunderstoreClient {
  http: Client,
  package_index_url: String,
  cache_dir: PathBuf,
  progress: Arc<dyn ProgressReporter>,
  download_concurrency: usize,
}

impl ThunderstoreClient {
  /// Returns a builder for configuring a [`ThunderstoreClient`].
  pub fn builder() -> ThunderstoreClientBuilder {
    ThunderstoreClientBuilder::default()
  }

  /// The composed package-index URL this client will query.
  pub fn package_index_url(&self) -> &str {
    &self.package_index_url
  }

  /// The cache directory this client reads from and writes to.
  pub fn cache_dir(&self) -> &Path {
    &self.cache_dir
  }

  /// Retrieves the package index, loading from cache when it is fresh.
  ///
  /// Performs a `HEAD` request and compares the server's `Last-Modified` against
  /// the cached timestamp; if the cache is current it is loaded from disk,
  /// otherwise the index is downloaded and cached.
  pub async fn get_manifest(&self) -> Result<InternedPackageManifest> {
    let last_modified = local_last_modified(&self.cache_dir).await?;
    tracing::info!("Manifest last modified: {}", last_modified);

    if api_manifest_file_exists(&self.cache_dir)
      && network_last_modified(&self.http, &self.package_index_url).await? <= last_modified
    {
      tracing::info!("Loading manifest from cache");

      get_manifest_from_disk(&self.cache_dir).await
    } else {
      tracing::info!("Downloading new manifest");

      get_manifest_from_network_and_cache(
        &self.cache_dir,
        &self.http,
        &self.package_index_url,
        self.progress.as_ref(),
      )
      .await
    }
  }

  /// Forces a network refresh of the package index, bypassing the cache.
  pub async fn refresh_index(&self) -> Result<InternedPackageManifest> {
    get_manifest_from_network_and_cache(
      &self.cache_dir,
      &self.http,
      &self.package_index_url,
      self.progress.as_ref(),
    )
    .await
  }

  /// Downloads multiple package archives concurrently into `cache_dir/downloads`.
  ///
  /// Keys of `urls` are filenames and values are download URLs. Files that
  /// already exist are skipped.
  pub async fn download_files(&self, urls: HashMap<String, String>) -> Result<()> {
    download_files(
      urls,
      &self.cache_dir,
      &self.http,
      self.download_concurrency,
      self.progress.clone(),
    )
    .await
  }
}

/// Builder for [`ThunderstoreClient`].
pub struct ThunderstoreClientBuilder {
  base_url: Option<String>,
  community: Option<String>,
  package_index_url: Option<String>,
  cache_dir: Option<PathBuf>,
  progress: Option<Arc<dyn ProgressReporter>>,
  download_concurrency: usize,
  download_timeout: Duration,
}

impl Default for ThunderstoreClientBuilder {
  fn default() -> Self {
    Self {
      base_url: None,
      community: None,
      package_index_url: None,
      cache_dir: None,
      progress: None,
      download_concurrency: 2,
      download_timeout: Duration::from_secs(60),
    }
  }
}

impl ThunderstoreClientBuilder {
  /// Sets the Thunderstore host (default `https://thunderstore.io`).
  pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
    self.base_url = Some(base_url.into());

    self
  }

  /// Sets the community slug, e.g. `"valheim"` or `"lethal-company"`.
  pub fn community(mut self, community: impl Into<String>) -> Self {
    self.community = Some(community.into());

    self
  }

  /// Overrides the fully composed package-index URL (mainly for tests/mirrors).
  ///
  /// When set, this takes precedence over `base_url`/`community` composition.
  pub fn package_index_url(mut self, url: impl Into<String>) -> Self {
    self.package_index_url = Some(url.into());

    self
  }

  /// Sets the cache directory used for the index and downloads.
  pub fn cache_dir(mut self, cache_dir: impl Into<PathBuf>) -> Self {
    self.cache_dir = Some(cache_dir.into());

    self
  }

  /// Sets the progress reporter (default: [`NoopProgress`]).
  pub fn progress(mut self, progress: Arc<dyn ProgressReporter>) -> Self {
    self.progress = Some(progress);

    self
  }

  /// Sets the maximum number of concurrent downloads (default 2).
  pub fn download_concurrency(mut self, concurrency: usize) -> Self {
    self.download_concurrency = concurrency;

    self
  }

  /// Sets the per-download HTTP timeout (default 60s).
  pub fn download_timeout(mut self, timeout: Duration) -> Self {
    self.download_timeout = timeout;

    self
  }

  /// Builds the [`ThunderstoreClient`].
  ///
  /// # Errors
  ///
  /// Returns an error if `cache_dir` was not set or the HTTP client cannot be
  /// constructed.
  pub fn build(self) -> Result<ThunderstoreClient> {
    let cache_dir = self
      .cache_dir
      .ok_or_else(|| Error::Manifest("cache_dir is required".to_string()))?;

    let package_index_url = match self.package_index_url {
      Some(url) => url,
      None => {
        let base = self.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL);
        let community = self
          .community
          .ok_or_else(|| Error::Manifest("community is required".to_string()))?;

        format!(
          "{}/c/{}/api/v1/package/",
          base.trim_end_matches('/'),
          community
        )
      }
    };

    let http = Client::builder().timeout(self.download_timeout).build()?;
    let progress = self.progress.unwrap_or_else(|| Arc::new(NoopProgress));

    Ok(ThunderstoreClient {
      http,
      package_index_url,
      cache_dir,
      progress,
      download_concurrency: self.download_concurrency,
    })
  }
}

/// Returns the path to the last_modified file in the cache directory.
fn last_modified_path(cache_dir: &Path) -> PathBuf {
  cache_dir.join(LAST_MODIFIED_FILENAME)
}

/// Returns the path to the v3 API manifest file in the cache directory.
fn api_manifest_path_v3(cache_dir: &Path) -> PathBuf {
  cache_dir.join(API_MANIFEST_FILENAME_V3)
}

/// Returns the path to the v2 API manifest file in the cache directory.
fn api_manifest_path_v2(cache_dir: &Path) -> PathBuf {
  cache_dir.join(API_MANIFEST_FILENAME_V2)
}

/// Returns the path to the v1 API manifest file in the cache directory.
fn api_manifest_path_v1(cache_dir: &Path) -> PathBuf {
  cache_dir.join(API_MANIFEST_FILENAME_V1)
}

/// Reads the cached API manifest from disk.
///
/// Attempts to read formats in order: v3 (interned), v2 (SoA), v1 (`Vec<Package>`).
/// When older formats are detected, they are automatically migrated to v3.
async fn get_manifest_from_disk(cache_dir: &Path) -> Result<InternedPackageManifest> {
  let path_v3 = api_manifest_path_v3(cache_dir);
  let path_v2 = api_manifest_path_v2(cache_dir);
  let path_v1 = api_manifest_path_v1(cache_dir);

  if path_v3.exists() {
    tracing::debug!("Loading v3 manifest format");

    let mut file = fs::File::open(&path_v3).await?;
    let mut compressed_data = Vec::new();
    file.read_to_end(&mut compressed_data).await?;

    let decompressed_data = zstd::decode_all(compressed_data.as_slice())
      .map_err(|e| Error::Manifest(format!("Failed to decompress v3 manifest: {}", e)))?;

    let serializable: SerializableInternedManifest = bincode::deserialize(&decompressed_data)
      .map_err(|e| Error::Manifest(format!("Failed to deserialize v3 manifest: {}", e)))?;

    let manifest: InternedPackageManifest = serializable.into();

    manifest
      .validate()
      .map_err(|e| Error::Manifest(format!("V3 manifest validation failed: {}", e)))?;

    return Ok(manifest);
  }

  if path_v2.exists() {
    tracing::info!("Found v2 manifest, migrating to v3 format");

    let mut file = fs::File::open(&path_v2).await?;
    let mut compressed_data = Vec::new();
    file.read_to_end(&mut compressed_data).await?;

    let decompressed_data = zstd::decode_all(compressed_data.as_slice())
      .map_err(|e| Error::Manifest(format!("Failed to decompress v2 manifest: {}", e)))?;

    let v2_manifest: PackageManifest = bincode::deserialize(&decompressed_data)
      .map_err(|e| Error::Manifest(format!("Failed to deserialize v2 manifest: {}", e)))?;

    let manifest: InternedPackageManifest = v2_manifest.into();

    manifest.validate().map_err(|e| {
      Error::Manifest(format!(
        "V3 manifest validation failed during migration: {}",
        e
      ))
    })?;

    let serializable: SerializableInternedManifest = (&manifest).into();
    let binary_data = bincode::serialize(&serializable)
      .map_err(|e| Error::Manifest(format!("Failed to serialize v3 manifest: {}", e)))?;

    write_cache_to_disk(path_v3.clone(), &binary_data, true).await?;

    match tokio::fs::metadata(&path_v3).await {
      Ok(metadata) if metadata.len() > 0 => {
        tracing::info!("V3 manifest written successfully, removing v2");

        if let Err(e) = fs::remove_file(&path_v2).await {
          tracing::warn!(
            "Failed to remove old v2 manifest (keeping as backup): {}",
            e
          );
        }
      }
      Ok(_) => {
        tracing::error!("V3 manifest written but is empty, keeping v2 as backup");
      }
      Err(e) => {
        tracing::error!(
          "Failed to verify v3 manifest write: {}, keeping v2 as backup",
          e
        );
      }
    }

    return Ok(manifest);
  }

  if path_v1.exists() {
    tracing::info!("Found v1 manifest, migrating to v3 format");

    let mut file = fs::File::open(&path_v1).await?;
    let mut compressed_data = Vec::new();
    file.read_to_end(&mut compressed_data).await?;

    let decompressed_data = zstd::decode_all(compressed_data.as_slice())
      .map_err(|e| Error::Manifest(format!("Failed to decompress v1 manifest: {}", e)))?;

    let packages: Vec<Package> = bincode::deserialize(&decompressed_data)
      .map_err(|e| Error::Manifest(format!("Failed to deserialize v1 manifest: {}", e)))?;

    let manifest: InternedPackageManifest = packages.into();

    manifest.validate().map_err(|e| {
      Error::Manifest(format!(
        "V3 manifest validation failed during migration: {}",
        e
      ))
    })?;

    let serializable: SerializableInternedManifest = (&manifest).into();
    let binary_data = bincode::serialize(&serializable)
      .map_err(|e| Error::Manifest(format!("Failed to serialize v3 manifest: {}", e)))?;

    write_cache_to_disk(path_v3.clone(), &binary_data, true).await?;

    match tokio::fs::metadata(&path_v3).await {
      Ok(metadata) if metadata.len() > 0 => {
        tracing::info!("V3 manifest written successfully, removing v1");

        if let Err(e) = fs::remove_file(&path_v1).await {
          tracing::warn!(
            "Failed to remove old v1 manifest (keeping as backup): {}",
            e
          );
        }
      }
      Ok(_) => {
        tracing::error!("V3 manifest written but is empty, keeping v1 as backup");
      }
      Err(e) => {
        tracing::error!(
          "Failed to verify v3 manifest write: {}, keeping v1 as backup",
          e
        );
      }
    }

    return Ok(manifest);
  }

  Err(Error::Manifest("No cached manifest found".to_string()))
}

/// Downloads the manifest from the API server.
async fn get_manifest_from_network(
  http: &Client,
  api_url: &str,
  progress: &dyn ProgressReporter,
) -> Result<(Vec<Package>, DateTime<FixedOffset>)> {
  let handle = progress.begin("Downloading Api Manifest", None);

  let response = http.get(api_url).send().await?;
  let last_modified_str = response
    .headers()
    .get(header::LAST_MODIFIED)
    .ok_or(Error::MissingHeader("Last-Modified".to_string()))?
    .to_str()?;
  let last_modified = DateTime::parse_from_rfc2822(last_modified_str)?;

  let raw_response_data = response.bytes().await?;

  let packages: Vec<Package> = serde_json::from_slice(&raw_response_data)?;

  handle.finish(Some(" Downloaded Api Manifest"));

  Ok((packages, last_modified))
}

/// Downloads the manifest from the network and caches it locally as v3.
async fn get_manifest_from_network_and_cache(
  cache_dir: &Path,
  http: &Client,
  api_url: &str,
  progress: &dyn ProgressReporter,
) -> Result<InternedPackageManifest> {
  let (packages, last_modified_from_network) =
    get_manifest_from_network(http, api_url, progress).await?;

  write_cache_to_disk(
    last_modified_path(cache_dir),
    last_modified_from_network.to_rfc2822().as_bytes(),
    false,
  )
  .await?;

  let manifest: InternedPackageManifest = packages.into();
  let serializable: SerializableInternedManifest = (&manifest).into();
  let binary_data = bincode::serialize(&serializable)
    .map_err(|e| Error::Manifest(format!("Failed to serialize manifest: {}", e)))?;

  write_cache_to_disk(api_manifest_path_v3(cache_dir), &binary_data, true).await?;

  Ok(manifest)
}

/// Retrieves the last modified date from the local cache file.
async fn local_last_modified(cache_dir: &Path) -> Result<DateTime<FixedOffset>> {
  let path = last_modified_path(cache_dir);

  if let Ok(mut file) = fs::File::open(&path).await {
    tracing::info!("Last modified file exists and was opened.");
    let mut contents = String::new();
    file.read_to_string(&mut contents).await?;

    let last_modified = DateTime::parse_from_rfc2822(&contents)?;

    Ok(last_modified)
  } else {
    tracing::info!("Last modified file does not exist and was not opened.");
    let dt = DateTime::from_timestamp(0, 0).unwrap().naive_utc();
    let offset = FixedOffset::east_opt(0).unwrap();

    let last_modified = DateTime::<FixedOffset>::from_naive_utc_and_offset(dt, offset);

    Ok(last_modified)
  }
}

/// Retrieves the last modified date from the API server via a `HEAD` request.
async fn network_last_modified(http: &Client, api_url: &str) -> Result<DateTime<FixedOffset>> {
  let response = http.head(api_url).send().await?;

  let last_modified = response
    .headers()
    .get(header::LAST_MODIFIED)
    .ok_or(Error::MissingHeader(
      "Last-Modified for API manifest head request".to_string(),
    ))?;
  let last_modified_date = DateTime::parse_from_rfc2822(last_modified.to_str()?)?;

  Ok(last_modified_date)
}

/// Checks for the existence of any version of the cached manifest file.
fn api_manifest_file_exists(cache_dir: &Path) -> bool {
  api_manifest_path_v3(cache_dir).exists()
    || api_manifest_path_v2(cache_dir).exists()
    || api_manifest_path_v1(cache_dir).exists()
}

/// Writes data to a cache file on disk, optionally compressing with zstd level 9.
async fn write_cache_to_disk<T: AsRef<Path>>(
  path: T,
  contents: &[u8],
  use_compression: bool,
) -> Result<()> {
  if let Some(parent) = path.as_ref().parent() {
    fs::create_dir_all(parent).await?;
  }

  if !path.as_ref().exists() {
    fs::File::create(&path).await?;
  }

  let mut file = fs::OpenOptions::new()
    .write(true)
    .truncate(true)
    .open(&path)
    .await?;

  if use_compression {
    let compressed_data = zstd::encode_all(contents, 9)
      .map_err(|e| Error::Manifest(format!("Failed to compress data: {}", e)))?;

    file.write_all(&compressed_data).await?;
    file.flush().await?;
  } else {
    file.write_all(contents).await?;
    file.flush().await?;
  }

  Ok(())
}

/// Downloads multiple files concurrently with progress indicators.
async fn download_files(
  urls: HashMap<String, String>,
  cache_dir: &Path,
  http: &Client,
  concurrency: usize,
  progress: Arc<dyn ProgressReporter>,
) -> Result<()> {
  if urls.is_empty() {
    tracing::debug!("No files to download");

    return Ok(());
  }

  tracing::debug!("Processing {} mods", urls.len());

  let futures: FuturesUnordered<_> = urls
    .into_iter()
    .map(|(archive_filename, url)| {
      let client = http.clone();
      let cache_dir = cache_dir.to_path_buf();
      let progress = progress.clone();

      tokio::spawn(async move {
        download_file(
          &client,
          &url,
          &archive_filename,
          &cache_dir,
          progress.as_ref(),
        )
        .await
      })
    })
    .collect();

  let responses = futures::stream::iter(futures)
    .buffer_unordered(concurrency)
    .collect::<Vec<_>>()
    .await;

  for response in responses {
    match response {
      Ok(Ok(_)) => {}
      Ok(Err(err)) => {
        tracing::error!("Download error: {:?}", err);
      }
      Err(err) => {
        return Err(Error::TaskFailed(err));
      }
    }
  }

  Ok(())
}

/// Downloads a single file from a URL with a progress indicator.
async fn download_file(
  client: &Client,
  url: &str,
  filename: &str,
  cache_dir: &Path,
  progress: &dyn ProgressReporter,
) -> Result<()> {
  let downloads_directory = cache_dir.join("downloads");
  let file_path = downloads_directory.join(filename);

  tokio::fs::DirBuilder::new()
    .recursive(true)
    .create(&downloads_directory)
    .await?;

  if file_path.exists() {
    tracing::debug!("{} already exists, skipping download", filename);

    return Ok(());
  }

  let response = client.get(url).send().await?;
  let content_length = response
    .headers()
    .get(header::CONTENT_LENGTH)
    .ok_or(Error::MissingHeader(format!(
      "Content-Length header for {}",
      url
    )))?
    .to_str()?
    .parse::<u64>()?;
  let mut response_data = response.bytes_stream();

  let handle = progress.begin(filename, Some(content_length));

  let mut file = tokio::fs::File::create(file_path).await?;

  while let Some(result) = response_data.next().await {
    let chunk = result?;
    file.write_all(&chunk).await?;
    handle.inc(chunk.len() as u64);
  }

  file.flush().await?;
  file.sync_all().await?;

  handle.finish(None);

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::progress::NoopProgress;
  use mockito::Server;
  use std::fs::File;
  use std::io::Write;
  use tempfile::tempdir;
  use time::OffsetDateTime;
  use tokio::runtime::Runtime;

  fn test_http() -> Client {
    Client::builder()
      .timeout(Duration::from_secs(60))
      .build()
      .unwrap()
  }

  #[test]
  fn test_api_manifest_file_exists() {
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    assert!(!api_manifest_file_exists(cache_dir));

    let _ = File::create(cache_dir.join(API_MANIFEST_FILENAME_V3)).unwrap();

    assert!(api_manifest_file_exists(cache_dir));
  }

  #[test]
  fn test_path_construction() {
    let cache_dir = Path::new("/some/cache/dir");

    let last_modified = last_modified_path(cache_dir);
    let api_manifest = api_manifest_path_v3(cache_dir);

    assert_eq!(last_modified, cache_dir.join(LAST_MODIFIED_FILENAME));
    assert_eq!(api_manifest, cache_dir.join(API_MANIFEST_FILENAME_V3));
  }

  #[test]
  fn test_builder_composes_url() {
    let temp_dir = tempdir().unwrap();

    let client = ThunderstoreClient::builder()
      .base_url("https://stacklands.thunderstore.io")
      .community("valheim")
      .cache_dir(temp_dir.path())
      .build()
      .unwrap();

    assert_eq!(
      client.package_index_url(),
      "https://stacklands.thunderstore.io/c/valheim/api/v1/package/"
    );
  }

  #[test]
  fn test_builder_requires_cache_dir() {
    let result = ThunderstoreClient::builder().community("valheim").build();

    assert!(result.is_err());
  }

  #[test]
  fn test_write_cache_to_disk() {
    let temp_dir = tempdir().unwrap();
    let cache_path = temp_dir.path().join("test_cache.txt");
    let test_data = b"Test cache data";

    let rt = Runtime::new().unwrap();
    let result = rt.block_on(write_cache_to_disk(&cache_path, test_data, false));

    assert!(result.is_ok());
    assert!(cache_path.exists());

    let content = std::fs::read(&cache_path).unwrap();
    assert_eq!(content, test_data);

    let new_data = b"Updated cache data";
    let result = rt.block_on(write_cache_to_disk(&cache_path, new_data, false));

    assert!(result.is_ok());
    let content = std::fs::read(&cache_path).unwrap();

    assert_eq!(content, new_data);
  }

  #[test]
  fn test_write_cache_to_disk_creates_directories() {
    let temp_dir = tempdir().unwrap();

    let nonexistent_subdir = temp_dir.path().join("subdir1/subdir2");
    let cache_path = nonexistent_subdir.join("test_cache.txt");

    assert!(!nonexistent_subdir.exists());
    assert!(!cache_path.exists());

    let test_data = b"Test cache data";
    let rt = Runtime::new().unwrap();

    let result = rt.block_on(write_cache_to_disk(&cache_path, test_data, false));
    assert!(result.is_ok());

    assert!(
      nonexistent_subdir.exists(),
      "Directory structure should be created"
    );
    assert!(cache_path.exists(), "File should be created");

    let content = std::fs::read(&cache_path).unwrap();
    assert_eq!(content, test_data, "File should contain the expected data");
  }

  #[test]
  fn test_write_cache_to_disk_with_compression() {
    let temp_dir = tempdir().unwrap();
    let api_manifest_path = temp_dir.path().join("test_compressed.bin");

    let package = sample_package("TestMod", "TestOwner-TestMod", "TestOwner");

    let packages = vec![package];
    let manifest: PackageManifest = packages.into();
    let binary_data = bincode::serialize(&manifest).unwrap();

    let rt = Runtime::new().unwrap();
    let result = rt.block_on(write_cache_to_disk(&api_manifest_path, &binary_data, true));

    assert!(result.is_ok());
    assert!(api_manifest_path.exists());

    let compressed_data = std::fs::read(&api_manifest_path).unwrap();
    let decompressed_data = zstd::decode_all(compressed_data.as_slice()).unwrap();
    let decoded_manifest: PackageManifest = bincode::deserialize(&decompressed_data).unwrap();

    assert_eq!(decoded_manifest.len(), 1);
    assert_eq!(
      decoded_manifest.full_names[0],
      Some("TestOwner-TestMod".to_string())
    );
  }

  #[test]
  fn test_local_last_modified() {
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();
    let rt = Runtime::new().unwrap();

    {
      let result = rt.block_on(local_last_modified(cache_dir));

      assert!(result.is_ok());
    }

    let test_date = "Wed, 21 Feb 2024 15:30:45 GMT";
    let mut file = File::create(cache_dir.join(LAST_MODIFIED_FILENAME)).unwrap();
    file.write_all(test_date.as_bytes()).unwrap();

    {
      let result = rt.block_on(local_last_modified(cache_dir));

      assert!(result.is_ok());
    }
  }

  #[test]
  fn test_get_manifest_from_disk() {
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    let package = sample_package("ModA", "Owner-ModA", "Owner");

    let packages = vec![package];
    let interned_manifest: InternedPackageManifest = packages.into();
    let serializable: SerializableInternedManifest = (&interned_manifest).into();
    let binary_data = bincode::serialize(&serializable).unwrap();
    let compressed_data = zstd::encode_all(binary_data.as_slice(), 9).unwrap();
    let mut file = File::create(cache_dir.join(API_MANIFEST_FILENAME_V3)).unwrap();
    file.write_all(&compressed_data).unwrap();

    let rt = Runtime::new().unwrap();
    let manifest = rt.block_on(get_manifest_from_disk(cache_dir)).unwrap();

    assert_eq!(manifest.len(), 1);
    assert_eq!(
      manifest.resolve_full_name_at(0),
      Some("Owner-ModA".to_string())
    );
  }

  #[test]
  fn test_get_manifest_from_disk_error() {
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    let mut file = File::create(cache_dir.join(API_MANIFEST_FILENAME_V3)).unwrap();
    file
      .write_all(b"This is not valid compressed data")
      .unwrap();

    let rt = Runtime::new().unwrap();
    let result = rt.block_on(get_manifest_from_disk(cache_dir));

    assert!(result.is_err());
  }

  #[test]
  fn test_network_last_modified() {
    let mut server = Server::new();
    let test_date = "Wed, 21 Feb 2024 15:30:45 GMT";
    let mock = server
      .mock("HEAD", "/c/valheim/api/v1/package/")
      .with_status(200)
      .with_header("Last-Modified", test_date)
      .create();
    let api_url = format!("{}/c/valheim/api/v1/package/", server.url());
    let rt = Runtime::new().unwrap();

    let result = rt.block_on(network_last_modified(&test_http(), &api_url));

    assert!(result.is_ok());
    if let Ok(parsed_date) = result {
      let expected_date = DateTime::parse_from_rfc2822(test_date).unwrap();
      assert_eq!(parsed_date, expected_date);
    }

    mock.assert();
  }

  #[test]
  fn test_get_manifest_from_network() {
    let mut server = Server::new();
    let test_date = "Wed, 21 Feb 2024 15:30:45 GMT";

    let mock = server
      .mock("GET", "/c/valheim/api/v1/package/")
      .with_status(200)
      .with_header("Content-Type", "application/json")
      .with_header("Last-Modified", test_date)
      .with_body(sample_manifest_json())
      .create();

    let api_url = format!("{}/c/valheim/api/v1/package/", server.url());

    let rt = Runtime::new().unwrap();
    let result = rt.block_on(get_manifest_from_network(
      &test_http(),
      &api_url,
      &NoopProgress,
    ));

    assert!(result.is_ok());
    if let Ok((packages, last_modified)) = result {
      assert_eq!(packages.len(), 1);
      assert_eq!(packages[0].full_name, Some("Owner-ModA".to_string()));
      let expected_date = DateTime::parse_from_rfc2822(test_date).unwrap();
      assert_eq!(last_modified, expected_date);
    }

    mock.assert();
  }

  #[test]
  fn test_get_manifest() {
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    let package = sample_package("CachedMod", "CachedOwner-CachedMod", "CachedOwner");

    let packages = vec![package];
    let interned_manifest: InternedPackageManifest = packages.into();
    let serializable: SerializableInternedManifest = (&interned_manifest).into();
    let binary_data = bincode::serialize(&serializable).unwrap();
    let compressed_data = zstd::encode_all(binary_data.as_slice(), 9).unwrap();

    let mut file = File::create(cache_dir.join(API_MANIFEST_FILENAME_V3)).unwrap();
    file.write_all(&compressed_data).unwrap();

    let now = chrono::Utc::now().with_timezone(&chrono::FixedOffset::east_opt(0).unwrap());
    let recent_date = now.to_rfc2822();
    let mut file = File::create(cache_dir.join(LAST_MODIFIED_FILENAME)).unwrap();
    file.write_all(recent_date.as_bytes()).unwrap();

    let client = ThunderstoreClient::builder()
      .community("valheim")
      .cache_dir(cache_dir)
      .build()
      .unwrap();

    let rt = Runtime::new().unwrap();
    let result = rt.block_on(client.get_manifest());

    assert!(result.is_ok());
  }

  #[test]
  fn test_manifest_v1_to_v2_migration() {
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    let package = sample_package("OldMod", "OldOwner-OldMod", "OldOwner");

    let packages = vec![package];
    let binary_data = bincode::serialize(&packages).unwrap();
    let compressed_data = zstd::encode_all(binary_data.as_slice(), 9).unwrap();

    let v1_path = cache_dir.join(API_MANIFEST_FILENAME_V1);
    let mut file = File::create(&v1_path).unwrap();
    file.write_all(&compressed_data).unwrap();

    let v3_path = cache_dir.join(API_MANIFEST_FILENAME_V3);
    assert!(!v3_path.exists());
    assert!(v1_path.exists());

    let rt = Runtime::new().unwrap();
    let result = rt.block_on(get_manifest_from_disk(cache_dir));

    assert!(result.is_ok());
    let manifest = result.unwrap();
    assert_eq!(manifest.len(), 1);
    assert_eq!(
      manifest.resolve_full_name_at(0),
      Some("OldOwner-OldMod".to_string())
    );

    assert!(v3_path.exists(), "v3 manifest should be created");
    assert!(!v1_path.exists(), "v1 manifest should be removed");
  }

  #[test]
  fn test_get_manifest_from_network_and_cache() {
    let mut server = Server::new();
    let test_date = "Wed, 21 Feb 2024 15:30:45 GMT";

    let mock = server
      .mock("GET", "/c/valheim/api/v1/package/")
      .with_status(200)
      .with_header("Content-Type", "application/json")
      .with_header("Last-Modified", test_date)
      .with_body(sample_manifest_json())
      .create();

    let api_url = format!("{}/c/valheim/api/v1/package/", server.url());
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    let rt = Runtime::new().unwrap();
    let result = rt.block_on(get_manifest_from_network_and_cache(
      cache_dir,
      &test_http(),
      &api_url,
      &NoopProgress,
    ));

    assert!(result.is_ok());
    if let Ok(manifest) = result {
      assert_eq!(manifest.len(), 1);
      assert_eq!(
        manifest.resolve_full_name_at(0),
        Some("Owner-ModA".to_string())
      );

      assert!(last_modified_path(cache_dir).exists());
      assert!(api_manifest_path_v3(cache_dir).exists());

      let last_mod_content = std::fs::read_to_string(last_modified_path(cache_dir)).unwrap();
      assert!(last_mod_content.contains("21 Feb 2024 15:30:45"));
    }

    mock.assert();
  }

  #[test]
  fn test_download_file() {
    let mut server = Server::new();
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    let test_data = b"This is test file content";
    let content_length = test_data.len();

    let _mock = server
      .mock("GET", "/test-file.zip")
      .with_status(200)
      .with_header("Content-Type", "application/zip")
      .with_header("Content-Length", &content_length.to_string())
      .with_body(test_data)
      .create();

    let file_url = format!("{}/test-file.zip", server.url());
    let filename = "test-file.zip".to_string();

    let rt = Runtime::new().unwrap();
    let client = test_http();

    let result = rt.block_on(download_file(
      &client,
      &file_url,
      &filename,
      cache_dir,
      &NoopProgress,
    ));

    assert!(result.is_ok());

    let downloads_dir = cache_dir.join("downloads");
    assert!(downloads_dir.exists());

    let downloaded_file = downloads_dir.join(&filename);
    assert!(downloaded_file.exists());

    let file_content = std::fs::read(&downloaded_file).unwrap();
    assert_eq!(file_content, test_data);

    let result2 = rt.block_on(download_file(
      &client,
      &file_url,
      &filename,
      cache_dir,
      &NoopProgress,
    ));

    assert!(result2.is_ok());
  }

  #[test]
  fn test_download_file_missing_header() {
    let mut server = Server::new();
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    let test_data = b"This is test file content";

    let mock = server
      .mock("GET", "/test-file.zip")
      .with_status(200)
      .with_header("Content-Type", "application/zip")
      .with_body(test_data)
      .create();

    let file_url = format!("{}/test-file.zip", server.url());
    let filename = "test-file-no-header.zip".to_string();

    let rt = Runtime::new().unwrap();
    let client = test_http();

    let _result = rt.block_on(download_file(
      &client,
      &file_url,
      &filename,
      cache_dir,
      &NoopProgress,
    ));

    mock.assert();
  }

  #[test]
  fn test_download_files() {
    let mut server = Server::new();
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    let test_data1 = b"This is test file 1 content";
    let content_length1 = test_data1.len();

    let test_data2 = b"This is test file 2 content - longer content";
    let content_length2 = test_data2.len();

    let mock1 = server
      .mock("GET", "/file1.zip")
      .with_status(200)
      .with_header("Content-Type", "application/zip")
      .with_header("Content-Length", &content_length1.to_string())
      .with_body(test_data1)
      .create();

    let mock2 = server
      .mock("GET", "/file2.zip")
      .with_status(200)
      .with_header("Content-Type", "application/zip")
      .with_header("Content-Length", &content_length2.to_string())
      .with_body(test_data2)
      .create();

    let mut urls = HashMap::new();
    urls.insert(
      "file1.zip".to_string(),
      format!("{}/file1.zip", server.url()),
    );
    urls.insert(
      "file2.zip".to_string(),
      format!("{}/file2.zip", server.url()),
    );

    let rt = Runtime::new().unwrap();
    let result = rt.block_on(download_files(
      urls,
      cache_dir,
      &test_http(),
      2,
      Arc::new(NoopProgress),
    ));

    assert!(result.is_ok());

    let downloads_dir = cache_dir.join("downloads");
    assert!(downloads_dir.exists());

    let file1 = downloads_dir.join("file1.zip");
    let file2 = downloads_dir.join("file2.zip");
    assert!(file1.exists());
    assert!(file2.exists());

    let file1_content = std::fs::read(&file1).unwrap();
    let file2_content = std::fs::read(&file2).unwrap();
    assert_eq!(file1_content, test_data1);
    assert_eq!(file2_content, test_data2);

    mock1.assert();
    mock2.assert();
  }

  #[test]
  fn test_download_files_empty() {
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    let urls = HashMap::new();

    let rt = Runtime::new().unwrap();
    let result = rt.block_on(download_files(
      urls,
      cache_dir,
      &test_http(),
      2,
      Arc::new(NoopProgress),
    ));

    assert!(result.is_ok());
  }

  fn sample_package(name: &str, full_name: &str, owner: &str) -> Package {
    Package {
      name: Some(name.to_string()),
      full_name: Some(full_name.to_string()),
      owner: Some(owner.to_string()),
      package_url: Some(format!("https://example.com/{}", name)),
      date_created: OffsetDateTime::now_utc(),
      date_updated: OffsetDateTime::now_utc(),
      uuid4: Some("test-uuid".to_string()),
      rating_score: Some(5),
      is_pinned: Some(false),
      is_deprecated: Some(false),
      has_nsfw_content: Some(false),
      categories: vec!["test".to_string()],
      versions: vec![],
    }
  }

  fn sample_manifest_json() -> &'static str {
    r#"[
      {
        "name": "ModA",
        "full_name": "Owner-ModA",
        "owner": "Owner",
        "package_url": "https://example.com/mods/ModA",
        "date_created": "2024-01-01T12:00:00Z",
        "date_updated": "2024-01-02T12:00:00Z",
        "uuid4": "test-uuid",
        "rating_score": 5,
        "is_pinned": false,
        "is_deprecated": false,
        "has_nsfw_content": false,
        "categories": ["category1"],
        "versions": [
          {
            "name": "ModA",
            "full_name": "Owner-ModA",
            "description": "Test description",
            "icon": "icon.png",
            "version_number": "1.0.0",
            "dependencies": [],
            "download_url": "https://example.com/mods/ModA/download",
            "downloads": 100,
            "date_created": "2024-01-01T12:00:00Z",
            "website_url": "https://example.com",
            "is_active": true,
            "uuid4": "test-version-uuid",
            "file_size": 1024
          }
        ]
      }
    ]"#
  }
}
