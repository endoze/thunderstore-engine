//! Async Thunderstore API client and package-index cache.
//!
//! [`ThunderstoreClient`](crate::client::ThunderstoreClient) fetches a community's package index
//! (`/c/<community>/api/v1/package/`) and caches it on disk compressed with
//! `zstd` + `bincode`, keyed by a `Last-Modified` freshness check. It also
//! downloads package archives with bounded concurrency. All progress is
//! reported through [`crate::progress`] so the crate stays UI-agnostic.
//!
//! API reference: <https://thunderstore.io/api/docs/>

#![deny(missing_docs)]

use crate::error::{Error, Result};
use crate::models::{Package, PackageIndex, SerializedPackageIndex};
use crate::progress::{NoopProgress, ProgressReporter};
use crate::retry::{RetryPolicy, retry_http};
use crate::util::{SizeCap, io_context};

use chrono::prelude::*;
use futures::stream::StreamExt;
use reqwest::Client;
use reqwest::header;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Default Thunderstore host used when no `base_url` is supplied.
const DEFAULT_BASE_URL: &str = "https://thunderstore.io";
const LAST_MODIFIED_FILENAME: &str = "last_modified";
const API_MANIFEST_FILENAME: &str = "api_manifest.bin.zst";

/// Subdirectory under `cache_dir` that holds the per-index (per-community) index
/// cache. Each community's manifest lives in its own subfolder keyed by a CRC32 of
/// the resolved index URL, so honest communities sharing one `cache_dir` avoid
/// accidentally reading each other's index. CRC32 is a non-cryptographic checksum,
/// so this is not a security boundary: it does not defend against attacker-chosen
/// `base_url`/`community` values crafted to collide.
const INDEX_CACHE_SUBDIR: &str = "index";

/// Default per-download size ceiling. Generous for real mod and texture packs
/// while still rejecting a runaway body that would otherwise fill the disk within
/// the request timeout. Tune it with
/// [`ThunderstoreClientBuilder::download_max_bytes`].
const DEFAULT_MAX_DOWNLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Default ceiling on the package-index response body (raw JSON) read into memory.
/// A large community index can run to hundreds of megabytes, so this default is
/// deliberately generous: it exists only to reject an absurd or runaway body from a
/// compromised or misbehaving index host, not to bound normal use. Raise it with
/// [`ThunderstoreClientBuilder::manifest_max_bytes`], or pass [`u64::MAX`] to
/// disable the cap.
const DEFAULT_MAX_MANIFEST_BYTES: u64 = 512 * 1024 * 1024;

/// Fallback ceiling on the *decompressed* size of a cached index read back from
/// disk, used only when neither the read cap nor the download cap is customised.
///
/// The interned-`bincode` cache is smaller than the JSON it was built from, so a
/// manifest that passes the download cap always fits under an equal read cap. To
/// make that guarantee automatic — so a manifest that downloads successfully can
/// never become permanently unreadable from cache — the read cap *defaults to
/// whatever the download cap is set to* (see [`ThunderstoreClientBuilder::build`]);
/// this const is only the last-resort fallback. Override it with
/// [`ThunderstoreClientBuilder::manifest_max_decompressed_bytes`].
///
/// Security: this caps how many bytes are inflated from a `zstd`/`bincode` cache
/// (e.g. one planted in a shared cache dir) before decoding begins. Decoding is
/// separately bounded by [`MAX_DECODE_ALLOC`], so a crafted high-ratio file cannot
/// exhaust memory even under a generous cap.
const DEFAULT_MAX_MANIFEST_DECOMPRESSED_BYTES: usize = 512 * 1024 * 1024;

/// Compile-time ceiling on the total allocation `bincode`'s native decoder will
/// commit while decoding a cached index. Because the cache uses native `Decode`,
/// every container claims `len * size_of` against this budget up front, so a
/// crafted high-ratio cache cannot amplify a small file into an unbounded
/// allocation — decode errors out instead. This is a fixed structural bound that,
/// unlike the (configurable) decompressed-size cap, never scales up with the
/// download cap, and it sits far above any real community's cache so it never
/// rejects a legitimate one.
const MAX_DECODE_ALLOC: usize = 1024 * 1024 * 1024;

/// Ceiling on the `last_modified` freshness marker read from disk. The marker is
/// a short RFC 2822 date; this bounds allocation against an oversized file
/// planted in a shared cache dir.
const MAX_LAST_MODIFIED_BYTES: u64 = 4096;

/// Thunderstore's profile-sharing endpoints. The path is `legacyprofile` upstream
/// (that is the current endpoint's name, not a deprecation marker).
const PROFILE_API_PATH: &str = "api/experimental/legacyprofile";

/// Ceiling on a profile-code response body read into memory. A shared profile is
/// capped at 20 MB of zip upstream, which is ~27 MB base64; this bounds a
/// misbehaving or hostile host well above that.
const MAX_PROFILE_CODE_BYTES: u64 = 64 * 1024 * 1024;

/// An async client for a single Thunderstore community.
///
/// Build one with [`ThunderstoreClient::builder`].
pub struct ThunderstoreClient {
  http: Client,
  package_index_url: String,
  base_url: String,
  cache_dir: PathBuf,
  index_cache_key: String,
  progress: Arc<dyn ProgressReporter>,
  download_concurrency: usize,
  max_download_bytes: Option<u64>,
  max_manifest_bytes: Option<u64>,
  max_manifest_decompressed_bytes: usize,
  retry: RetryPolicy,
}

impl ThunderstoreClient {
  /// Returns a builder for configuring a [`ThunderstoreClient`].
  ///
  /// `cache_dir` is the only required setting. Nothing is read, written, or sent
  /// over the network until a method such as
  /// [`get_manifest`](Self::get_manifest) is awaited, so building is cheap and
  /// infallible apart from configuration validation.
  ///
  /// # Examples
  ///
  /// ```
  /// use thunderstore_engine::client::ThunderstoreClient;
  ///
  /// let client = ThunderstoreClient::builder()
  ///   .base_url("https://thunderstore.io")
  ///   .community("valheim")
  ///   .cache_dir("/var/cache/my-mod-manager")
  ///   .build()
  ///   .expect("base_url + community compose a valid index URL");
  ///
  /// assert_eq!(client.base_url(), "https://thunderstore.io");
  /// assert_eq!(
  ///   client.package_index_url(),
  ///   "https://thunderstore.io/c/valheim/api/v1/package/"
  /// );
  /// ```
  pub fn builder() -> ThunderstoreClientBuilder {
    ThunderstoreClientBuilder::default()
  }

  /// The composed package-index URL this client will query.
  pub fn package_index_url(&self) -> &str {
    &self.package_index_url
  }

  /// The Thunderstore host this client targets, with any trailing slash trimmed.
  pub fn base_url(&self) -> &str {
    &self.base_url
  }

  /// The cache directory this client reads from and writes to.
  pub fn cache_dir(&self) -> &Path {
    &self.cache_dir
  }

  /// The per-community subdirectory of `cache_dir` that holds this client's
  /// cached package index, namespaced by a CRC32 of the resolved index URL so two
  /// honest communities sharing one `cache_dir` avoid accidental collisions. The
  /// CRC32 key guards against accidental clashes only, not attacker-chosen
  /// `base_url`/`community` values crafted to collide, so it is not a security
  /// boundary.
  fn index_cache_dir(&self) -> PathBuf {
    self
      .cache_dir
      .join(INDEX_CACHE_SUBDIR)
      .join(&self.index_cache_key)
  }

  /// Retrieves the package index, loading from cache when it is fresh.
  ///
  /// Performs a `HEAD` request and compares the server's `Last-Modified` against
  /// the cached timestamp; if the cache is current it is loaded from disk,
  /// otherwise the index is downloaded and cached.
  ///
  /// # Errors
  ///
  /// A failed freshness check is **not** an error on its own: it is logged and a
  /// readable cache is served instead, so this succeeds offline once the index
  /// has been fetched at least once. Errors therefore mean no usable cache *and*
  /// a failed fetch:
  ///
  /// - [`Error::Http`] if the download fails or returns a non-success status.
  /// - [`Error::SizeLimitExceeded`] if the response exceeds
  ///   [`manifest_max_bytes`](ThunderstoreClientBuilder::manifest_max_bytes), or
  ///   the cached copy exceeds
  ///   [`manifest_max_decompressed_bytes`](ThunderstoreClientBuilder::manifest_max_decompressed_bytes).
  /// - [`Error::Json`] if the index is not valid Thunderstore package JSON.
  /// - [`Error::Io`] if the cache directory cannot be created or written.
  pub async fn get_manifest(&self) -> Result<PackageIndex> {
    let index_dir = self.index_cache_dir();
    let last_modified = local_last_modified(&index_dir).await?;
    tracing::info!("Manifest last modified: {}", last_modified);

    if api_manifest_file_exists(&index_dir) {
      match network_last_modified(&self.http, &self.package_index_url).await {
        Ok(network) if is_fresh(network, last_modified) => {
          tracing::info!("Loading manifest from cache");

          if let Some(manifest) =
            load_cached_index(&index_dir, self.max_manifest_decompressed_bytes).await
          {
            return Ok(manifest);
          }
        }
        Ok(_) => tracing::info!("Cache is stale; downloading new manifest"),
        Err(err) => {
          tracing::warn!("Freshness check failed ({}); serving cached manifest", err);

          if let Some(manifest) =
            load_cached_index(&index_dir, self.max_manifest_decompressed_bytes).await
          {
            return Ok(manifest);
          }
        }
      }
    }

    tracing::info!("Downloading new manifest");

    get_manifest_from_network_and_cache(
      &index_dir,
      &self.http,
      &self.package_index_url,
      self.max_manifest_bytes,
      &self.retry,
      self.progress.as_ref(),
    )
    .await
  }

  /// Forces a network refresh of the package index, bypassing the cache.
  ///
  /// # Errors
  ///
  /// The same set as [`get_manifest`](Self::get_manifest), minus the cache
  /// fallback: because this always goes to the network, any network failure is
  /// fatal here even when a perfectly good cached index exists.
  pub async fn refresh_index(&self) -> Result<PackageIndex> {
    get_manifest_from_network_and_cache(
      &self.index_cache_dir(),
      &self.http,
      &self.package_index_url,
      self.max_manifest_bytes,
      &self.retry,
      self.progress.as_ref(),
    )
    .await
  }

  /// Downloads multiple package archives concurrently into `cache_dir/downloads`.
  ///
  /// Keys of `urls` are filenames and values are download URLs. Files that
  /// already exist are skipped. Concurrency is bounded by the configured
  /// `download_concurrency`, and each archive is bounded by the configured
  /// `download_max_bytes`. Each archive is written atomically (temp file +
  /// rename) after its full `Content-Length` is received. Individual download
  /// failures are logged and, if any occurred, surfaced as an aggregate error
  /// rather than silently ignored.
  ///
  /// # Errors
  ///
  /// Returns a single [`Error::Download`] carrying how many of how many files
  /// failed plus the first failure's message. **This is all-or-nothing only in
  /// its reporting, not its effects**: files that succeeded are still on disk, so
  /// a caller that retries should expect those to be skipped as already present.
  /// The underlying per-file causes are [`Error::Http`],
  /// [`Error::SizeLimitExceeded`] (archive over
  /// [`download_max_bytes`](ThunderstoreClientBuilder::download_max_bytes)),
  /// [`Error::IncompleteDownload`] (body shorter than its `Content-Length`), and
  /// [`Error::Io`].
  pub async fn download_files(&self, urls: HashMap<String, String>) -> Result<()> {
    download_files(
      urls,
      &self.cache_dir,
      &self.http,
      self.download_concurrency,
      self.max_download_bytes,
      self.retry,
      self.progress.clone(),
    )
    .await
  }

  /// Uploads a profile-code payload and returns the server-issued key.
  ///
  /// `payload` is sent verbatim as `application/octet-stream`; composing it (the
  /// `#r2modman` prefix and base64 body) belongs to
  /// [`crate::profile::portability`]. Transient failures are retried per the
  /// client's [`RetryPolicy`].
  ///
  /// # Errors
  ///
  /// - [`Error::Http`] if the upload fails or the server returns a non-success
  ///   status, after the retry policy is exhausted.
  /// - [`Error::SizeLimitExceeded`] if the response body is implausibly large for
  ///   a profile key.
  /// - [`Error::Json`] if the response is not the expected `{ "key": ... }` shape.
  pub async fn create_profile_code(&self, payload: &[u8]) -> Result<String> {
    let url = format!("{}/{}/create/", self.base_url, PROFILE_API_PATH);

    let key = retry_http(&self.retry, || async {
      let response = self
        .http
        .post(&url)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(payload.to_vec())
        .send()
        .await?
        .error_for_status()?;

      let body = read_capped_body(response, Some(MAX_PROFILE_CODE_BYTES)).await?;
      let parsed: ProfileCodeResponse = serde_json::from_slice(&body)?;

      Ok(parsed.key)
    })
    .await?;

    Ok(key)
  }

  /// Downloads a shared profile by its code, returning the raw response body.
  ///
  /// Validating the `#r2modman` prefix and decoding the base64 body belongs to
  /// [`crate::profile::portability`].
  ///
  /// # Errors
  ///
  /// - [`Error::Config`] if `code` is empty or contains anything outside ASCII
  ///   alphanumerics, `-`, and `_`. This is checked before any request, so a
  ///   malformed code never reaches the network and can never be smuggled into
  ///   the request path.
  /// - [`Error::Http`] if the download fails or the code is unknown to the
  ///   server, after the retry policy is exhausted.
  /// - [`Error::SizeLimitExceeded`] if the shared profile exceeds the response
  ///   cap.
  pub async fn fetch_profile_code(&self, code: &str) -> Result<Vec<u8>> {
    validate_profile_code(code)?;

    let url = format!("{}/{}/get/{}/", self.base_url, PROFILE_API_PATH, code);

    let body = retry_http(&self.retry, || async {
      let response = self.http.get(&url).send().await?.error_for_status()?;

      read_capped_body(response, Some(MAX_PROFILE_CODE_BYTES)).await
    })
    .await?;

    Ok(body)
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
  max_download_bytes: Option<u64>,
  max_manifest_bytes: Option<u64>,
  max_manifest_decompressed_bytes: Option<u64>,
  retry: RetryPolicy,
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
      max_download_bytes: Some(DEFAULT_MAX_DOWNLOAD_BYTES),
      max_manifest_bytes: Some(DEFAULT_MAX_MANIFEST_BYTES),
      // `None` means "track the download cap" (resolved in `build`), so a manifest
      // that passes the download cap is always readable back from cache.
      max_manifest_decompressed_bytes: None,
      retry: RetryPolicy::default(),
    }
  }
}

impl ThunderstoreClientBuilder {
  /// Sets the Thunderstore host (default `https://thunderstore.io`).
  pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
    self.base_url = Some(base_url.into());

    self
  }

  /// Sets the community slug — the lowercase, hyphenated identifier in a
  /// community's Thunderstore URL.
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

  /// Sets the network timeout (default 60s) applied as both the connection
  /// timeout and the per-read (idle) timeout — *not* a deadline on the whole
  /// request. A large archive can therefore take arbitrarily long to download as
  /// long as data keeps flowing, while a stalled connection still fails promptly.
  pub fn download_timeout(mut self, timeout: Duration) -> Self {
    self.download_timeout = timeout;

    self
  }

  /// Sets the maximum size, in bytes, of the package-index response body (raw
  /// JSON) read into memory. The default is a generous ceiling that accommodates
  /// large community indices; a response whose declared `Content-Length` or actual
  /// streamed size exceeds it is rejected, bounding memory against an oversized
  /// index from a compromised or misbehaving host. Pass [`u64::MAX`] to
  /// effectively disable the cap.
  ///
  /// Unless [`manifest_max_decompressed_bytes`](Self::manifest_max_decompressed_bytes)
  /// is also set, the cache read-back cap tracks this value, so raising this alone
  /// keeps a downloaded manifest readable back from cache.
  pub fn manifest_max_bytes(mut self, max_bytes: u64) -> Self {
    self.max_manifest_bytes = Some(max_bytes);

    self
  }

  /// Sets the maximum *decompressed* size, in bytes, of the on-disk index cache
  /// permitted when reading it back.
  ///
  /// By default (when this is left unset) the read cap tracks
  /// [`manifest_max_bytes`](Self::manifest_max_bytes), i.e. the download cap. The
  /// interned-`bincode` cache is always smaller than the JSON it was built from, so
  /// tracking the download cap guarantees any manifest that downloads successfully
  /// can be read back from cache rather than being silently re-downloaded on every
  /// run.
  ///
  /// Set this explicitly to bound decode allocation more tightly than the download
  /// cap — e.g. when a `cache_dir` is shared across mutually-distrusting users and
  /// you want a smaller ceiling. Setting it *below* what your community's cache
  /// actually needs will make that cache unreadable (it re-downloads instead), so
  /// size it to your community. Pass [`u64::MAX`] to effectively disable the cap.
  pub fn manifest_max_decompressed_bytes(mut self, max_bytes: u64) -> Self {
    self.max_manifest_decompressed_bytes = Some(max_bytes);

    self
  }

  /// Sets the retry policy for transient network failures (default: a few
  /// retries with exponential backoff). Retries apply to package downloads and
  /// the index download, only for transient errors (connection/timeout/body,
  /// `5xx`, `429`) — never a `4xx` or a terminal error. Pass
  /// [`RetryPolicy::none`] to disable retrying.
  pub fn retry(mut self, policy: RetryPolicy) -> Self {
    self.retry = policy;

    self
  }

  /// Convenience for [`retry`](Self::retry): sets only the maximum number of
  /// retries, keeping the default backoff.
  pub fn max_retries(mut self, max_retries: u32) -> Self {
    self.retry.max_retries = max_retries;

    self
  }

  /// Sets the maximum size, in bytes, of a single package archive download. The
  /// default is a generous ceiling; a download whose declared `Content-Length` or
  /// actual streamed size exceeds it is rejected with
  /// [`Error::SizeLimitExceeded`], which
  /// bounds disk use against an oversized or runaway body. Pass [`u64::MAX`] to
  /// effectively disable the cap.
  pub fn download_max_bytes(mut self, max_bytes: u64) -> Self {
    self.max_download_bytes = Some(max_bytes);

    self
  }

  /// Builds the [`ThunderstoreClient`].
  ///
  /// # Errors
  ///
  /// Returns an error if `cache_dir` was not set, the `community` slug is
  /// invalid, or the HTTP client cannot be constructed.
  pub fn build(self) -> Result<ThunderstoreClient> {
    let cache_dir = self
      .cache_dir
      .ok_or_else(|| Error::Config("cache_dir is required".to_string()))?;

    let base_url = self
      .base_url
      .clone()
      .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
      .trim_end_matches('/')
      .to_string();

    let package_index_url = compose_index_url(
      self.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL),
      self.community.as_deref(),
      self.package_index_url.as_deref(),
    )?;

    let index_cache_key = format!("{:08x}", crc32fast::hash(package_index_url.as_bytes()));

    let max_manifest_decompressed_bytes = resolve_decompressed_cap(
      self.max_manifest_decompressed_bytes,
      self.max_manifest_bytes,
    );

    // The timeout is applied as a connection + per-read (idle) timeout rather
    // than a deadline on the whole request, so a large but healthy download is
    // not cut off mid-stream while a stalled socket still fails promptly.
    // Redirects are followed but bounded (reqwest's default limit) and never for
    // non-HTTP schemes; download URLs come from the fetched index, so this is a
    // deliberate, documented policy.
    let http = Client::builder()
      .connect_timeout(self.download_timeout)
      .read_timeout(self.download_timeout)
      .redirect(reqwest::redirect::Policy::limited(10))
      .build()?;
    let progress = self.progress.unwrap_or_else(|| Arc::new(NoopProgress));

    Ok(ThunderstoreClient {
      http,
      package_index_url,
      base_url,
      cache_dir,
      index_cache_key,
      progress,
      download_concurrency: self.download_concurrency,
      max_download_bytes: self.max_download_bytes,
      max_manifest_bytes: self.max_manifest_bytes,
      max_manifest_decompressed_bytes,
      retry: self.retry,
    })
  }
}

/// Validates a community slug so it cannot inject extra path segments, a query,
/// or `..` into the composed index URL. Real Thunderstore communities are
/// lowercase alphanumeric slugs with hyphens.
fn validate_community(community: &str) -> Result<()> {
  let is_slug = !community.is_empty()
    && community
      .chars()
      .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');

  if !is_slug {
    return Err(Error::Config(format!(
      "invalid community slug {:?}: expected only ASCII letters, digits, '-', and '_'",
      community
    )));
  }

  Ok(())
}

/// Composes the package-index URL from the builder's configuration.
///
/// An explicit `override_url` (e.g. a mirror or a test server) takes precedence
/// and is used verbatim. Otherwise the URL is built from `base_url` and the
/// community slug, which is validated first so it cannot inject extra path
/// segments. Returns an error if neither an override nor a community is given,
/// or the community slug is invalid.
fn compose_index_url(
  base_url: &str,
  community: Option<&str>,
  override_url: Option<&str>,
) -> Result<String> {
  if let Some(url) = override_url {
    return Ok(url.to_string());
  }

  let community = community.ok_or_else(|| Error::Config("community is required".to_string()))?;

  validate_community(community)?;

  Ok(format!(
    "{}/c/{}/api/v1/package/",
    base_url.trim_end_matches('/'),
    community
  ))
}

/// The `POST …/legacyprofile/create/` response body.
#[derive(serde::Deserialize)]
struct ProfileCodeResponse {
  key: String,
}

/// Validates a profile code before it is interpolated into a URL path, so a
/// crafted code cannot inject extra path segments or a query. Real codes are
/// server-issued identifiers of ASCII alphanumerics and hyphens.
fn validate_profile_code(code: &str) -> Result<()> {
  let is_valid = !code.is_empty()
    && code
      .chars()
      .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');

  if !is_valid {
    return Err(Error::Config(format!(
      "invalid profile code {code:?}: expected only ASCII letters, digits, '-', and '_'"
    )));
  }

  Ok(())
}

/// Resolves the maximum *decompressed* cache size from the two configured caps.
///
/// An explicit `configured` value wins. Otherwise the read-back cap tracks the
/// `download_cap` so any manifest that passes the (JSON) download cap fits under
/// it once interned — the bincode cache is always smaller than the JSON — and can
/// never become unreadable from cache. With neither set, the fallback
/// [`DEFAULT_MAX_MANIFEST_DECOMPRESSED_BYTES`] applies.
fn resolve_decompressed_cap(configured: Option<u64>, download_cap: Option<u64>) -> usize {
  configured
    .or(download_cap)
    .map(|n| usize::try_from(n).unwrap_or(usize::MAX))
    .unwrap_or(DEFAULT_MAX_MANIFEST_DECOMPRESSED_BYTES)
}

/// Returns the path to the last_modified file in the cache directory.
fn last_modified_path(cache_dir: &Path) -> PathBuf {
  cache_dir.join(LAST_MODIFIED_FILENAME)
}

/// Returns the path to the cached API manifest file in the cache directory.
fn api_manifest_path(cache_dir: &Path) -> PathBuf {
  cache_dir.join(API_MANIFEST_FILENAME)
}

/// Reads the cached API manifest from disk using the fallback decompressed-size
/// cap ([`DEFAULT_MAX_MANIFEST_DECOMPRESSED_BYTES`]).
///
/// The cache is a `zstd` + `bincode`-encoded interned manifest. A corrupt or
/// truncated file is reported as an error rather than panicking, and the
/// decompressed size is hard-capped. That cap — not any per-container decode limit
/// — is what bounds allocation, so a crafted cache planted in a shared cache dir
/// can still expand into a bounded amount of `Vec` backing stores but no more. The
/// live client passes its configured cap via
/// [`get_manifest_from_disk_with_limit`]; this convenience wrapper (used by tests)
/// applies the fallback cap.
#[cfg(test)]
async fn get_manifest_from_disk(cache_dir: &Path) -> Result<PackageIndex> {
  get_manifest_from_disk_with_limit(cache_dir, DEFAULT_MAX_MANIFEST_DECOMPRESSED_BYTES).await
}

/// [`get_manifest_from_disk`] with an explicit decompressed-size cap.
async fn get_manifest_from_disk_with_limit(
  cache_dir: &Path,
  max_decompressed: usize,
) -> Result<PackageIndex> {
  let path = api_manifest_path(cache_dir);

  if !path.exists() {
    return Err(Error::Cache("No cached manifest found".to_string()));
  }

  // The zstd inflate + bincode decode are blocking CPU/IO work, so they run on a
  // blocking thread rather than stalling the async runtime.
  tokio::task::spawn_blocking(move || decode_cached_manifest(&path, max_decompressed))
    .await
    .map_err(|e| Error::Cache(format!("cache decode task panicked: {}", e)))?
}

/// Whether a cache dated `local` is still fresh given the server's `network`
/// last-modified time — i.e. the server has nothing newer than what is cached.
fn is_fresh(network: DateTime<FixedOffset>, local: DateTime<FixedOffset>) -> bool {
  network <= local
}

/// Loads the cached index, logging a warning and returning `None` — so the
/// caller re-fetches from the network — when it is missing or unreadable.
///
/// This is the shared fallback used on both the fresh-cache and
/// freshness-check-failed paths of [`ThunderstoreClient::get_manifest`].
async fn load_cached_index(index_dir: &Path, max_decompressed: usize) -> Option<PackageIndex> {
  match get_manifest_from_disk_with_limit(index_dir, max_decompressed).await {
    Ok(manifest) => Some(manifest),
    Err(err) => {
      tracing::warn!("Cached manifest unreadable ({}); re-fetching", err);

      None
    }
  }
}

/// Reads and decodes a cached index from `path` on the current (blocking) thread.
///
/// The decode streams straight from the file with a hard cap on the decompressed
/// output, so the inflated bytes are never larger than `max_decompressed` before
/// [`PackageIndex::validate`] (which runs only after a full decode) sees them. The
/// `bincode` serde deserializer can still expand those bytes into larger `Vec`
/// backing stores, so it is this decompressed-size cap that ultimately bounds
/// allocation for a high-ratio file planted in a shared cache dir.
fn decode_cached_manifest(path: &Path, max_decompressed: usize) -> Result<PackageIndex> {
  let limit = max_decompressed as u64;
  let file = std::fs::File::open(path)?;

  let mut decoder = zstd::Decoder::new(file)?;
  let mut decompressed_data = Vec::new();

  decoder
    .by_ref()
    .take(limit + 1)
    .read_to_end(&mut decompressed_data)?;

  if decompressed_data.len() as u64 > limit {
    return Err(Error::SizeLimitExceeded {
      context: "decompressed cache".to_string(),
      limit,
      actual: Some(decompressed_data.len() as u64),
    });
  }

  // Native `bincode::Decode` (not the serde bridge): its `with_limit` makes the
  // decoder claim `len * size_of` up front for every container, so a crafted cache
  // cannot amplify a small input into a huge allocation — decode fails once the
  // running total would exceed [`MAX_DECODE_ALLOC`]. That structural ceiling holds
  // regardless of the (configurable, larger) decompressed-size cap above.
  let config = bincode::config::standard().with_limit::<MAX_DECODE_ALLOC>();
  let (serializable, _): (SerializedPackageIndex, usize) =
    bincode::decode_from_slice(&decompressed_data, config)
      .map_err(|e| Error::Cache(format!("failed to decode cached manifest: {}", e)))?;

  serializable.try_into()
}

/// Downloads the manifest from the API server.
///
/// The response body is bounded by `max_bytes` (checked against the declared
/// `Content-Length` up front and the actual streamed size) so an oversized index
/// from a compromised or misbehaving host cannot exhaust memory.
async fn get_manifest_from_network(
  http: &Client,
  api_url: &str,
  max_bytes: Option<u64>,
  retry: &RetryPolicy,
  progress: &dyn ProgressReporter,
) -> Result<(Vec<Package>, DateTime<FixedOffset>)> {
  let handle = progress.begin("Downloading Api Manifest", None);

  // Retry only the transient network parts; a missing header, oversize body, or
  // JSON parse error is terminal and returns on the first attempt.
  let result = retry_http(retry, || async {
    let response = http.get(api_url).send().await?.error_for_status()?;
    let last_modified_str = response
      .headers()
      .get(header::LAST_MODIFIED)
      .ok_or(Error::MissingHeader("Last-Modified".to_string()))?
      .to_str()?;
    let last_modified = DateTime::parse_from_rfc2822(last_modified_str)
      .map_err(|e| Error::InvalidHeader(format!("invalid Last-Modified header: {}", e)))?;

    let raw_response_data = read_capped_body(response, max_bytes).await?;

    let packages: Vec<Package> = serde_json::from_slice(&raw_response_data)?;

    Ok((packages, last_modified))
  })
  .await;

  // Finish the handle on both paths so a real reporter never leaves an orphaned
  // bar when the download ultimately fails.
  match result {
    Ok((packages, last_modified)) => {
      handle.finish(Some(" Downloaded Api Manifest"));

      Ok((packages, last_modified))
    }
    Err(err) => {
      handle.finish(None);

      Err(err)
    }
  }
}

/// Reads a response body fully into memory, rejecting one that exceeds
/// `max_bytes` — either by its declared `Content-Length` (before streaming) or by
/// its actual streamed size (for chunked bodies with no declared length).
async fn read_capped_body(response: reqwest::Response, max_bytes: Option<u64>) -> Result<Vec<u8>> {
  let mut cap = max_bytes.map(|max| SizeCap::new("manifest body", max));

  if let Some(cap) = &cap {
    cap.check_declared(response.content_length())?;
  }

  let mut stream = response.bytes_stream();
  let mut body: Vec<u8> = Vec::new();

  while let Some(chunk) = stream.next().await {
    let chunk = chunk?;

    if let Some(cap) = cap.as_mut() {
      cap.add(chunk.len() as u64)?;
    }

    body.extend_from_slice(&chunk);
  }

  Ok(body)
}

/// Interns the packages into a [`PackageIndex`] and encodes the on-disk cache
/// blob (the serializable view, `bincode`-encoded) in one CPU-bound pass.
///
/// Pure: no I/O. Runs inside `spawn_blocking` on the caching path, and is
/// directly unit-testable without a server or temp directory.
fn encode_manifest_cache(packages: Vec<Package>) -> Result<(PackageIndex, Vec<u8>)> {
  let manifest: PackageIndex = packages.into();
  let serializable: SerializedPackageIndex = (&manifest).into();

  let binary_data = bincode::encode_to_vec(&serializable, bincode::config::standard())
    .map_err(|e| Error::Cache(format!("failed to encode manifest cache: {}", e)))?;

  Ok((manifest, binary_data))
}

/// Downloads the manifest from the network and caches it locally.
///
/// The manifest is written to disk *before* the `last_modified` freshness marker
/// is advanced, so an interrupted or failed write can never leave the marker
/// pointing at a timestamp the on-disk manifest doesn't actually have (which
/// would pin a stale cache as "fresh").
async fn get_manifest_from_network_and_cache(
  cache_dir: &Path,
  http: &Client,
  api_url: &str,
  max_manifest_bytes: Option<u64>,
  retry: &RetryPolicy,
  progress: &dyn ProgressReporter,
) -> Result<PackageIndex> {
  let (packages, last_modified_from_network) =
    get_manifest_from_network(http, api_url, max_manifest_bytes, retry, progress).await?;

  // Interning every package string, building the serializable view, and the
  // bincode encode are all CPU-bound, so they run on a blocking thread rather than
  // stalling the async runtime — mirroring the decode-path offload above.
  let (manifest, binary_data) =
    tokio::task::spawn_blocking(move || encode_manifest_cache(packages))
      .await
      .map_err(|e| Error::Cache(format!("cache encode task panicked: {}", e)))??;

  write_cache_to_disk(api_manifest_path(cache_dir), &binary_data, true).await?;

  // Only now that the manifest content is durably on disk do we advance the
  // freshness marker. The marker write is best-effort: the manifest is already
  // persisted, so a failure here just leaves a missing/stale marker that triggers
  // a harmless re-fetch on the next call rather than discarding valid cached data.
  if let Err(err) = write_cache_to_disk(
    last_modified_path(cache_dir),
    last_modified_from_network.to_rfc2822().as_bytes(),
    false,
  )
  .await
  {
    tracing::warn!(
      "Failed to write last_modified marker ({}); cached manifest kept, will re-fetch",
      err
    );
  }

  Ok(manifest)
}

/// Retrieves the last modified date from the local cache file.
async fn local_last_modified(cache_dir: &Path) -> Result<DateTime<FixedOffset>> {
  let path = last_modified_path(cache_dir);

  if let Ok(file) = fs::File::open(&path).await {
    tracing::info!("Last modified file exists and was opened.");

    // Cap the read so an oversized file planted in a shared cache dir cannot
    // exhaust memory, and decode lossily: a missing, non-UTF-8, or unparseable
    // marker is treated as "unknown" (epoch), so a corrupt/legacy marker triggers
    // a re-fetch rather than failing the whole call — the marker is only a hint.
    let mut buf = Vec::new();

    file
      .take(MAX_LAST_MODIFIED_BYTES)
      .read_to_end(&mut buf)
      .await?;

    let contents = String::from_utf8_lossy(&buf);

    match DateTime::parse_from_rfc2822(contents.trim()) {
      Ok(last_modified) => Ok(last_modified),
      Err(err) => {
        tracing::warn!(
          "Unparseable last_modified marker ({}); treating as stale",
          err
        );

        Ok(epoch())
      }
    }
  } else {
    tracing::info!("Last modified file does not exist and was not opened.");

    Ok(epoch())
  }
}

/// The Unix epoch as a fixed-offset timestamp, used as the "unknown / very old"
/// sentinel for cache freshness so any real server timestamp is newer.
fn epoch() -> DateTime<FixedOffset> {
  let dt = DateTime::from_timestamp(0, 0)
    .expect("timestamp 0 is valid")
    .naive_utc();
  let offset = FixedOffset::east_opt(0).expect("zero offset is valid");

  DateTime::<FixedOffset>::from_naive_utc_and_offset(dt, offset)
}

/// Retrieves the last modified date from the API server via a `HEAD` request.
async fn network_last_modified(http: &Client, api_url: &str) -> Result<DateTime<FixedOffset>> {
  let response = http.head(api_url).send().await?.error_for_status()?;

  let last_modified = response
    .headers()
    .get(header::LAST_MODIFIED)
    .ok_or(Error::MissingHeader(
      "Last-Modified for API manifest head request".to_string(),
    ))?;
  let last_modified_date = DateTime::parse_from_rfc2822(last_modified.to_str()?)
    .map_err(|e| Error::InvalidHeader(format!("invalid Last-Modified header: {}", e)))?;

  Ok(last_modified_date)
}

/// Checks for the existence of any version of the cached manifest file.
fn api_manifest_file_exists(cache_dir: &Path) -> bool {
  api_manifest_path(cache_dir).exists()
}

/// Computes a unique sibling temp path next to `path` for atomic writes.
///
/// The temp file lives in the same directory as the destination so the
/// subsequent rename is atomic on the same filesystem. A per-process counter and
/// the process id keep concurrent writers from colliding on the same temp name.
fn temp_sibling_path(path: &Path) -> PathBuf {
  static COUNTER: AtomicU64 = AtomicU64::new(0);

  let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
  let pid = std::process::id();
  let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("cache");
  let tmp_name = format!(".{name}.{pid}.{seq}.tmp");

  match path.parent() {
    Some(parent) => parent.join(tmp_name),
    None => PathBuf::from(tmp_name),
  }
}

/// Writes data to a cache file on disk, optionally compressing with zstd level 9.
///
/// The write is atomic: data is written to a sibling temp file, `fsync`ed, then
/// renamed over the destination, so an interrupted write can never leave a
/// truncated or partially-written cache file in place.
async fn write_cache_to_disk<T: AsRef<Path>>(
  path: T,
  contents: &[u8],
  use_compression: bool,
) -> Result<()> {
  let path = path.as_ref();

  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent).await?;
  }

  let payload = if use_compression {
    // zstd level 9 is CPU-bound; run it off the async runtime thread.
    let data = contents.to_vec();

    tokio::task::spawn_blocking(move || zstd::encode_all(data.as_slice(), 9))
      .await
      .map_err(|e| Error::Cache(format!("cache encode task panicked: {}", e)))??
  } else {
    contents.to_vec()
  };

  let tmp_path = temp_sibling_path(path);

  // On any failure, remove the sibling temp file so a failed write does not leave
  // an orphan behind in the cache dir.
  if let Err(err) = write_and_rename(&tmp_path, path, &payload).await {
    let _ = fs::remove_file(&tmp_path).await;

    return Err(err);
  }

  Ok(())
}

/// Writes `payload` to `tmp_path`, fsyncs it, renames it over `path`, then fsyncs
/// the containing directory so the rename (the new directory entry) is durable
/// across a crash — not just the file's contents.
async fn write_and_rename(tmp_path: &Path, path: &Path, payload: &[u8]) -> Result<()> {
  let mut file = fs::File::create(tmp_path).await?;

  file.write_all(payload).await?;
  file.flush().await?;
  file.sync_all().await?;

  fs::rename(tmp_path, path).await?;

  // Best-effort directory fsync: succeeds on Unix, may be unsupported elsewhere,
  // so a failure here is not treated as fatal.
  if let Some(parent) = path.parent()
    && let Ok(dir) = fs::File::open(parent).await
  {
    let _ = dir.sync_all().await;
  }

  Ok(())
}

/// Parses a `Content-Length` header into a byte count, returning `None` when the
/// header is absent or unparseable (e.g. a chunked body, or a malformed value).
fn parse_content_length(headers: &reqwest::header::HeaderMap) -> Option<u64> {
  headers
    .get(header::CONTENT_LENGTH)
    .and_then(|value| value.to_str().ok())
    .and_then(|value| value.parse::<u64>().ok())
}

/// Verifies a finished download received exactly the declared number of bytes.
///
/// When the response declared a `Content-Length`, a stream that ends with a
/// different number of bytes is an incomplete/corrupt transfer and is rejected
/// with [`Error::IncompleteDownload`]. With no declared length (a chunked body),
/// any clean end is accepted. Pure — no I/O.
fn check_complete(content_length: Option<u64>, written: u64) -> Result<()> {
  if let Some(expected) = content_length
    && written != expected
  {
    return Err(Error::IncompleteDownload {
      expected,
      actual: written,
    });
  }

  Ok(())
}

/// Collapses the per-file download results into a single outcome: `Ok(())` if
/// every file succeeded, otherwise an [`Error::Download`] reporting how many of
/// `total` failed and the first failure's message. Pure over the result vector.
fn aggregate_download_results(results: Vec<Result<()>>, total: usize) -> Result<()> {
  let mut failures = 0;
  let mut first_error: Option<String> = None;

  for result in results {
    if let Err(err) = result {
      failures += 1;

      tracing::error!("Download error: {:?}", err);

      if first_error.is_none() {
        first_error = Some(err.to_string());
      }
    }
  }

  if failures > 0 {
    return Err(Error::Download {
      failed: failures,
      total,
      first: first_error.unwrap_or_default(),
    });
  }

  Ok(())
}

/// Downloads multiple files concurrently with progress indicators.
async fn download_files(
  urls: HashMap<String, String>,
  cache_dir: &Path,
  http: &Client,
  concurrency: usize,
  max_bytes: Option<u64>,
  retry: RetryPolicy,
  progress: Arc<dyn ProgressReporter>,
) -> Result<()> {
  if urls.is_empty() {
    tracing::debug!("No files to download");

    return Ok(());
  }

  tracing::debug!("Processing {} mods", urls.len());

  let total = urls.len();
  let concurrency = concurrency.max(1);

  let results: Vec<Result<()>> =
    futures::stream::iter(urls.into_iter().map(|(archive_filename, url)| {
      let client = http.clone();
      let cache_dir = cache_dir.to_path_buf();
      let progress = progress.clone();

      async move {
        download_file(
          &client,
          &url,
          &archive_filename,
          &cache_dir,
          max_bytes,
          &retry,
          progress.as_ref(),
        )
        .await
      }
    }))
    .buffer_unordered(concurrency)
    .collect()
    .await;

  aggregate_download_results(results, total)
}

/// Validates a download filename, rejecting absolute paths and any path
/// traversal (`..`, separators) so a download cannot escape the downloads
/// directory.
fn safe_download_name(filename: &str) -> Result<&str> {
  let mut components = Path::new(filename).components();
  let only = components.next();

  let is_single_normal = matches!(only, Some(std::path::Component::Normal(_)));

  if !is_single_normal || components.next().is_some() {
    return Err(Error::UnsafeName(format!(
      "Unsafe download filename: {}",
      filename
    )));
  }

  Ok(filename)
}

/// Best-effort cleanup guard for a download's sibling temp file.
///
/// While armed, dropping the guard removes the temp file, so a download future
/// that is cancelled (dropped) mid-stream — or that returns early on error — never
/// leaves an orphan `.tmp` behind. Call [`disarm`](TempFileGuard::disarm) once the
/// file has been renamed into place, at which point there is nothing left to
/// remove at the temp path.
struct TempFileGuard {
  path: Option<PathBuf>,
}

impl TempFileGuard {
  fn new(path: PathBuf) -> Self {
    Self { path: Some(path) }
  }

  /// Stops the guard from removing the temp file, e.g. after a successful rename.
  fn disarm(&mut self) {
    self.path = None;
  }
}

impl Drop for TempFileGuard {
  fn drop(&mut self) {
    if let Some(path) = self.path.take() {
      let _ = std::fs::remove_file(path);
    }
  }
}

/// Downloads a single file from a URL with a progress indicator.
///
/// The body is streamed to a sibling temp file and renamed into place only after
/// the full `Content-Length` has been received, so an interrupted download never
/// leaves a truncated file that would be mistaken for a complete one on retry.
///
/// Transient failures (connection/timeout/body errors, `5xx`, `429`) are retried
/// per `retry`; each attempt re-creates (truncates) the temp file, so a retry
/// restarts cleanly. A `4xx` and the terminal size-cap/incomplete errors are not
/// retried.
///
/// When `max_bytes` is set, the download is rejected — before streaming if the
/// server declares an oversized `Content-Length`, and mid-stream once the actual
/// bytes written exceed the cap — so an oversized or runaway body cannot fill the
/// disk. A failed *or cancelled* download never leaves a partial file behind: a
/// drop-guard removes the temp file on any early return or if the future is
/// dropped mid-stream, and is disarmed only once the file is renamed into place.
async fn download_file(
  client: &Client,
  url: &str,
  filename: &str,
  cache_dir: &Path,
  max_bytes: Option<u64>,
  retry: &RetryPolicy,
  progress: &dyn ProgressReporter,
) -> Result<()> {
  let downloads_directory = cache_dir.join("downloads");
  let file_path = downloads_directory.join(safe_download_name(filename)?);

  tokio::fs::DirBuilder::new()
    .recursive(true)
    .create(&downloads_directory)
    .await
    .map_err(|e| {
      io_context(
        format!("creating downloads dir {}", downloads_directory.display()),
        e,
      )
    })?;

  if file_path.exists() {
    tracing::debug!("{} already exists, skipping download", filename);

    return Ok(());
  }

  let tmp_path = temp_sibling_path(&file_path);

  // Removes the temp file if this future is dropped (cancelled) mid-download or
  // returns early on error; disarmed only once the file is renamed into place.
  let mut tmp_guard = TempFileGuard::new(tmp_path.clone());

  // One progress handle spans all attempts and is finished exactly once (below),
  // so a retried transient failure never orphans a half-finished bar. Its total
  // is set once the response's Content-Length is known.
  let handle = progress.begin(filename, None);

  // Each attempt re-runs the request and re-creates the temp file, so a retried
  // transient failure restarts from a clean slate. Terminal errors (a declared or
  // streamed oversize, an incomplete transfer) short-circuit the retry loop.
  let attempt = retry_http(retry, || async {
    // Rewind the bar to zero at the start of every attempt so a retried mid-stream
    // failure re-inc'ing re-downloaded bytes cannot overshoot the total.
    handle.set_position(0);

    let response = client.get(url).send().await?.error_for_status()?;

    // `Content-Length` is optional: servers using chunked transfer encoding omit
    // it. When present it gives the progress total and a completeness check; when
    // absent (or malformed) a stream that finishes without a transport error is
    // treated as complete.
    let content_length = parse_content_length(response.headers());
    let mut cap = max_bytes.map(|max| SizeCap::new("download body", max));

    // Reject a declared-oversize body up front, before streaming a single byte.
    if let Some(cap) = &cap {
      cap.check_declared(content_length)?;
    }

    if let Some(total) = content_length {
      handle.set_length(total);
    }

    let mut response_data = response.bytes_stream();
    let mut file = tokio::fs::File::create(&tmp_path).await.map_err(|e| {
      io_context(
        format!("creating temp download file {}", tmp_path.display()),
        e,
      )
    })?;
    let mut written: u64 = 0;

    while let Some(result) = response_data.next().await {
      let chunk = result?;

      written += chunk.len() as u64;

      if let Some(cap) = cap.as_mut() {
        cap.add(chunk.len() as u64)?;
      }

      file.write_all(&chunk).await?;
      handle.inc(chunk.len() as u64);
    }

    file.flush().await?;
    file.sync_all().await?;

    check_complete(content_length, written)
  })
  .await;

  if let Err(err) = attempt {
    // `tmp_guard` removes the temp file as it drops on return.
    handle.finish(None);

    return Err(err);
  }

  // Rename into place first; only report completion once the file actually
  // lands, so a rename failure is never shown to the UI as a finished download.
  // On a rename failure `tmp_guard` removes the leftover temp file on return.
  if let Err(err) = tokio::fs::rename(&tmp_path, &file_path).await {
    handle.finish(None);

    return Err(io_context(
      format!("renaming {} to {}", tmp_path.display(), file_path.display()),
      err,
    ));
  }

  // The temp file has been renamed away, so there is nothing left to clean up.
  tmp_guard.disarm();
  handle.finish(None);

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::progress::{NoopProgress, ProgressHandle};
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

    let _ = File::create(cache_dir.join(API_MANIFEST_FILENAME)).unwrap();

    assert!(api_manifest_file_exists(cache_dir));
  }

  #[test]
  fn test_path_construction() {
    let cache_dir = Path::new("/some/cache/dir");

    let last_modified = last_modified_path(cache_dir);
    let api_manifest = api_manifest_path(cache_dir);

    assert_eq!(last_modified, cache_dir.join(LAST_MODIFIED_FILENAME));
    assert_eq!(api_manifest, cache_dir.join(API_MANIFEST_FILENAME));
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

    assert!(matches!(result, Err(Error::Config(_))));
  }

  #[test]
  fn parse_content_length_reads_valid_and_ignores_missing_or_bad() {
    use reqwest::header::{CONTENT_LENGTH, HeaderMap, HeaderValue};

    let mut h = HeaderMap::new();

    assert_eq!(parse_content_length(&h), None);

    h.insert(CONTENT_LENGTH, HeaderValue::from_static("2048"));

    assert_eq!(parse_content_length(&h), Some(2048));

    h.insert(CONTENT_LENGTH, HeaderValue::from_static("not-a-number"));

    assert_eq!(parse_content_length(&h), None);
  }

  #[test]
  fn aggregate_download_results_all_ok_is_ok() {
    assert!(aggregate_download_results(vec![Ok(()), Ok(())], 2).is_ok());
    assert!(aggregate_download_results(vec![], 0).is_ok());
  }

  #[test]
  fn aggregate_download_results_reports_count_and_first_error() {
    let results = vec![
      Ok(()),
      Err(Error::UnsafeName("bad".to_string())),
      Err(Error::IncompleteDownload {
        expected: 5,
        actual: 2,
      }),
    ];

    let err = aggregate_download_results(results, 3).unwrap_err();

    match err {
      Error::Download {
        failed,
        total,
        first,
      } => {
        assert_eq!(failed, 2);
        assert_eq!(total, 3);
        assert!(first.contains("unsafe name"), "first was: {first}");
      }
      other => panic!("unexpected error: {other}"),
    }
  }

  #[test]
  fn compose_index_url_prefers_override_then_builds_from_community() {
    assert_eq!(
      compose_index_url("https://thunderstore.io", Some("valheim"), None).unwrap(),
      "https://thunderstore.io/c/valheim/api/v1/package/"
    );

    // A trailing slash on the base is trimmed so the path is never doubled.
    assert_eq!(
      compose_index_url("https://thunderstore.io/", Some("valheim"), None).unwrap(),
      "https://thunderstore.io/c/valheim/api/v1/package/"
    );

    // An override is used verbatim, ignoring base/community.
    assert_eq!(
      compose_index_url("https://ignored", None, Some("https://custom/pkg/")).unwrap(),
      "https://custom/pkg/"
    );

    // Neither override nor community => Config error.
    assert!(matches!(
      compose_index_url("https://thunderstore.io", None, None),
      Err(Error::Config(_))
    ));

    // An invalid community slug is rejected.
    assert!(matches!(
      compose_index_url("https://thunderstore.io", Some("bad/slug"), None),
      Err(Error::Config(_))
    ));
  }

  #[test]
  fn resolve_decompressed_cap_precedence() {
    // An explicit configured value wins.
    assert_eq!(resolve_decompressed_cap(Some(42), Some(100)), 42);

    // Unset falls back to the download cap.
    assert_eq!(resolve_decompressed_cap(None, Some(100)), 100);

    // Neither set falls back to the default.
    assert_eq!(
      resolve_decompressed_cap(None, None),
      DEFAULT_MAX_MANIFEST_DECOMPRESSED_BYTES
    );
  }

  #[test]
  fn is_fresh_compares_last_modified() {
    let older = DateTime::parse_from_rfc2822("Wed, 21 Feb 2024 15:30:45 +0000").unwrap();
    let newer = DateTime::parse_from_rfc2822("Thu, 22 Feb 2024 15:30:45 +0000").unwrap();

    // Server has nothing newer than the cache => fresh (serve cache).
    assert!(is_fresh(older, older));
    assert!(is_fresh(older, newer));

    // Server is newer than the cache => stale (re-download).
    assert!(!is_fresh(newer, older));
  }

  #[test]
  fn encode_manifest_cache_builds_index_and_nonempty_blob() {
    let (index, bytes) = encode_manifest_cache(vec![sample_package("X", "O-X", "O")]).unwrap();

    assert_eq!(index.len(), 1);
    assert!(!bytes.is_empty());
  }

  #[test]
  fn check_complete_flags_short_and_accepts_exact_or_unknown() {
    // Exact match against the declared length is complete.
    assert!(check_complete(Some(100), 100).is_ok());

    // No declared length (chunked) => any clean end is accepted.
    assert!(check_complete(None, 42).is_ok());

    // Fewer bytes than declared => incomplete transfer.
    let err = check_complete(Some(100), 50).unwrap_err();

    assert!(matches!(
      err,
      Error::IncompleteDownload {
        expected: 100,
        actual: 50,
      }
    ));
  }

  #[test]
  fn safe_download_name_accepts_plain_and_rejects_traversal() {
    // A plain single-component filename is accepted verbatim.
    assert_eq!(
      safe_download_name("Author-Mod-1.0.0.zip").unwrap(),
      "Author-Mod-1.0.0.zip"
    );

    // Parent-dir traversal, an absolute path, and an embedded separator are all
    // rejected so a download can never escape the downloads directory. (A
    // backslash is a normal filename char on Unix and is intentionally not
    // covered here — the guard is about real path separators.)
    assert!(matches!(
      safe_download_name("../escape.zip"),
      Err(Error::UnsafeName(_))
    ));
    assert!(matches!(
      safe_download_name("/etc/passwd"),
      Err(Error::UnsafeName(_))
    ));
    assert!(matches!(
      safe_download_name("a/b.zip"),
      Err(Error::UnsafeName(_))
    ));
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
    let binary_data =
      bincode::serde::encode_to_vec(&packages, bincode::config::standard()).unwrap();

    let rt = Runtime::new().unwrap();
    let result = rt.block_on(write_cache_to_disk(&api_manifest_path, &binary_data, true));

    assert!(result.is_ok());
    assert!(api_manifest_path.exists());

    let compressed_data = std::fs::read(&api_manifest_path).unwrap();
    let decompressed_data = zstd::decode_all(compressed_data.as_slice()).unwrap();
    let (decoded, _): (Vec<Package>, usize) =
      bincode::serde::decode_from_slice(&decompressed_data, bincode::config::standard()).unwrap();

    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].full_name, Some("TestOwner-TestMod".to_string()));
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
    let interned_manifest: PackageIndex = packages.into();
    let serializable: SerializedPackageIndex = (&interned_manifest).into();
    let binary_data = bincode::encode_to_vec(&serializable, bincode::config::standard()).unwrap();
    let compressed_data = zstd::encode_all(binary_data.as_slice(), 9).unwrap();
    let mut file = File::create(cache_dir.join(API_MANIFEST_FILENAME)).unwrap();
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

    let mut file = File::create(cache_dir.join(API_MANIFEST_FILENAME)).unwrap();
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
      None,
      &RetryPolicy::none(),
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

  /// Seeds a valid compressed cache and a `last_modified` marker into `index_dir`.
  fn seed_index_cache(index_dir: &Path, last_modified: &str) {
    std::fs::create_dir_all(index_dir).unwrap();

    let package = sample_package("CachedMod", "CachedOwner-CachedMod", "CachedOwner");
    let interned_manifest: PackageIndex = vec![package].into();
    let serializable: SerializedPackageIndex = (&interned_manifest).into();
    let binary_data = bincode::encode_to_vec(&serializable, bincode::config::standard()).unwrap();
    let compressed_data = zstd::encode_all(binary_data.as_slice(), 9).unwrap();

    File::create(index_dir.join(API_MANIFEST_FILENAME))
      .unwrap()
      .write_all(&compressed_data)
      .unwrap();

    File::create(index_dir.join(LAST_MODIFIED_FILENAME))
      .unwrap()
      .write_all(last_modified.as_bytes())
      .unwrap();
  }

  #[test]
  fn test_get_manifest_loads_fresh_cache() {
    let mut server = Server::new();
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    // The server reports an old timestamp, so the recently-cached copy wins.
    let mock = server
      .mock("HEAD", "/pkg/")
      .with_status(200)
      .with_header("Last-Modified", "Wed, 21 Feb 2024 15:30:45 GMT")
      .create();
    let api_url = format!("{}/pkg/", server.url());

    let client = ThunderstoreClient::builder()
      .package_index_url(&api_url)
      .cache_dir(cache_dir)
      .build()
      .unwrap();

    let now = chrono::Utc::now().with_timezone(&chrono::FixedOffset::east_opt(0).unwrap());
    seed_index_cache(&client.index_cache_dir(), &now.to_rfc2822());

    let rt = Runtime::new().unwrap();
    let manifest = rt.block_on(client.get_manifest()).unwrap();

    assert_eq!(manifest.len(), 1);
    assert_eq!(
      manifest.resolve_full_name_at(0),
      Some("CachedOwner-CachedMod".to_string())
    );

    mock.assert();
  }

  #[test]
  fn test_get_manifest_falls_back_to_cache_when_head_fails() {
    let mut server = Server::new();
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    // The freshness HEAD request fails (server 500). The cache must still load.
    let mock = server
      .mock("HEAD", "/c/valheim/api/v1/package/")
      .with_status(500)
      .create();
    let api_url = format!("{}/c/valheim/api/v1/package/", server.url());

    let client = ThunderstoreClient::builder()
      .package_index_url(&api_url)
      .cache_dir(cache_dir)
      .build()
      .unwrap();

    seed_index_cache(&client.index_cache_dir(), "Wed, 21 Feb 2024 15:30:45 GMT");

    let rt = Runtime::new().unwrap();
    let result = rt.block_on(client.get_manifest());

    assert!(result.is_ok());

    let manifest = result.unwrap();

    assert_eq!(manifest.len(), 1);
    assert_eq!(
      manifest.resolve_full_name_at(0),
      Some("CachedOwner-CachedMod".to_string())
    );

    mock.assert();
  }

  #[test]
  fn test_refresh_index_bypasses_cache_and_downloads() {
    let mut server = Server::new();
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    // `refresh_index` never issues a HEAD; it downloads unconditionally.
    let get_mock = server
      .mock("GET", "/pkg/")
      .with_status(200)
      .with_header("Content-Type", "application/json")
      .with_header("Last-Modified", "Wed, 21 Feb 2024 15:30:45 GMT")
      .with_body(sample_manifest_json())
      .create();
    let api_url = format!("{}/pkg/", server.url());

    let client = ThunderstoreClient::builder()
      .package_index_url(&api_url)
      .cache_dir(cache_dir)
      .build()
      .unwrap();

    // A fresh cache is seeded that would otherwise be served; refresh ignores it.
    let now = chrono::Utc::now().with_timezone(&chrono::FixedOffset::east_opt(0).unwrap());
    seed_index_cache(&client.index_cache_dir(), &now.to_rfc2822());

    let rt = Runtime::new().unwrap();
    let manifest = rt.block_on(client.refresh_index()).unwrap();

    // The network index ("Owner-ModA"), not the cached one, is returned.
    assert_eq!(
      manifest.resolve_full_name_at(0),
      Some("Owner-ModA".to_string())
    );

    get_mock.assert();
  }

  #[test]
  fn test_get_manifest_downloads_when_cache_is_stale() {
    let mut server = Server::new();
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    // HEAD reports a timestamp newer than the seeded cache => stale => download.
    let head_mock = server
      .mock("HEAD", "/pkg/")
      .with_status(200)
      .with_header("Last-Modified", "Wed, 28 Feb 2024 15:30:45 GMT")
      .create();
    let get_mock = server
      .mock("GET", "/pkg/")
      .with_status(200)
      .with_header("Content-Type", "application/json")
      .with_header("Last-Modified", "Wed, 28 Feb 2024 15:30:45 GMT")
      .with_body(sample_manifest_json())
      .create();
    let api_url = format!("{}/pkg/", server.url());

    let client = ThunderstoreClient::builder()
      .package_index_url(&api_url)
      .cache_dir(cache_dir)
      .build()
      .unwrap();

    seed_index_cache(&client.index_cache_dir(), "Wed, 21 Feb 2024 15:30:45 GMT");

    let rt = Runtime::new().unwrap();
    let manifest = rt.block_on(client.get_manifest()).unwrap();

    // The freshly downloaded (network) index wins over the stale cache.
    assert_eq!(
      manifest.resolve_full_name_at(0),
      Some("Owner-ModA".to_string())
    );

    head_mock.assert();
    get_mock.assert();
  }

  #[test]
  fn test_download_file_skips_existing_without_refetch() {
    let mut server = Server::new();
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    // The mock permits exactly one hit; a second download that re-fetched would
    // issue an unmatched request and fail `assert()`.
    let mock = server
      .mock("GET", "/mod.zip")
      .with_status(200)
      .with_body("zip-bytes")
      .expect(1)
      .create();
    let file_url = format!("{}/mod.zip", server.url());

    let rt = Runtime::new().unwrap();

    rt.block_on(download_file(
      &test_http(),
      &file_url,
      "mod.zip",
      cache_dir,
      None,
      &RetryPolicy::none(),
      &NoopProgress,
    ))
    .unwrap();

    // The file now exists, so the second call must short-circuit before any HTTP.
    rt.block_on(download_file(
      &test_http(),
      &file_url,
      "mod.zip",
      cache_dir,
      None,
      &RetryPolicy::none(),
      &NoopProgress,
    ))
    .unwrap();

    mock.assert();
  }

  #[test]
  fn test_head_error_status_is_not_trusted_as_freshness() {
    let mut server = Server::new();
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    // A 500 that still carries a (future) Last-Modified must NOT be trusted as
    // the freshness signal; the cached copy is served instead.
    let mock = server
      .mock("HEAD", "/pkg/")
      .with_status(500)
      .with_header("Last-Modified", "Wed, 21 Feb 2099 15:30:45 GMT")
      .create();
    let api_url = format!("{}/pkg/", server.url());

    let client = ThunderstoreClient::builder()
      .package_index_url(&api_url)
      .cache_dir(cache_dir)
      .build()
      .unwrap();

    seed_index_cache(&client.index_cache_dir(), "Wed, 21 Feb 2024 15:30:45 GMT");

    let rt = Runtime::new().unwrap();
    let manifest = rt.block_on(client.get_manifest()).unwrap();

    assert_eq!(manifest.len(), 1);

    mock.assert();
  }

  #[test]
  fn test_get_manifest_refetches_on_unsupported_cache_version() {
    let mut server = Server::new();
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    // HEAD says the cache is fresh, but the cached file is written by an
    // incompatible format version. The client must fall back to a network GET
    // rather than error.
    let test_date = "Wed, 21 Feb 2024 15:30:45 GMT";
    let head = server
      .mock("HEAD", "/pkg/")
      .with_status(200)
      .with_header("Last-Modified", test_date)
      .create();
    let get = server
      .mock("GET", "/pkg/")
      .with_status(200)
      .with_header("Content-Type", "application/json")
      .with_header("Last-Modified", test_date)
      .with_body(sample_manifest_json())
      .create();
    let api_url = format!("{}/pkg/", server.url());

    let client = ThunderstoreClient::builder()
      .package_index_url(&api_url)
      .cache_dir(cache_dir)
      .build()
      .unwrap();

    // Seed a fresh marker but a cache blob with a bad format version.
    let index_dir = client.index_cache_dir();
    std::fs::create_dir_all(&index_dir).unwrap();

    let interned: PackageIndex = vec![sample_package("X", "O-X", "O")].into();
    let mut serializable: SerializedPackageIndex = (&interned).into();
    serializable.format_version = 99;
    let binary_data = bincode::encode_to_vec(&serializable, bincode::config::standard()).unwrap();
    let compressed = zstd::encode_all(binary_data.as_slice(), 9).unwrap();

    File::create(index_dir.join(API_MANIFEST_FILENAME))
      .unwrap()
      .write_all(&compressed)
      .unwrap();

    let now = chrono::Utc::now().with_timezone(&chrono::FixedOffset::east_opt(0).unwrap());

    File::create(index_dir.join(LAST_MODIFIED_FILENAME))
      .unwrap()
      .write_all(now.to_rfc2822().as_bytes())
      .unwrap();

    let rt = Runtime::new().unwrap();
    let manifest = rt.block_on(client.get_manifest()).unwrap();

    // The network copy (Owner-ModA) replaced the unreadable cache.
    assert_eq!(
      manifest.resolve_full_name_at(0),
      Some("Owner-ModA".to_string())
    );

    head.assert();
    get.assert();
  }

  #[test]
  fn test_builder_rejects_invalid_community() {
    let temp_dir = tempdir().unwrap();

    let result = ThunderstoreClient::builder()
      .community("../../etc")
      .cache_dir(temp_dir.path())
      .build();

    assert!(matches!(result, Err(Error::Config(_))));
  }

  #[test]
  fn test_index_cache_dir_is_namespaced_per_community() {
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    let a = ThunderstoreClient::builder()
      .community("valheim")
      .cache_dir(cache_dir)
      .build()
      .unwrap();
    let b = ThunderstoreClient::builder()
      .community("lethal-company")
      .cache_dir(cache_dir)
      .build()
      .unwrap();

    assert_ne!(a.index_cache_dir(), b.index_cache_dir());
    assert!(a.index_cache_dir().starts_with(cache_dir));
    assert!(b.index_cache_dir().starts_with(cache_dir));
  }

  #[test]
  fn test_local_last_modified_treats_corrupt_marker_as_stale() {
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    File::create(cache_dir.join(LAST_MODIFIED_FILENAME))
      .unwrap()
      .write_all(b"not a date")
      .unwrap();

    let rt = Runtime::new().unwrap();
    let result = rt.block_on(local_last_modified(cache_dir)).unwrap();

    assert_eq!(result, epoch());
  }

  #[test]
  fn test_get_manifest_from_network_rejects_oversize_body() {
    let mut server = Server::new();
    let test_date = "Wed, 21 Feb 2024 15:30:45 GMT";

    let mock = server
      .mock("GET", "/pkg/")
      .with_status(200)
      .with_header("Last-Modified", test_date)
      .with_body(sample_manifest_json())
      .create();
    let api_url = format!("{}/pkg/", server.url());

    let rt = Runtime::new().unwrap();
    let result = rt.block_on(get_manifest_from_network(
      &test_http(),
      &api_url,
      Some(8),
      &RetryPolicy::none(),
      &NoopProgress,
    ));

    assert!(matches!(result, Err(Error::SizeLimitExceeded { .. })));

    mock.assert();
  }

  #[test]
  fn test_get_manifest_from_disk_rejects_oversized_decompressed() {
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    let interned: PackageIndex = vec![sample_package("X", "O-X", "O")].into();
    let serializable: SerializedPackageIndex = (&interned).into();
    let binary_data = bincode::encode_to_vec(&serializable, bincode::config::standard()).unwrap();
    let compressed = zstd::encode_all(binary_data.as_slice(), 9).unwrap();

    File::create(cache_dir.join(API_MANIFEST_FILENAME))
      .unwrap()
      .write_all(&compressed)
      .unwrap();

    let rt = Runtime::new().unwrap();
    let result = rt.block_on(get_manifest_from_disk_with_limit(cache_dir, 4));

    assert!(matches!(result, Err(Error::SizeLimitExceeded { .. })));
  }

  #[test]
  fn test_read_cap_defaults_to_download_cap() {
    let temp_dir = tempdir().unwrap();

    // Left unset, the cache read-back cap tracks the download cap, so a manifest
    // that passes the download cap is always readable back from cache.
    let client = ThunderstoreClient::builder()
      .community("valheim")
      .cache_dir(temp_dir.path())
      .build()
      .unwrap();

    assert_eq!(
      client.max_manifest_decompressed_bytes as u64,
      DEFAULT_MAX_MANIFEST_BYTES
    );

    // Raising only the download cap raises the read cap with it.
    let client = ThunderstoreClient::builder()
      .community("valheim")
      .cache_dir(temp_dir.path())
      .manifest_max_bytes(900 * 1024 * 1024)
      .build()
      .unwrap();

    assert_eq!(
      client.max_manifest_decompressed_bytes as u64,
      900 * 1024 * 1024
    );

    // An explicit read cap overrides the tracking default (e.g. to bound decode
    // allocation more tightly than the download cap).
    let client = ThunderstoreClient::builder()
      .community("valheim")
      .cache_dir(temp_dir.path())
      .manifest_max_bytes(900 * 1024 * 1024)
      .manifest_max_decompressed_bytes(64 * 1024 * 1024)
      .build()
      .unwrap();

    assert_eq!(client.max_manifest_decompressed_bytes, 64 * 1024 * 1024);
  }

  #[test]
  fn test_large_cache_roundtrips_within_cap_and_rejects_below() {
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    // A multi-package index with distinct strings, so the interned cache is a
    // realistic (if far smaller) stand-in for a large community's cache rather than
    // a trivial one-package blob.
    let packages: Vec<Package> = (0..500)
      .map(|i| {
        sample_package(
          &format!("Mod{i}"),
          &format!("Owner{i}-Mod{i}"),
          &format!("Owner{i}"),
        )
      })
      .collect();
    let count = packages.len();

    let interned: PackageIndex = packages.into();
    let serializable: SerializedPackageIndex = (&interned).into();
    let binary_data = bincode::encode_to_vec(&serializable, bincode::config::standard()).unwrap();
    let decompressed_len = binary_data.len();
    let compressed = zstd::encode_all(binary_data.as_slice(), 9).unwrap();

    File::create(cache_dir.join(API_MANIFEST_FILENAME))
      .unwrap()
      .write_all(&compressed)
      .unwrap();

    let rt = Runtime::new().unwrap();

    // A cap at least the decompressed size reads the whole cache back.
    let manifest = rt
      .block_on(get_manifest_from_disk_with_limit(
        cache_dir,
        decompressed_len,
      ))
      .unwrap();

    assert_eq!(manifest.len(), count);

    // The default cap comfortably accommodates a real community's cache, so the
    // same cache also round-trips under the default.
    assert!(decompressed_len < DEFAULT_MAX_MANIFEST_DECOMPRESSED_BYTES);

    let manifest = rt
      .block_on(get_manifest_from_disk_with_limit(
        cache_dir,
        DEFAULT_MAX_MANIFEST_DECOMPRESSED_BYTES,
      ))
      .unwrap();

    assert_eq!(manifest.len(), count);

    // A cap below the decompressed size still rejects the cache, preserving the
    // decompression-bomb guard at the configured boundary.
    let result = rt.block_on(get_manifest_from_disk_with_limit(
      cache_dir,
      decompressed_len - 1,
    ));

    assert!(matches!(result, Err(Error::SizeLimitExceeded { .. })));
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
      None,
      &RetryPolicy::none(),
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
      assert!(api_manifest_path(cache_dir).exists());

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
      None,
      &RetryPolicy::none(),
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
      None,
      &RetryPolicy::none(),
      &NoopProgress,
    ));

    assert!(result2.is_ok());
  }

  #[test]
  fn test_download_file_without_content_length_succeeds() {
    let mut server = Server::new();
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    let test_data = b"This is test file content";

    // No Content-Length header (as with chunked transfer encoding). The download
    // must still succeed, treating a cleanly finished stream as complete.
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

    let result = rt.block_on(download_file(
      &client,
      &file_url,
      &filename,
      cache_dir,
      None,
      &RetryPolicy::none(),
      &NoopProgress,
    ));

    assert!(result.is_ok());

    let downloaded = cache_dir.join("downloads").join(&filename);

    assert!(downloaded.exists());
    assert_eq!(std::fs::read(&downloaded).unwrap(), test_data);

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
      None,
      RetryPolicy::none(),
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
      None,
      RetryPolicy::none(),
      Arc::new(NoopProgress),
    ));

    assert!(result.is_ok());
  }

  #[test]
  fn test_download_files_reports_failure() {
    let mut server = Server::new();
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    let mock = server.mock("GET", "/missing.zip").with_status(500).create();

    let mut urls = HashMap::new();
    urls.insert(
      "missing.zip".to_string(),
      format!("{}/missing.zip", server.url()),
    );

    let rt = Runtime::new().unwrap();
    let result = rt.block_on(download_files(
      urls,
      cache_dir,
      &test_http(),
      2,
      None,
      RetryPolicy::none(),
      Arc::new(NoopProgress),
    ));

    assert!(result.is_err());

    mock.assert();
  }

  #[test]
  fn test_download_file_rejects_unsafe_filename() {
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    let rt = Runtime::new().unwrap();
    let result = rt.block_on(download_file(
      &test_http(),
      "http://example.invalid/x.zip",
      "../escape.zip",
      cache_dir,
      None,
      &RetryPolicy::none(),
      &NoopProgress,
    ));

    assert!(result.is_err());
  }

  #[test]
  fn test_download_file_rejects_declared_oversize() {
    let mut server = Server::new();
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    // The server declares a Content-Length larger than the cap, so the download
    // is rejected up front without streaming the body.
    let body = vec![0u8; 100];
    let mock = server
      .mock("GET", "/big.zip")
      .with_status(200)
      .with_header("Content-Length", "100")
      .with_body(&body)
      .create();

    let file_url = format!("{}/big.zip", server.url());

    let rt = Runtime::new().unwrap();
    let result = rt.block_on(download_file(
      &test_http(),
      &file_url,
      "big.zip",
      cache_dir,
      Some(8),
      &RetryPolicy::none(),
      &NoopProgress,
    ));

    assert!(matches!(result, Err(Error::SizeLimitExceeded { .. })));
    assert!(!cache_dir.join("downloads").join("big.zip").exists());

    mock.assert();
  }

  #[test]
  fn test_download_file_rejects_streamed_oversize() {
    let mut server = Server::new();
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    // A chunked body carries no Content-Length, so the cap must be enforced from
    // the actual streamed bytes rather than a declared size.
    let mock = server
      .mock("GET", "/stream.zip")
      .with_status(200)
      .with_chunked_body(|w| w.write_all(&[0u8; 100]))
      .create();

    let file_url = format!("{}/stream.zip", server.url());

    let rt = Runtime::new().unwrap();
    let result = rt.block_on(download_file(
      &test_http(),
      &file_url,
      "stream.zip",
      cache_dir,
      Some(8),
      &RetryPolicy::none(),
      &NoopProgress,
    ));

    assert!(matches!(result, Err(Error::SizeLimitExceeded { .. })));

    let downloads_dir = cache_dir.join("downloads");
    assert!(!downloads_dir.join("stream.zip").exists());

    let leftover = std::fs::read_dir(&downloads_dir)
      .unwrap()
      .filter_map(|e| e.ok())
      .any(|e| e.file_name().to_string_lossy().contains(".tmp"));

    assert!(!leftover, "no .tmp temp files should remain");

    mock.assert();
  }

  fn fast_retry(max_retries: u32) -> RetryPolicy {
    RetryPolicy {
      max_retries,
      base_delay: Duration::ZERO,
      max_delay: Duration::ZERO,
    }
  }

  #[test]
  fn test_download_file_retries_transient_5xx() {
    let mut server = Server::new();
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    // Every attempt gets a 500; with 2 retries the endpoint is hit 3 times.
    let mock = server
      .mock("GET", "/flaky.zip")
      .with_status(500)
      .expect(3)
      .create();
    let file_url = format!("{}/flaky.zip", server.url());

    let rt = Runtime::new().unwrap();
    let result = rt.block_on(download_file(
      &test_http(),
      &file_url,
      "flaky.zip",
      cache_dir,
      None,
      &fast_retry(2),
      &NoopProgress,
    ));

    assert!(result.is_err());

    // Exactly initial + 2 retries.
    mock.assert();
  }

  #[test]
  fn test_download_file_does_not_retry_4xx() {
    let mut server = Server::new();
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    // A 404 is terminal: even with a generous retry budget it is hit once.
    let mock = server
      .mock("GET", "/missing.zip")
      .with_status(404)
      .expect(1)
      .create();
    let file_url = format!("{}/missing.zip", server.url());

    let rt = Runtime::new().unwrap();
    let result = rt.block_on(download_file(
      &test_http(),
      &file_url,
      "missing.zip",
      cache_dir,
      None,
      &fast_retry(5),
      &NoopProgress,
    ));

    assert!(result.is_err());

    mock.assert();
  }

  /// A [`ProgressReporter`] that records every absolute position reset, used to
  /// assert the bar is rewound to zero at the start of each retried attempt.
  struct RecordingProgress {
    positions: Arc<std::sync::Mutex<Vec<u64>>>,
  }

  impl RecordingProgress {
    fn new() -> Self {
      Self {
        positions: Arc::new(std::sync::Mutex::new(Vec::new())),
      }
    }
  }

  impl ProgressReporter for RecordingProgress {
    fn begin(&self, _label: &str, _total: Option<u64>) -> Box<dyn ProgressHandle> {
      Box::new(RecordingHandle {
        positions: self.positions.clone(),
      })
    }
  }

  struct RecordingHandle {
    positions: Arc<std::sync::Mutex<Vec<u64>>>,
  }

  impl ProgressHandle for RecordingHandle {
    fn inc(&self, _delta: u64) {}

    fn set_position(&self, position: u64) {
      self.positions.lock().unwrap().push(position);
    }

    fn set_message(&self, _msg: &str) {}
    fn finish(&self, _msg: Option<&str>) {}
  }

  #[test]
  fn test_download_file_resets_progress_each_attempt() {
    let mut server = Server::new();
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    // Every attempt gets a 500; with 2 retries the closure runs 3 times, so the
    // bar must be reset to zero once at the start of each attempt.
    let mock = server
      .mock("GET", "/flaky.zip")
      .with_status(500)
      .expect(3)
      .create();
    let file_url = format!("{}/flaky.zip", server.url());

    let progress = RecordingProgress::new();

    let rt = Runtime::new().unwrap();
    let result = rt.block_on(download_file(
      &test_http(),
      &file_url,
      "flaky.zip",
      cache_dir,
      None,
      &fast_retry(2),
      &progress,
    ));

    assert!(result.is_err());

    let positions = progress.positions.lock().unwrap();

    // One `set_position(0)` per attempt (initial + 2 retries).
    assert_eq!(positions.as_slice(), &[0, 0, 0]);

    mock.assert();
  }

  #[test]
  fn test_get_manifest_from_network_and_cache_tolerates_marker_write_failure() {
    let mut server = Server::new();
    let test_date = "Wed, 21 Feb 2024 15:30:45 GMT";

    let mock = server
      .mock("GET", "/pkg/")
      .with_status(200)
      .with_header("Content-Type", "application/json")
      .with_header("Last-Modified", test_date)
      .with_body(sample_manifest_json())
      .create();

    let api_url = format!("{}/pkg/", server.url());
    let temp_dir = tempdir().unwrap();
    let cache_dir = temp_dir.path();

    // Occupy the marker path with a directory so the marker write fails while the
    // manifest blob still persists. A marker-write failure must not discard the
    // already-cached manifest.
    std::fs::create_dir(last_modified_path(cache_dir)).unwrap();

    let rt = Runtime::new().unwrap();
    let result = rt.block_on(get_manifest_from_network_and_cache(
      cache_dir,
      &test_http(),
      &api_url,
      None,
      &RetryPolicy::none(),
      &NoopProgress,
    ));

    assert!(result.is_ok());

    let manifest = result.unwrap();

    assert_eq!(manifest.len(), 1);
    assert!(api_manifest_path(cache_dir).exists());

    // The marker write really did fail (the path is still the directory we
    // planted), confirming the tolerated branch was exercised.
    assert!(last_modified_path(cache_dir).is_dir());

    mock.assert();
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

  #[test]
  fn create_profile_code_posts_payload_and_returns_key() {
    let mut server = Server::new();

    let mock = server
      .mock("POST", "/api/experimental/legacyprofile/create/")
      .match_header("content-type", "application/octet-stream")
      .match_body(mockito::Matcher::Exact("#r2modman\nQUJD".to_string()))
      .with_status(200)
      .with_header("Content-Type", "application/json")
      .with_body(r#"{"key":"abc-123"}"#)
      .create();

    let cache = tempdir().unwrap();
    let client = ThunderstoreClient::builder()
      .base_url(server.url())
      .community("valheim")
      .cache_dir(cache.path())
      .build()
      .unwrap();

    let rt = Runtime::new().unwrap();
    let key = rt
      .block_on(client.create_profile_code(b"#r2modman\nQUJD"))
      .unwrap();

    assert_eq!(key, "abc-123");
    mock.assert();
  }

  #[test]
  fn fetch_profile_code_returns_body_bytes() {
    let mut server = Server::new();

    let mock = server
      .mock("GET", "/api/experimental/legacyprofile/get/abc-123/")
      .with_status(200)
      .with_body("#r2modman\nQUJD")
      .create();

    let cache = tempdir().unwrap();
    let client = ThunderstoreClient::builder()
      .base_url(server.url())
      .community("valheim")
      .cache_dir(cache.path())
      .build()
      .unwrap();

    let rt = Runtime::new().unwrap();
    let body = rt.block_on(client.fetch_profile_code("abc-123")).unwrap();

    assert_eq!(body, b"#r2modman\nQUJD");
    mock.assert();
  }

  #[test]
  fn fetch_profile_code_rejects_unsafe_code() {
    let cache = tempdir().unwrap();
    let client = ThunderstoreClient::builder()
      .community("valheim")
      .cache_dir(cache.path())
      .build()
      .unwrap();

    let rt = Runtime::new().unwrap();

    // A code is interpolated into the URL path, so it must not inject segments.
    assert!(matches!(
      rt.block_on(client.fetch_profile_code("../../evil")),
      Err(Error::Config(_))
    ));
  }
}
