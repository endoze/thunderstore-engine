#![deny(missing_docs)]

use crate::error::Error;
use crate::intern::{
  InternKey, StringInterner, intern_option, intern_vec, resolve_option, resolve_vec,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use time::OffsetDateTime;

/// An in-memory index of every package (and all of their versions) in a
/// Thunderstore community.
///
/// This is the crate's central read model. Obtain one from
/// [`ThunderstoreClient::get_manifest`](crate::client::ThunderstoreClient::get_manifest)
/// or [`refresh_index`](crate::client::ThunderstoreClient::refresh_index), then
/// query it for packages, versions, and download URLs.
///
/// Packages are addressed by a numeric index in the range `0..len()`. To look up
/// a package by its `Owner-Name` identifier, build a map once with
/// [`build_name_index`](PackageIndex::build_name_index), or use
/// [`get_package_by_full_name`](PackageIndex::get_package_by_full_name). To pull a
/// single field without materializing a whole [`Package`], use the `resolve_*_at`
/// accessors.
///
/// A `PackageIndex` is built from a `Vec<Package>` (via [`From`]) — for example
/// the JSON returned by the Thunderstore API — and is what the client persists to
/// and loads from its on-disk cache.
#[derive(Debug, Clone)]
pub struct PackageIndex {
  pub(crate) interner: StringInterner,
  pub(crate) names: Vec<Option<InternKey>>,
  pub(crate) full_names: Vec<Option<InternKey>>,
  pub(crate) owners: Vec<Option<InternKey>>,
  pub(crate) package_urls: Vec<Option<InternKey>>,
  pub(crate) dates_created: Vec<OffsetDateTime>,
  pub(crate) dates_updated: Vec<OffsetDateTime>,
  pub(crate) uuid4s: Vec<Option<InternKey>>,
  pub(crate) rating_scores: Vec<Option<u32>>,
  pub(crate) is_pinned: Vec<Option<bool>>,
  pub(crate) is_deprecated: Vec<Option<bool>>,
  pub(crate) has_nsfw_content: Vec<Option<bool>>,
  pub(crate) categories: Vec<Vec<InternKey>>,
  pub(crate) version_ranges: Vec<(usize, usize)>,
  pub(crate) versions: VersionTable,
}

/// Struct-of-Arrays representation of version data with interned strings.
///
/// Separates frequently accessed "hot" data from rarely accessed "cold" metadata
/// for better cache performance and memory efficiency. String values are stored as
/// interned keys that reference the index's string interner.
#[derive(Debug, Clone)]
pub(crate) struct VersionTable {
  pub package_indices: Vec<usize>,
  pub names: Vec<Option<InternKey>>,
  pub full_names: Vec<Option<InternKey>>,
  pub version_numbers: Vec<Option<InternKey>>,
  pub download_urls: Vec<Option<InternKey>>,
  pub dependencies: Vec<Vec<InternKey>>,
  pub dates_created: Vec<OffsetDateTime>,
  pub descriptions: Vec<Option<InternKey>>,
  pub icons: Vec<Option<InternKey>>,
  pub downloads: Vec<Option<u32>>,
  pub website_urls: Vec<Option<InternKey>>,
  pub is_active: Vec<Option<bool>>,
  pub uuid4s: Vec<Option<InternKey>>,
  pub file_sizes: Vec<Option<u64>>,
}

/// The on-disk cache format version. `bincode` is not self-describing, so this
/// leading tag lets a build reject a cache written by an incompatible layout
/// (rather than silently misdecoding it) — bump it on any change to the
/// serialized field layout.
///
/// Version 2 switched the cache from the `bincode` serde bridge to `bincode`'s
/// native `Encode`/`Decode` (so container allocation is bounded by the decode
/// limit) and stores timestamps as `i128` Unix-nanosecond values.
pub(crate) const CACHE_FORMAT_VERSION: u32 = 2;

/// On-disk (`bincode`) representation of a [`PackageIndex`].
///
/// This structure converts the runtime PackageIndex into a format that can be
/// efficiently serialized and deserialized. The string interner is converted to a simple
/// vector of strings (the string table), and all interned keys become u32 indices into
/// this table.
///
/// This separation allows for optimal serialization performance while maintaining the
/// memory efficiency benefits of string interning at runtime.
///
/// This is the on-disk cache encoding written and read by [`crate::client`]
/// (`zstd` + `bincode`). Each field parallels the same-named field of
/// [`PackageIndex`], with interned keys stored as `u32` indices into
/// `string_table`.
///
/// Encoded and decoded with `bincode`'s native `Encode`/`Decode` (not the serde
/// bridge) so the decode limit bounds container allocation, and timestamps are
/// stored as `i128` Unix nanoseconds (see [`encode_dates`]/[`decode_dates`])
/// because `time::OffsetDateTime` is not a native `bincode` type.
#[derive(bincode::Encode, bincode::Decode, Debug, Clone)]
pub(crate) struct SerializedPackageIndex {
  /// Cache-format tag; must equal [`CACHE_FORMAT_VERSION`]. Kept first so it is
  /// the first value decoded from the stream.
  pub format_version: u32,
  pub string_table: Vec<String>,
  pub names: Vec<Option<u32>>,
  pub full_names: Vec<Option<u32>>,
  pub owners: Vec<Option<u32>>,
  pub package_urls: Vec<Option<u32>>,
  /// Package creation/update times as `i128` Unix nanoseconds.
  pub dates_created: Vec<i128>,
  pub dates_updated: Vec<i128>,
  pub uuid4s: Vec<Option<u32>>,
  pub rating_scores: Vec<Option<u32>>,
  pub is_pinned: Vec<Option<bool>>,
  pub is_deprecated: Vec<Option<bool>>,
  pub has_nsfw_content: Vec<Option<bool>>,
  pub categories: Vec<Vec<u32>>,
  pub version_ranges: Vec<(usize, usize)>,
  pub versions: SerializedVersionTable,
}

/// Serializable representation of interned version data.
///
/// Contains version information with string values represented as u32 indices into
/// the parent manifest's string table. This allows efficient serialization while
/// maintaining the benefits of string interning.
#[derive(bincode::Encode, bincode::Decode, Debug, Clone)]
pub(crate) struct SerializedVersionTable {
  pub package_indices: Vec<usize>,
  pub names: Vec<Option<u32>>,
  pub full_names: Vec<Option<u32>>,
  pub version_numbers: Vec<Option<u32>>,
  pub download_urls: Vec<Option<u32>>,
  pub dependencies: Vec<Vec<u32>>,
  /// Version creation times as `i128` Unix nanoseconds.
  pub dates_created: Vec<i128>,
  pub descriptions: Vec<Option<u32>>,
  pub icons: Vec<Option<u32>>,
  pub downloads: Vec<Option<u32>>,
  pub website_urls: Vec<Option<u32>>,
  pub is_active: Vec<Option<bool>>,
  pub uuid4s: Vec<Option<u32>>,
  pub file_sizes: Vec<Option<u64>>,
}

impl PackageIndex {
  /// Returns the number of packages in the index.
  pub fn len(&self) -> usize {
    self.names.len()
  }

  /// Returns true if the index contains no packages.
  #[allow(dead_code)]
  pub fn is_empty(&self) -> bool {
    self.names.is_empty()
  }

  /// Returns the short package name at `idx`, or `None` if `idx` is out of range
  /// or the name is not set.
  pub fn resolve_name_at(&self, idx: usize) -> Option<String> {
    resolve_option(&self.interner, *self.names.get(idx)?)
  }

  /// Returns the full package name (`Owner-Name`) at `idx`, or `None` if `idx` is
  /// out of range or the name is not set.
  pub fn resolve_full_name_at(&self, idx: usize) -> Option<String> {
    resolve_option(&self.interner, *self.full_names.get(idx)?)
  }

  /// The indices of every package whose name or `Owner-Name` full name contains
  /// `term`, case-insensitively.
  ///
  /// Returns indices rather than references so a caller can resolve only the
  /// fields it needs from the interned index. An empty `term` matches every
  /// package, so clearing a search box shows the full list rather than nothing.
  pub fn search(&self, term: &str) -> Vec<usize> {
    let needle = term.to_lowercase();

    (0..self.len())
      .filter(|&idx| {
        if let Some(name) = self.resolve_name_at(idx)
          && name.to_lowercase().contains(&needle)
        {
          return true;
        }

        self
          .resolve_full_name_at(idx)
          .is_some_and(|full_name| full_name.to_lowercase().contains(&needle))
      })
      .collect()
  }

  /// Returns the owner of the package at `idx`, or `None` if `idx` is out of
  /// range or the owner is not set.
  pub fn resolve_owner_at(&self, idx: usize) -> Option<String> {
    resolve_option(&self.interner, *self.owners.get(idx)?)
  }

  /// Borrows the short package name at `idx`, or `None` if `idx` is out of range
  /// or the name is not set.
  ///
  /// Unlike [`resolve_name_at`](Self::resolve_name_at), this borrows from the
  /// index's interner and performs no allocation.
  pub fn name_at(&self, idx: usize) -> Option<&str> {
    self
      .names
      .get(idx)
      .copied()
      .flatten()
      .map(|k| self.interner.resolve(&k))
  }

  /// Borrows the full package name (`Owner-Name`) at `idx`, or `None` if `idx` is
  /// out of range or the name is not set.
  ///
  /// Unlike [`resolve_full_name_at`](Self::resolve_full_name_at), this borrows
  /// from the index's interner and performs no allocation.
  pub fn full_name_at(&self, idx: usize) -> Option<&str> {
    self
      .full_names
      .get(idx)
      .copied()
      .flatten()
      .map(|k| self.interner.resolve(&k))
  }

  /// Borrows the owner of the package at `idx`, or `None` if `idx` is out of
  /// range or the owner is not set.
  ///
  /// Unlike [`resolve_owner_at`](Self::resolve_owner_at), this borrows from the
  /// index's interner and performs no allocation.
  pub fn owner_at(&self, idx: usize) -> Option<&str> {
    self
      .owners
      .get(idx)
      .copied()
      .flatten()
      .map(|k| self.interner.resolve(&k))
  }

  /// Looks up a package by its full name (`Owner-Name`) and materializes it,
  /// returning `None` if no package matches.
  #[allow(dead_code)]
  pub fn get_package_by_full_name(&self, full_name: &str) -> Option<Package> {
    let idx = self.find_index_by_full_name(full_name)?;

    self.get_package_at(idx)
  }

  /// Returns the index of the package with the given full name (`Owner-Name`),
  /// or `None` if no package matches.
  #[allow(dead_code)]
  pub fn find_index_by_full_name(&self, full_name: &str) -> Option<usize> {
    self.full_names.iter().position(|key| {
      key
        .map(|k| self.interner.resolve(&k) == full_name)
        .unwrap_or(false)
    })
  }

  /// Materializes a full [`Package`], including all of its versions, from the
  /// package at `idx`, or `None` if `idx` is out of range.
  #[allow(dead_code)]
  pub fn get_package_at(&self, idx: usize) -> Option<Package> {
    let (version_start, version_end) = *self.version_ranges.get(idx)?;
    let mut versions = Vec::with_capacity(version_end - version_start);

    for ver_idx in version_start..version_end {
      versions.push(Version {
        name: resolve_option(&self.interner, self.versions.names[ver_idx]),
        full_name: resolve_option(&self.interner, self.versions.full_names[ver_idx]),
        description: resolve_option(&self.interner, self.versions.descriptions[ver_idx]),
        icon: resolve_option(&self.interner, self.versions.icons[ver_idx]),
        version_number: resolve_option(&self.interner, self.versions.version_numbers[ver_idx]),
        dependencies: resolve_vec(&self.interner, &self.versions.dependencies[ver_idx]),
        download_url: resolve_option(&self.interner, self.versions.download_urls[ver_idx]),
        downloads: self.versions.downloads[ver_idx],
        date_created: self.versions.dates_created[ver_idx],
        website_url: resolve_option(&self.interner, self.versions.website_urls[ver_idx]),
        is_active: self.versions.is_active[ver_idx],
        uuid4: resolve_option(&self.interner, self.versions.uuid4s[ver_idx]),
        file_size: self.versions.file_sizes[ver_idx],
      });
    }

    Some(Package {
      name: resolve_option(&self.interner, self.names[idx]),
      full_name: resolve_option(&self.interner, self.full_names[idx]),
      owner: resolve_option(&self.interner, self.owners[idx]),
      package_url: resolve_option(&self.interner, self.package_urls[idx]),
      date_created: self.dates_created[idx],
      date_updated: self.dates_updated[idx],
      uuid4: resolve_option(&self.interner, self.uuid4s[idx]),
      rating_score: self.rating_scores[idx],
      is_pinned: self.is_pinned[idx],
      is_deprecated: self.is_deprecated[idx],
      has_nsfw_content: self.has_nsfw_content[idx],
      categories: resolve_vec(&self.interner, &self.categories[idx]),
      versions,
    })
  }

  /// Returns the index of the latest version of the package at `idx`, chosen by
  /// most recent creation date, or `None` if `idx` is out of range or the
  /// package has no versions.
  ///
  /// The returned value is an index into the internal (crate-private) version
  /// table, so this is `pub(crate)`: exposing it publicly would hand callers an
  /// opaque `usize` that no public accessor consumes and that is trivially
  /// confused with a package index.
  pub(crate) fn get_latest_version_at(&self, idx: usize) -> Option<usize> {
    let (ver_start, ver_end) = *self.version_ranges.get(idx)?;

    if ver_start >= ver_end {
      return None;
    }

    (ver_start..ver_end).max_by_key(|&ver_idx| self.versions.dates_created[ver_idx])
  }

  /// Builds a map from each package's full name (`Owner-Name`) to its index,
  /// for repeated by-name lookups.
  pub fn build_name_index(&self) -> HashMap<String, usize> {
    self
      .full_names
      .iter()
      .enumerate()
      .filter_map(|(idx, key)| {
        let name = resolve_option(&self.interner, *key)?;
        Some((name, idx))
      })
      .collect()
  }

  /// Checks the index for internal consistency.
  ///
  /// Returns `Ok(())` if the index is well-formed, otherwise an
  /// [`Error::Cache`] describing the first problem found. The client runs this
  /// automatically when loading a cached index, so a corrupt or truncated cache
  /// surfaces as a recoverable error rather than a later panic.
  pub fn validate(&self) -> crate::Result<()> {
    self.validate_lengths()?;
    self.validate_version_ranges()?;
    self.validate_version_ownership()?;
    self.validate_unique_full_names()?;

    Ok(())
  }

  /// Checks that every parallel struct-of-arrays vector has the length its group
  /// requires: package-level vectors match the package count, version-level
  /// vectors match the version count.
  fn validate_lengths(&self) -> crate::Result<()> {
    let pkg_count = self.names.len();

    check_len("full_names", self.full_names.len(), pkg_count)?;
    check_len("owners", self.owners.len(), pkg_count)?;
    check_len("package_urls", self.package_urls.len(), pkg_count)?;
    check_len("dates_created", self.dates_created.len(), pkg_count)?;
    check_len("dates_updated", self.dates_updated.len(), pkg_count)?;
    check_len("uuid4s", self.uuid4s.len(), pkg_count)?;
    check_len("rating_scores", self.rating_scores.len(), pkg_count)?;
    check_len("is_pinned", self.is_pinned.len(), pkg_count)?;
    check_len("is_deprecated", self.is_deprecated.len(), pkg_count)?;
    check_len("has_nsfw_content", self.has_nsfw_content.len(), pkg_count)?;
    check_len("categories", self.categories.len(), pkg_count)?;
    check_len("version_ranges", self.version_ranges.len(), pkg_count)?;

    let version_count = self.versions.package_indices.len();

    check_len("versions.names", self.versions.names.len(), version_count)?;
    check_len(
      "versions.full_names",
      self.versions.full_names.len(),
      version_count,
    )?;
    check_len(
      "versions.version_numbers",
      self.versions.version_numbers.len(),
      version_count,
    )?;
    check_len(
      "versions.download_urls",
      self.versions.download_urls.len(),
      version_count,
    )?;
    check_len(
      "versions.dependencies",
      self.versions.dependencies.len(),
      version_count,
    )?;
    check_len(
      "versions.dates_created",
      self.versions.dates_created.len(),
      version_count,
    )?;
    check_len(
      "versions.descriptions",
      self.versions.descriptions.len(),
      version_count,
    )?;
    check_len("versions.icons", self.versions.icons.len(), version_count)?;
    check_len(
      "versions.downloads",
      self.versions.downloads.len(),
      version_count,
    )?;
    check_len(
      "versions.website_urls",
      self.versions.website_urls.len(),
      version_count,
    )?;
    check_len(
      "versions.is_active",
      self.versions.is_active.len(),
      version_count,
    )?;
    check_len("versions.uuid4s", self.versions.uuid4s.len(), version_count)?;
    check_len(
      "versions.file_sizes",
      self.versions.file_sizes.len(),
      version_count,
    )?;

    Ok(())
  }

  /// Checks that every version range is ordered and in-bounds, and that every
  /// version's recorded `package_index` refers to a real package. Must run before
  /// [`validate_version_ownership`](Self::validate_version_ownership), which
  /// slices `package_indices` by these ranges.
  fn validate_version_ranges(&self) -> crate::Result<()> {
    let pkg_count = self.names.len();
    let version_count = self.versions.package_indices.len();

    for (idx, (start, end)) in self.version_ranges.iter().enumerate() {
      if start > end {
        return Err(Error::Cache(format!(
          "Invalid version range at package {}: {} > {}",
          idx, start, end
        )));
      }

      if *end > version_count {
        return Err(Error::Cache(format!(
          "Version range at package {} ends at {} but only {} versions exist",
          idx, end, version_count
        )));
      }
    }

    for (i, &pkg_idx) in self.versions.package_indices.iter().enumerate() {
      if pkg_idx >= pkg_count {
        return Err(Error::Cache(format!(
          "version {} references package index {} but only {} packages exist",
          i, pkg_idx, pkg_count
        )));
      }
    }

    Ok(())
  }

  /// Checks that every version is owned by exactly one package: its slot falls
  /// inside exactly one package's contiguous range, and that package's recorded
  /// `package_indices` entry agrees. This rejects a corrupt cache whose ranges are
  /// individually in-bounds but overlap, leave gaps, or disagree with
  /// `package_indices` — which would otherwise attach the wrong versions (and
  /// wrong download URLs) to a package.
  fn validate_version_ownership(&self) -> crate::Result<()> {
    let version_count = self.versions.package_indices.len();

    let mut owner_of: Vec<Option<usize>> = vec![None; version_count];

    for (pkg_idx, range) in self.version_ranges.iter().enumerate() {
      let (start, end) = *range;

      for (offset, &owner_pkg) in self.versions.package_indices[start..end].iter().enumerate() {
        let ver_idx = start + offset;

        if owner_of[ver_idx].is_some() {
          return Err(Error::Cache(format!(
            "version {} is claimed by multiple packages (overlapping version ranges)",
            ver_idx
          )));
        }

        owner_of[ver_idx] = Some(pkg_idx);

        if owner_pkg != pkg_idx {
          return Err(Error::Cache(format!(
            "version {} lies in package {}'s range but records package_index {}",
            ver_idx, pkg_idx, owner_pkg
          )));
        }
      }
    }

    for (ver_idx, owner) in owner_of.iter().enumerate() {
      if owner.is_none() {
        return Err(Error::Cache(format!(
          "version {} is not covered by any package's version range",
          ver_idx
        )));
      }
    }

    Ok(())
  }

  /// Checks that no full name repeats. Thunderstore guarantees full names are
  /// unique, so a well-formed index never repeats one. Reject a corrupt or crafted
  /// cache that does: otherwise `find_index_by_full_name` (first match) and
  /// `build_name_index` (last match) could resolve the same name to different
  /// packages, and the inspected package would differ from the installed one.
  fn validate_unique_full_names(&self) -> crate::Result<()> {
    let mut seen_full_names: HashSet<&str> = HashSet::new();

    for key in self.full_names.iter().flatten() {
      let full_name = self.interner.resolve(key);

      if !seen_full_names.insert(full_name) {
        return Err(Error::Cache(format!(
          "duplicate full name {:?} appears in multiple packages",
          full_name
        )));
      }
    }

    Ok(())
  }
}

/// Errors with a `{field} length {actual} != {expected}` message when a parallel
/// struct-of-arrays vector's length does not match the count its group requires.
fn check_len(field: &str, actual: usize, expected: usize) -> crate::Result<()> {
  if actual != expected {
    return Err(Error::Cache(format!(
      "{field} length {actual} != {expected}"
    )));
  }

  Ok(())
}

/// Represents a mod package from the Thunderstore API.
///
/// This struct contains information about a mod package, including its metadata
/// and all available versions.
///
/// `#[non_exhaustive]`: fields may be added as the Thunderstore API grows, so
/// downstream code cannot construct or exhaustively destructure this directly.
#[derive(Serialize, Deserialize, Clone)]
#[non_exhaustive]
pub struct Package {
  /// The short name of the package
  pub name: Option<String>,
  /// The full name of the package, typically in format "Owner-Name"
  pub full_name: Option<String>,
  /// The username of the package owner
  pub owner: Option<String>,
  /// The URL to the package's page on Thunderstore
  pub package_url: Option<String>,
  /// When the package was first published
  #[serde(with = "time::serde::rfc3339")]
  pub date_created: time::OffsetDateTime,
  /// When the package was last updated
  #[serde(with = "time::serde::rfc3339")]
  pub date_updated: time::OffsetDateTime,
  /// Unique identifier for the package
  pub uuid4: Option<String>,
  /// User rating score for the package
  pub rating_score: Option<u32>,
  /// Whether the package is pinned by Thunderstore
  pub is_pinned: Option<bool>,
  /// Whether the package is marked as deprecated
  pub is_deprecated: Option<bool>,
  /// Whether the package contains NSFW content
  pub has_nsfw_content: Option<bool>,
  /// List of categories the package belongs to
  pub categories: Vec<String>,
  /// All available versions of the package
  pub versions: Vec<Version>,
}

/// Represents a specific version of a mod package.
///
/// This struct contains information about one version of a package,
/// including its version number, dependencies, and download information.
///
/// `#[non_exhaustive]`: fields may be added as the Thunderstore API grows, so
/// downstream code cannot construct or exhaustively destructure this directly.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[non_exhaustive]
pub struct Version {
  /// The name of this version
  pub name: Option<String>,
  /// The full name of this version
  pub full_name: Option<String>,
  /// The description of this version
  pub description: Option<String>,
  /// URL to the icon for this version
  pub icon: Option<String>,
  /// The version number (e.g., "1.0.0")
  pub version_number: Option<String>,
  /// List of dependencies required by this version
  pub dependencies: Vec<String>,
  /// URL to download this version
  pub download_url: Option<String>,
  /// Number of times this version has been downloaded
  pub downloads: Option<u32>,
  /// When this version was published
  #[serde(with = "time::serde::rfc3339")]
  pub date_created: time::OffsetDateTime,
  /// URL to the website for this version
  pub website_url: Option<String>,
  /// Whether this version is active
  pub is_active: Option<bool>,
  /// Unique identifier for this version
  pub uuid4: Option<String>,
  /// Size of the download file in bytes
  pub file_size: Option<u64>,
}

impl Package {
  /// The most recently created version, or `None` for a package with no
  /// versions.
  ///
  /// Ordering is by `date_created`, not by version number, so a republished
  /// older version wins over a higher version string. This matches how
  /// Thunderstore itself presents "latest".
  pub fn latest_version(&self) -> Option<&Version> {
    self.versions.iter().max_by_key(|v| v.date_created)
  }

  /// The `Owner-PackageName-1.0.0.zip` filename and download URL for the latest
  /// version, or `None` if the package has no version or that version carries no
  /// download URL.
  ///
  /// The filename is the archive name the download layer writes to disk, so it
  /// must stay in the `Owner-Name-Version.zip` shape
  /// [`crate::extract::parse_package_filename`] reads back.
  #[cfg(test)]
  pub fn zip_and_url(&self) -> Option<(String, String)> {
    let pkg = self.latest_version()?;
    let url: String = pkg.download_url.clone()?;
    let version_number = pkg.version_number.as_ref()?;
    let package_name = &self.full_name.as_ref()?;
    let zip_name = format!("{}-{}.zip", package_name, version_number);

    Some((zip_name, url))
  }
}

/// Shows only the most relevant fields: the full name, and the latest version's
/// dependencies and download URL.
impl std::fmt::Debug for Package {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Package")
      .field("full_name", &self.full_name)
      .field(
        "dependencies",
        &self.latest_version().map(|v| &v.dependencies),
      )
      .field(
        "download_url",
        &self.latest_version().map(|v| &v.download_url),
      )
      .finish()
  }
}

/// The outcome of [`DependencyGraph::resolve_report`]: the packages that
/// resolved to a download, plus the names that did not.
///
/// `#[non_exhaustive]`: fields may be added as resolution reporting grows, so
/// downstream code cannot construct or exhaustively destructure this directly.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct Resolution {
  /// Map of `Owner-Name-Version.zip` filename to download URL for every package
  /// (requested or dependency) that resolved to a version in the index.
  pub urls: HashMap<String, String>,
  /// The `Owner-Name` identifiers that were requested (directly or as a
  /// dependency) but not present in the index, sorted and deduplicated. A
  /// non-empty list means the resulting install is incomplete.
  pub unresolved: Vec<String>,
}

/// Represents a graph of package dependencies to be resolved and installed.
///
/// This structure helps determine which packages need to be installed,
/// including any dependencies they require.
#[derive(Debug, Deserialize, Serialize)]
pub struct DependencyGraph {
  /// The list of package names that the user wants to install
  install_packages: Vec<String>,
}

impl DependencyGraph {
  /// Creates a graph seeded with the packages a caller asked for.
  ///
  /// Names are `Owner-Name`; a trailing `-Version` is accepted but discarded
  /// during resolution (see [`resolve`](Self::resolve)'s limitations). Nothing is
  /// validated here, so an unknown name surfaces only at resolution time.
  pub fn new(install_packages: Vec<String>) -> Self {
    Self { install_packages }
  }

  /// Resolves all dependencies for the requested packages against the package index.
  ///
  /// This method:
  /// 1. Starts with the user-requested packages
  /// 2. For each package, adds its dependencies to the list to process
  /// 3. Continues until all dependencies are resolved
  ///
  /// Returns a map of archive filename to download URL covering the requested
  /// packages and their full transitive closure, ready to hand to
  /// [`crate::client::ThunderstoreClient::download_files`].
  ///
  /// # Limitations
  ///
  /// Resolution is version-agnostic. The version suffix of a dependency string
  /// (e.g. the `1.2.3` in `Owner-Mod-1.2.3`) is discarded and every package
  /// resolves to its *latest* available version. There is no version-constraint
  /// solving, so a package pinned to an older dependency will still receive the
  /// latest one.
  ///
  /// A requested package or dependency that is absent from `index` is silently
  /// skipped here; use [`resolve_report`](Self::resolve_report) to learn which
  /// names failed to resolve so an incomplete install can be surfaced rather than
  /// treated as success.
  pub fn resolve(&self, index: &PackageIndex) -> HashMap<String, String> {
    self.resolve_report(index).urls
  }

  /// Resolves the install set like [`resolve`](Self::resolve), but also reports
  /// every requested package or transitive dependency that could not be resolved
  /// to a download — either absent from `index`, or present but with no
  /// downloadable version.
  ///
  /// A non-empty [`Resolution::unresolved`] means the returned URL map is
  /// incomplete: those names were moderated/removed, misspelled, live in a
  /// different community, or have no downloadable version, and installing only
  /// what resolved would leave a broken mod. Callers that must fail (or warn) on a
  /// missing dependency should use this instead of [`resolve`](Self::resolve),
  /// which discards the report.
  pub fn resolve_report(&self, index: &PackageIndex) -> Resolution {
    let name_index = index.build_name_index();
    let mut sorted_set: BTreeSet<String> = self.install_packages.iter().cloned().collect();
    let mut install_indices = Vec::new();
    let mut seen: HashSet<usize> = HashSet::new();
    let mut unresolved: Vec<String> = Vec::new();

    tracing::debug!("Starting with mod list of: {:#?}", sorted_set);

    while !sorted_set.is_empty() {
      let Some(item) = sorted_set.pop_first() else {
        continue;
      };

      let item = crate::util::full_name_prefix(&item).to_string();

      let Some(&pkg_idx) = name_index.get(&item) else {
        tracing::warn!(
          "requested package {:?} was not found in the index; the install will be incomplete",
          item
        );
        unresolved.push(item);

        continue;
      };

      // Skip packages already processed so a dependency cycle (A->B->A or a
      // self-dependency) terminates instead of re-inserting deps forever.
      if !seen.insert(pkg_idx) {
        continue;
      }

      if let Some(latest_ver_idx) = index.get_latest_version_at(pkg_idx) {
        let deps = resolve_vec(
          &index.interner,
          &index.versions.dependencies[latest_ver_idx],
        );

        for dep in deps {
          tracing::debug!(
            "Found dependency of {:#?} for {:#?} mod",
            dep,
            index.resolve_name_at(pkg_idx).unwrap_or_default(),
          );
          sorted_set.insert(dep);
        }
      }

      install_indices.push(pkg_idx);
    }

    let mut urls = HashMap::new();

    for &idx in &install_indices {
      if let Some((zip_name, url)) = Self::package_zip_and_url(index, idx) {
        urls.insert(zip_name, url);
      } else if let Some(name) = index
        .resolve_full_name_at(idx)
        .or_else(|| index.resolve_name_at(idx))
      {
        // Found in the index but with no downloadable version (no versions, or a
        // latest lacking a URL/version number): it is part of the desired set yet
        // yields no download, so report it rather than dropping it silently.
        tracing::warn!(
          "package {:?} has no downloadable version; the install will be incomplete",
          name
        );
        unresolved.push(name);
      }
    }

    unresolved.sort();
    unresolved.dedup();

    Resolution { urls, unresolved }
  }

  /// Resolves one package index to its `(zip_filename, download_url)`, or `None`
  /// when the package has no version that can be downloaded — it has no versions,
  /// or its latest version is missing a download URL or version number.
  fn package_zip_and_url(index: &PackageIndex, idx: usize) -> Option<(String, String)> {
    let latest_ver_idx = index.get_latest_version_at(idx)?;
    let url = resolve_option(
      &index.interner,
      index.versions.download_urls[latest_ver_idx],
    )?;
    let version_number = resolve_option(
      &index.interner,
      index.versions.version_numbers[latest_ver_idx],
    )?;
    let package_name = index.resolve_full_name_at(idx)?;
    let zip_name = format!("{}-{}.zip", package_name, version_number);

    Some((zip_name, url))
  }
}

impl From<Vec<Package>> for PackageIndex {
  fn from(packages: Vec<Package>) -> Self {
    let num_packages = packages.len();
    let total_versions: usize = packages.iter().map(|p| p.versions.len()).sum();

    let mut interner = StringInterner::default();

    let mut names = Vec::with_capacity(num_packages);
    let mut full_names = Vec::with_capacity(num_packages);
    let mut owners = Vec::with_capacity(num_packages);
    let mut package_urls = Vec::with_capacity(num_packages);
    let mut dates_created = Vec::with_capacity(num_packages);
    let mut dates_updated = Vec::with_capacity(num_packages);
    let mut uuid4s = Vec::with_capacity(num_packages);
    let mut rating_scores = Vec::with_capacity(num_packages);
    let mut is_pinned = Vec::with_capacity(num_packages);
    let mut is_deprecated = Vec::with_capacity(num_packages);
    let mut has_nsfw_content = Vec::with_capacity(num_packages);
    let mut categories = Vec::with_capacity(num_packages);
    let mut version_ranges = Vec::with_capacity(num_packages);

    let mut package_indices = Vec::with_capacity(total_versions);
    let mut version_names = Vec::with_capacity(total_versions);
    let mut version_full_names = Vec::with_capacity(total_versions);
    let mut version_numbers = Vec::with_capacity(total_versions);
    let mut download_urls = Vec::with_capacity(total_versions);
    let mut dependencies = Vec::with_capacity(total_versions);
    let mut version_dates_created = Vec::with_capacity(total_versions);
    let mut descriptions = Vec::with_capacity(total_versions);
    let mut icons = Vec::with_capacity(total_versions);
    let mut downloads = Vec::with_capacity(total_versions);
    let mut website_urls = Vec::with_capacity(total_versions);
    let mut is_active = Vec::with_capacity(total_versions);
    let mut version_uuid4s = Vec::with_capacity(total_versions);
    let mut file_sizes = Vec::with_capacity(total_versions);

    let mut version_offset = 0;

    for (pkg_idx, package) in packages.into_iter().enumerate() {
      names.push(intern_option(&mut interner, package.name.as_deref()));
      full_names.push(intern_option(&mut interner, package.full_name.as_deref()));
      owners.push(intern_option(&mut interner, package.owner.as_deref()));
      package_urls.push(intern_option(&mut interner, package.package_url.as_deref()));
      dates_created.push(package.date_created);
      dates_updated.push(package.date_updated);
      uuid4s.push(intern_option(&mut interner, package.uuid4.as_deref()));
      rating_scores.push(package.rating_score);
      is_pinned.push(package.is_pinned);
      is_deprecated.push(package.is_deprecated);
      has_nsfw_content.push(package.has_nsfw_content);
      categories.push(intern_vec(&mut interner, &package.categories));

      let version_count = package.versions.len();
      version_ranges.push((version_offset, version_offset + version_count));

      for version in package.versions {
        package_indices.push(pkg_idx);
        version_names.push(intern_option(&mut interner, version.name.as_deref()));
        version_full_names.push(intern_option(&mut interner, version.full_name.as_deref()));
        version_numbers.push(intern_option(
          &mut interner,
          version.version_number.as_deref(),
        ));
        download_urls.push(intern_option(
          &mut interner,
          version.download_url.as_deref(),
        ));
        dependencies.push(intern_vec(&mut interner, &version.dependencies));
        version_dates_created.push(version.date_created);
        descriptions.push(intern_option(&mut interner, version.description.as_deref()));
        icons.push(intern_option(&mut interner, version.icon.as_deref()));
        downloads.push(version.downloads);
        website_urls.push(intern_option(&mut interner, version.website_url.as_deref()));
        is_active.push(version.is_active);
        version_uuid4s.push(intern_option(&mut interner, version.uuid4.as_deref()));
        file_sizes.push(version.file_size);
      }

      version_offset += version_count;
    }

    PackageIndex {
      interner,
      names,
      full_names,
      owners,
      package_urls,
      dates_created,
      dates_updated,
      uuid4s,
      rating_scores,
      is_pinned,
      is_deprecated,
      has_nsfw_content,
      categories,
      version_ranges,
      versions: VersionTable {
        package_indices,
        names: version_names,
        full_names: version_full_names,
        version_numbers,
        download_urls,
        dependencies,
        dates_created: version_dates_created,
        descriptions,
        icons,
        downloads,
        website_urls,
        is_active,
        uuid4s: version_uuid4s,
        file_sizes,
      },
    }
  }
}

/// Narrows an [`InternKey`] to the `u32` index the cache wire format stores.
///
/// `_interner` is unused and kept only so this mirrors the shape of its
/// resolving counterpart.
///
/// # Panics
///
/// Panics if the index exceeds `u32::MAX`, which needs roughly four billion
/// distinct interned strings. A silent truncation here would corrupt the string
/// table in release builds, so this fails loudly instead.
fn key_to_index(_interner: &StringInterner, key: InternKey) -> u32 {
  let index = key.to_index();

  // The wire format stores keys as `u32`. A checked cast turns the (practically
  // unreachable, ~4 billion strings) overflow into a loud panic rather than a
  // silent truncation that would corrupt the string table in release builds.
  u32::try_from(index)
    .unwrap_or_else(|_| panic!("intern key index {index} exceeds u32 range and would truncate"))
}

/// Converts a `u32` string-table index from a deserialized cache back into an
/// [`InternKey`], validating it against the rebuilt interner.
///
/// Returns an [`Error`] (rather than panicking) when the index is out of range
/// for the string table or otherwise not representable as a key, so a corrupt or
/// truncated cache file surfaces as a recoverable error.
fn decode_key(index: u32, table_len: usize) -> Result<InternKey, Error> {
  if index as usize >= table_len {
    return Err(Error::Cache(format!(
      "interned string index {} out of range (string table length {})",
      index, table_len
    )));
  }

  InternKey::from_index(index as usize)
    .ok_or_else(|| Error::Cache(format!("invalid interned key index {}", index)))
}

/// Encodes runtime timestamps to the cache's `i128` Unix-nanosecond wire form.
///
/// `time::OffsetDateTime` is not a native `bincode` type, so the cache stores each
/// timestamp as its absolute instant in nanoseconds since the Unix epoch. The
/// offset is not preserved, which is lossless for equality/ordering (both compare
/// the instant) and matches the UTC timestamps the Thunderstore API returns.
fn encode_dates(dates: &[OffsetDateTime]) -> Vec<i128> {
  dates.iter().map(|d| d.unix_timestamp_nanos()).collect()
}

/// Decodes the cache's `i128` Unix-nanosecond timestamps back to `OffsetDateTime`
/// (as UTC), returning an [`Error`] rather than panicking if a crafted cache holds
/// an out-of-range value.
fn decode_dates(nanos: &[i128]) -> Result<Vec<OffsetDateTime>, Error> {
  nanos
    .iter()
    .map(|&n| {
      OffsetDateTime::from_unix_timestamp_nanos(n)
        .map_err(|e| Error::Cache(format!("invalid cached timestamp {}: {}", n, e)))
    })
    .collect()
}

impl From<&PackageIndex> for SerializedPackageIndex {
  fn from(manifest: &PackageIndex) -> Self {
    let interner = &manifest.interner;
    let string_table: Vec<String> = interner.strings().map(|s| s.to_string()).collect();

    // Symmetric with the `opt`/`nested` decode closures in `TryFrom`: fold the
    // otherwise ~14 identical key-column mappings to `u32` indices into one place.
    let opt = |keys: &[Option<InternKey>]| -> Vec<Option<u32>> {
      keys
        .iter()
        .map(|k| k.map(|key| key_to_index(interner, key)))
        .collect()
    };

    let nested = |lists: &[Vec<InternKey>]| -> Vec<Vec<u32>> {
      lists
        .iter()
        .map(|keys| keys.iter().map(|k| key_to_index(interner, *k)).collect())
        .collect()
    };

    SerializedPackageIndex {
      format_version: CACHE_FORMAT_VERSION,
      string_table,
      names: opt(&manifest.names),
      full_names: opt(&manifest.full_names),
      owners: opt(&manifest.owners),
      package_urls: opt(&manifest.package_urls),
      dates_created: encode_dates(&manifest.dates_created),
      dates_updated: encode_dates(&manifest.dates_updated),
      uuid4s: opt(&manifest.uuid4s),
      rating_scores: manifest.rating_scores.clone(),
      is_pinned: manifest.is_pinned.clone(),
      is_deprecated: manifest.is_deprecated.clone(),
      has_nsfw_content: manifest.has_nsfw_content.clone(),
      categories: nested(&manifest.categories),
      version_ranges: manifest.version_ranges.clone(),
      versions: SerializedVersionTable {
        package_indices: manifest.versions.package_indices.clone(),
        names: opt(&manifest.versions.names),
        full_names: opt(&manifest.versions.full_names),
        version_numbers: opt(&manifest.versions.version_numbers),
        download_urls: opt(&manifest.versions.download_urls),
        dependencies: nested(&manifest.versions.dependencies),
        dates_created: encode_dates(&manifest.versions.dates_created),
        descriptions: opt(&manifest.versions.descriptions),
        icons: opt(&manifest.versions.icons),
        downloads: manifest.versions.downloads.clone(),
        website_urls: opt(&manifest.versions.website_urls),
        is_active: manifest.versions.is_active.clone(),
        uuid4s: opt(&manifest.versions.uuid4s),
        file_sizes: manifest.versions.file_sizes.clone(),
      },
    }
  }
}

impl TryFrom<SerializedPackageIndex> for PackageIndex {
  type Error = Error;

  /// Rebuilds the runtime index from its serialized form, validating every
  /// string-table index and the structural invariants (via [`PackageIndex::validate`])
  /// so a corrupt or malicious cache yields an [`Error`] instead of a panic.
  fn try_from(manifest: SerializedPackageIndex) -> Result<Self, Self::Error> {
    if manifest.format_version != CACHE_FORMAT_VERSION {
      return Err(Error::UnsupportedCacheVersion {
        found: manifest.format_version,
        expected: CACHE_FORMAT_VERSION,
      });
    }

    let mut interner = StringInterner::default();

    for s in &manifest.string_table {
      interner.get_or_intern(s);
    }

    let table_len = manifest.string_table.len();

    // The decode below relies on each string-table index mapping 1:1 to an
    // interner key (index `i` -> key `i`). `get_or_intern` deduplicates, so a
    // table with duplicate (or otherwise non-unique) entries yields a smaller
    // interner; an index that passes `decode_key`'s range check would then
    // dangle past the interner and panic later in `resolve`. Reject such a
    // table here so a corrupt or malicious cache surfaces as a recoverable
    // error rather than a panic.
    if interner.len() != table_len {
      return Err(Error::Cache(
        "cache string table contains duplicate or unresolvable entries".to_string(),
      ));
    }

    let opt = |v: &[Option<u32>]| -> Result<Vec<Option<InternKey>>, Error> {
      v.iter()
        .map(|idx| idx.map(|i| decode_key(i, table_len)).transpose())
        .collect()
    };

    let nested = |v: &[Vec<u32>]| -> Result<Vec<Vec<InternKey>>, Error> {
      v.iter()
        .map(|indices| indices.iter().map(|&i| decode_key(i, table_len)).collect())
        .collect()
    };

    let names = opt(&manifest.names)?;
    let full_names = opt(&manifest.full_names)?;
    let owners = opt(&manifest.owners)?;
    let package_urls = opt(&manifest.package_urls)?;
    let uuid4s = opt(&manifest.uuid4s)?;
    let categories = nested(&manifest.categories)?;

    let version_names = opt(&manifest.versions.names)?;
    let version_full_names = opt(&manifest.versions.full_names)?;
    let version_numbers = opt(&manifest.versions.version_numbers)?;
    let download_urls = opt(&manifest.versions.download_urls)?;
    let dependencies = nested(&manifest.versions.dependencies)?;
    let descriptions = opt(&manifest.versions.descriptions)?;
    let icons = opt(&manifest.versions.icons)?;
    let website_urls = opt(&manifest.versions.website_urls)?;
    let version_uuid4s = opt(&manifest.versions.uuid4s)?;

    let dates_created = decode_dates(&manifest.dates_created)?;
    let dates_updated = decode_dates(&manifest.dates_updated)?;
    let version_dates_created = decode_dates(&manifest.versions.dates_created)?;

    let result = PackageIndex {
      interner,
      names,
      full_names,
      owners,
      package_urls,
      dates_created,
      dates_updated,
      uuid4s,
      rating_scores: manifest.rating_scores,
      is_pinned: manifest.is_pinned,
      is_deprecated: manifest.is_deprecated,
      has_nsfw_content: manifest.has_nsfw_content,
      categories,
      version_ranges: manifest.version_ranges,
      versions: VersionTable {
        package_indices: manifest.versions.package_indices,
        names: version_names,
        full_names: version_full_names,
        version_numbers,
        download_urls,
        dependencies,
        dates_created: version_dates_created,
        descriptions,
        icons,
        downloads: manifest.versions.downloads,
        website_urls,
        is_active: manifest.versions.is_active,
        uuid4s: version_uuid4s,
        file_sizes: manifest.versions.file_sizes,
      },
    };

    result.validate()?;

    Ok(result)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use time::OffsetDateTime;

  fn create_test_package(name: &str, owner: &str, version: &str) -> Package {
    let full_name = format!("{}-{}", owner, name);
    Package {
      name: Some(name.to_string()),
      full_name: Some(full_name.clone()),
      owner: Some(owner.to_string()),
      package_url: Some(format!("https://example.com/{}", name)),
      date_created: OffsetDateTime::now_utc(),
      date_updated: OffsetDateTime::now_utc(),
      uuid4: Some("test-uuid".to_string()),
      rating_score: Some(5),
      is_pinned: Some(false),
      is_deprecated: Some(false),
      has_nsfw_content: Some(false),
      categories: vec!["category1".to_string()],
      versions: vec![Version {
        name: Some(name.to_string()),
        full_name: Some(full_name.clone()),
        description: Some("Test description".to_string()),
        icon: Some("icon.png".to_string()),
        version_number: Some(version.to_string()),
        dependencies: vec![],
        download_url: Some(format!("https://example.com/{}/download", name)),
        downloads: Some(100),
        date_created: OffsetDateTime::now_utc(),
        website_url: Some("https://example.com".to_string()),
        is_active: Some(true),
        uuid4: Some("test-version-uuid".to_string()),
        file_size: Some(1024),
      }],
    }
  }

  fn create_test_package_with_dependencies(
    name: &str,
    owner: &str,
    version: &str,
    dependencies: Vec<String>,
  ) -> Package {
    let mut pkg = create_test_package(name, owner, version);
    if let Some(latest_version) = pkg.versions.first_mut() {
      latest_version.dependencies = dependencies;
    }
    pkg
  }

  #[test]
  fn test_latest_version() {
    let mut pkg = create_test_package("TestMod", "TestOwner", "1.0.0");

    let older_version = Version {
      name: Some("TestMod".to_string()),
      full_name: Some("TestOwner-TestMod".to_string()),
      description: Some("Older version".to_string()),
      icon: Some("icon.png".to_string()),
      version_number: Some("0.9.0".to_string()),
      dependencies: vec![],
      download_url: Some("https://example.com/TestMod/download-old".to_string()),
      downloads: Some(50),
      date_created: OffsetDateTime::now_utc().saturating_sub(time::Duration::days(30)),
      website_url: Some("https://example.com".to_string()),
      is_active: Some(true),
      uuid4: Some("old-version-uuid".to_string()),
      file_size: Some(512),
    };
    pkg.versions.push(older_version);

    let newer_version = Version {
      name: Some("TestMod".to_string()),
      full_name: Some("TestOwner-TestMod".to_string()),
      description: Some("Newer version".to_string()),
      icon: Some("icon.png".to_string()),
      version_number: Some("1.1.0".to_string()),
      dependencies: vec![],
      download_url: Some("https://example.com/TestMod/download-new".to_string()),
      downloads: Some(150),
      date_created: OffsetDateTime::now_utc().saturating_add(time::Duration::days(30)),
      website_url: Some("https://example.com".to_string()),
      is_active: Some(true),
      uuid4: Some("new-version-uuid".to_string()),
      file_size: Some(2048),
    };
    pkg.versions.push(newer_version);

    let latest = pkg.latest_version().unwrap();
    assert_eq!(latest.version_number, Some("1.1.0".to_string()));
    assert_eq!(latest.description, Some("Newer version".to_string()));
  }

  #[test]
  fn test_zip_and_url() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let (filename, url) = pkg.zip_and_url().unwrap();

    assert_eq!(filename, "TestOwner-TestMod-1.0.0.zip");
    assert_eq!(url, "https://example.com/TestMod/download");
  }

  #[test]
  fn test_zip_and_url_missing_data() {
    let mut pkg = create_test_package("TestMod", "TestOwner", "1.0.0");

    if let Some(version) = pkg.versions.first_mut() {
      version.version_number = None;
    }

    assert!(pkg.zip_and_url().is_none());

    let mut pkg = create_test_package("TestMod", "TestOwner", "1.0.0");

    if let Some(version) = pkg.versions.first_mut() {
      version.download_url = None;
    }

    assert!(pkg.zip_and_url().is_none());

    let mut pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    pkg.full_name = None;

    assert!(pkg.zip_and_url().is_none());
  }

  #[test]
  fn test_package_debug() {
    let pkg = create_test_package("ModTest", "OwnerTest", "1.2.3");
    let debug_output = format!("{:?}", pkg);

    assert!(debug_output.contains("Package"));
    assert!(debug_output.contains("full_name: Some(\"OwnerTest-ModTest\")"));
    assert!(debug_output.contains("dependencies: Some([])"));
    assert!(
      debug_output.contains("download_url: Some(Some(\"https://example.com/ModTest/download\"))")
    );

    let mut pkg_with_deps = create_test_package_with_dependencies(
      "ModWithDeps",
      "OwnerTest",
      "2.0.0",
      vec!["Dep1-Mod1".to_string(), "Dep2-Mod2".to_string()],
    );
    let debug_with_deps = format!("{:?}", pkg_with_deps);

    assert!(debug_with_deps.contains("dependencies: Some([\"Dep1-Mod1\", \"Dep2-Mod2\"])"));

    pkg_with_deps.full_name = None;
    let debug_missing_name = format!("{:?}", pkg_with_deps);
    assert!(debug_missing_name.contains("full_name: None"));

    let mut pkg_no_version = create_test_package("NoVersion", "TestOwner", "1.0.0");
    pkg_no_version.versions.clear();
    let debug_no_version = format!("{:?}", pkg_no_version);

    assert!(debug_no_version.contains("dependencies: None"));
    assert!(debug_no_version.contains("download_url: None"));
  }

  #[test]
  fn test_package_index_basic_operations() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let interned: PackageIndex = packages.into();

    assert_eq!(interned.len(), 1);

    assert!(!interned.is_empty());

    assert_eq!(interned.resolve_name_at(0), Some("TestMod".to_string()));

    assert_eq!(
      interned.resolve_full_name_at(0),
      Some("TestOwner-TestMod".to_string())
    );

    assert_eq!(interned.resolve_owner_at(0), Some("TestOwner".to_string()));
  }

  #[test]
  fn search_matches_name_and_full_name_case_insensitively() {
    let packages = vec![
      create_test_package("CoolMod", "Author", "1.0.0"),
      create_test_package("OtherThing", "Someone", "1.0.0"),
    ];
    let index: PackageIndex = packages.into();

    // Matches on the short name, ignoring case.
    assert_eq!(index.search("coolmod"), vec![0]);
    assert_eq!(index.search("COOLMOD"), vec![0]);
    // Matches on the owner-qualified full name too.
    assert_eq!(index.search("author-cool"), vec![0]);
    // A substring anywhere counts.
    assert_eq!(index.search("oolMo"), vec![0]);
    // No match is an empty result, not an error.
    assert!(index.search("nothing-like-this").is_empty());
    // An empty term matches everything, so clearing a search box shows the full
    // list rather than nothing.
    assert_eq!(index.search(""), vec![0, 1]);
  }

  #[test]
  fn test_package_index_borrowing_accessors() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let interned: PackageIndex = packages.into();

    // The borrowing accessors return the same values as their allocating
    // counterparts, and yield None for an out-of-range index.
    assert_eq!(interned.name_at(0), Some("TestMod"));

    assert_eq!(interned.full_name_at(0), Some("TestOwner-TestMod"));

    assert_eq!(interned.owner_at(0), Some("TestOwner"));

    assert_eq!(interned.name_at(1), None);

    assert_eq!(interned.full_name_at(1), None);

    assert_eq!(interned.owner_at(1), None);
  }

  #[test]
  fn test_package_index_get_package_by_full_name() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let interned: PackageIndex = packages.into();

    let found = interned.get_package_by_full_name("TestOwner-TestMod");

    assert!(found.is_some());

    let package = found.unwrap();

    assert_eq!(package.name, Some("TestMod".to_string()));

    assert_eq!(package.owner, Some("TestOwner".to_string()));

    assert_eq!(package.versions.len(), 1);
  }

  #[test]
  fn test_package_index_find_index_by_full_name() {
    let pkg1 = create_test_package("ModA", "Owner1", "1.0.0");
    let pkg2 = create_test_package("ModB", "Owner2", "2.0.0");
    let packages = vec![pkg1, pkg2];
    let interned: PackageIndex = packages.into();

    assert_eq!(interned.find_index_by_full_name("Owner1-ModA"), Some(0));

    assert_eq!(interned.find_index_by_full_name("Owner2-ModB"), Some(1));

    assert_eq!(interned.find_index_by_full_name("NonExistent"), None);
  }

  #[test]
  fn test_package_index_get_package_at() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let interned: PackageIndex = packages.into();

    let package = interned.get_package_at(0).unwrap();

    assert_eq!(package.name, Some("TestMod".to_string()));

    assert_eq!(package.full_name, Some("TestOwner-TestMod".to_string()));

    assert_eq!(package.owner, Some("TestOwner".to_string()));

    assert_eq!(package.versions.len(), 1);

    assert_eq!(
      package.versions[0].version_number,
      Some("1.0.0".to_string())
    );
  }

  #[test]
  fn test_package_index_get_latest_version_at() {
    let mut pkg = create_test_package("TestMod", "TestOwner", "1.0.0");

    let older_version = Version {
      name: Some("TestMod".to_string()),
      full_name: Some("TestOwner-TestMod".to_string()),
      description: Some("Older version".to_string()),
      icon: Some("icon.png".to_string()),
      version_number: Some("0.9.0".to_string()),
      dependencies: vec![],
      download_url: Some("https://example.com/TestMod/download-old".to_string()),
      downloads: Some(50),
      date_created: OffsetDateTime::now_utc().saturating_sub(time::Duration::days(30)),
      website_url: Some("https://example.com".to_string()),
      is_active: Some(true),
      uuid4: Some("old-version-uuid".to_string()),
      file_size: Some(512),
    };
    pkg.versions.insert(0, older_version);

    let packages = vec![pkg];
    let interned: PackageIndex = packages.into();

    let latest_idx = interned.get_latest_version_at(0);

    assert!(latest_idx.is_some());

    let version_idx = latest_idx.unwrap();
    let version_number = interned.versions.version_numbers[version_idx]
      .map(|key| interned.interner.resolve(&key).to_string());

    assert_eq!(version_number, Some("1.0.0".to_string()));
  }

  #[test]
  fn test_package_index_get_latest_version_at_empty_range() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let mut interned: PackageIndex = packages.into();

    interned.version_ranges[0] = (0, 0);

    let latest = interned.get_latest_version_at(0);

    assert!(latest.is_none());
  }

  #[test]
  fn test_package_index_build_name_index() {
    let pkg1 = create_test_package("ModA", "Owner1", "1.0.0");
    let pkg2 = create_test_package("ModB", "Owner2", "2.0.0");
    let packages = vec![pkg1, pkg2];
    let interned: PackageIndex = packages.into();

    let index = interned.build_name_index();

    assert_eq!(index.len(), 2);

    assert_eq!(index.get("Owner1-ModA"), Some(&0));

    assert_eq!(index.get("Owner2-ModB"), Some(&1));
  }

  #[test]
  fn test_package_index_validate_valid() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let interned: PackageIndex = packages.into();

    let result = interned.validate();

    assert!(result.is_ok());
  }

  #[test]
  fn validate_split_methods_are_individually_callable() {
    let interned: PackageIndex = vec![create_test_package("ModA", "Owner1", "1.0.0")].into();

    // All four checks pass on a well-formed index.
    assert!(interned.validate_lengths().is_ok());
    assert!(interned.validate_version_ranges().is_ok());
    assert!(interned.validate_version_ownership().is_ok());
    assert!(interned.validate_unique_full_names().is_ok());

    // A parallel-vector length mismatch is caught specifically by validate_lengths.
    let mut broken: PackageIndex = vec![create_test_package("ModA", "Owner1", "1.0.0")].into();
    broken.owners.pop();
    assert!(broken.validate_lengths().is_err());

    // A repeated full name is caught specifically by validate_unique_full_names.
    let dup: PackageIndex = vec![
      create_test_package("ModA", "Owner1", "1.0.0"),
      create_test_package("ModA", "Owner1", "2.0.0"),
    ]
    .into();
    assert!(dup.validate_unique_full_names().is_err());
  }

  #[test]
  fn encode_decode_dates_roundtrips_and_rejects_out_of_range() {
    let dates = vec![
      OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
      OffsetDateTime::from_unix_timestamp(0).unwrap(),
    ];

    let encoded = encode_dates(&dates);
    let decoded = decode_dates(&encoded).unwrap();

    assert_eq!(decoded, dates);

    // An i128 nanosecond value beyond the representable range is a recoverable
    // error, not a panic.
    let err = decode_dates(&[i128::MAX]).unwrap_err();

    assert!(matches!(err, Error::Cache(_)));
  }

  #[test]
  fn from_packages_populates_core_fields() {
    let interned: PackageIndex = vec![create_test_package("ModA", "Owner1", "1.0.0")].into();

    assert_eq!(interned.len(), 1);
    assert_eq!(
      interned.resolve_full_name_at(0),
      Some("Owner1-ModA".to_string())
    );

    // A single package with one version yields one range covering that version,
    // and the version records package index 0 as its owner.
    assert_eq!(interned.version_ranges, vec![(0, 1)]);
    assert_eq!(interned.versions.package_indices, vec![0]);
  }

  #[test]
  fn test_package_index_validate_mismatched_full_names_length() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let mut interned: PackageIndex = packages.into();

    interned.full_names.pop();

    let result = interned.validate();

    assert!(result.is_err());

    assert!(
      result
        .unwrap_err()
        .to_string()
        .contains("full_names length")
    );
  }

  #[test]
  fn test_package_index_validate_mismatched_owners_length() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let mut interned: PackageIndex = packages.into();

    interned.owners.pop();

    let result = interned.validate();

    assert!(result.is_err());

    assert!(result.unwrap_err().to_string().contains("owners length"));
  }

  #[test]
  fn test_package_index_validate_mismatched_package_urls_length() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let mut interned: PackageIndex = packages.into();

    interned.package_urls.pop();

    let result = interned.validate();

    assert!(result.is_err());

    assert!(
      result
        .unwrap_err()
        .to_string()
        .contains("package_urls length")
    );
  }

  #[test]
  fn test_package_index_validate_mismatched_dates_created_length() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let mut interned: PackageIndex = packages.into();

    interned.dates_created.pop();

    let result = interned.validate();

    assert!(result.is_err());

    assert!(
      result
        .unwrap_err()
        .to_string()
        .contains("dates_created length")
    );
  }

  #[test]
  fn test_package_index_validate_mismatched_dates_updated_length() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let mut interned: PackageIndex = packages.into();

    interned.dates_updated.pop();

    let result = interned.validate();

    assert!(result.is_err());

    assert!(
      result
        .unwrap_err()
        .to_string()
        .contains("dates_updated length")
    );
  }

  #[test]
  fn test_package_index_validate_mismatched_uuid4s_length() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let mut interned: PackageIndex = packages.into();

    interned.uuid4s.pop();

    let result = interned.validate();

    assert!(result.is_err());

    assert!(result.unwrap_err().to_string().contains("uuid4s length"));
  }

  #[test]
  fn test_package_index_validate_mismatched_rating_scores_length() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let mut interned: PackageIndex = packages.into();

    interned.rating_scores.pop();

    let result = interned.validate();

    assert!(result.is_err());

    assert!(
      result
        .unwrap_err()
        .to_string()
        .contains("rating_scores length")
    );
  }

  #[test]
  fn test_package_index_validate_mismatched_is_pinned_length() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let mut interned: PackageIndex = packages.into();

    interned.is_pinned.pop();

    let result = interned.validate();

    assert!(result.is_err());

    assert!(result.unwrap_err().to_string().contains("is_pinned length"));
  }

  #[test]
  fn test_package_index_validate_mismatched_is_deprecated_length() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let mut interned: PackageIndex = packages.into();

    interned.is_deprecated.pop();

    let result = interned.validate();

    assert!(result.is_err());

    assert!(
      result
        .unwrap_err()
        .to_string()
        .contains("is_deprecated length")
    );
  }

  #[test]
  fn test_package_index_validate_mismatched_has_nsfw_content_length() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let mut interned: PackageIndex = packages.into();

    interned.has_nsfw_content.pop();

    let result = interned.validate();

    assert!(result.is_err());

    assert!(
      result
        .unwrap_err()
        .to_string()
        .contains("has_nsfw_content length")
    );
  }

  #[test]
  fn test_package_index_validate_mismatched_categories_length() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let mut interned: PackageIndex = packages.into();

    interned.categories.pop();

    let result = interned.validate();

    assert!(result.is_err());

    assert!(
      result
        .unwrap_err()
        .to_string()
        .contains("categories length")
    );
  }

  #[test]
  fn test_package_index_validate_mismatched_version_ranges_length() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let mut interned: PackageIndex = packages.into();

    interned.version_ranges.pop();

    let result = interned.validate();

    assert!(result.is_err());

    assert!(
      result
        .unwrap_err()
        .to_string()
        .contains("version_ranges length")
    );
  }

  #[test]
  fn test_package_index_validate_mismatched_version_numbers_length() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let mut interned: PackageIndex = packages.into();

    interned.versions.version_numbers.pop();

    let result = interned.validate();

    assert!(result.is_err());

    assert!(
      result
        .unwrap_err()
        .to_string()
        .contains("version_numbers length")
    );
  }

  #[test]
  fn test_package_index_validate_mismatched_version_download_urls_length() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let mut interned: PackageIndex = packages.into();

    interned.versions.download_urls.pop();

    let result = interned.validate();

    assert!(result.is_err());

    assert!(
      result
        .unwrap_err()
        .to_string()
        .contains("download_urls length")
    );
  }

  #[test]
  fn test_package_index_validate_mismatched_version_dependencies_length() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let mut interned: PackageIndex = packages.into();

    interned.versions.dependencies.pop();

    let result = interned.validate();

    assert!(result.is_err());

    assert!(
      result
        .unwrap_err()
        .to_string()
        .contains("dependencies length")
    );
  }

  #[test]
  fn test_package_index_validate_mismatched_version_dates_created_length() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let mut interned: PackageIndex = packages.into();

    interned.versions.dates_created.pop();

    let result = interned.validate();

    assert!(result.is_err());

    assert!(
      result
        .unwrap_err()
        .to_string()
        .contains("dates_created length")
    );
  }

  #[test]
  fn test_package_index_validate_mismatched_version_descriptions_length() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let mut interned: PackageIndex = packages.into();

    interned.versions.descriptions.pop();

    let result = interned.validate();

    assert!(result.is_err());

    assert!(
      result
        .unwrap_err()
        .to_string()
        .contains("descriptions length")
    );
  }

  #[test]
  fn test_package_index_validate_mismatched_version_icons_length() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let mut interned: PackageIndex = packages.into();

    interned.versions.icons.pop();

    let result = interned.validate();

    assert!(result.is_err());

    assert!(result.unwrap_err().to_string().contains("icons length"));
  }

  #[test]
  fn test_package_index_validate_mismatched_version_downloads_length() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let mut interned: PackageIndex = packages.into();

    interned.versions.downloads.pop();

    let result = interned.validate();

    assert!(result.is_err());

    assert!(result.unwrap_err().to_string().contains("downloads length"));
  }

  #[test]
  fn test_package_index_validate_mismatched_version_website_urls_length() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let mut interned: PackageIndex = packages.into();

    interned.versions.website_urls.pop();

    let result = interned.validate();

    assert!(result.is_err());

    assert!(
      result
        .unwrap_err()
        .to_string()
        .contains("website_urls length")
    );
  }

  #[test]
  fn test_package_index_validate_mismatched_version_is_active_length() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let mut interned: PackageIndex = packages.into();

    interned.versions.is_active.pop();

    let result = interned.validate();

    assert!(result.is_err());

    assert!(result.unwrap_err().to_string().contains("is_active length"));
  }

  #[test]
  fn test_package_index_validate_mismatched_version_uuid4s_length() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let mut interned: PackageIndex = packages.into();

    interned.versions.uuid4s.pop();

    let result = interned.validate();

    assert!(result.is_err());

    assert!(result.unwrap_err().to_string().contains("uuid4s length"));
  }

  #[test]
  fn test_package_index_validate_mismatched_version_file_sizes_length() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let mut interned: PackageIndex = packages.into();

    interned.versions.file_sizes.pop();

    let result = interned.validate();

    assert!(result.is_err());

    assert!(
      result
        .unwrap_err()
        .to_string()
        .contains("file_sizes length")
    );
  }

  #[test]
  fn test_package_index_validate_invalid_version_range() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let mut interned: PackageIndex = packages.into();

    interned.version_ranges[0] = (5, 3);

    let result = interned.validate();

    assert!(result.is_err());

    assert!(
      result
        .unwrap_err()
        .to_string()
        .contains("Invalid version range")
    );
  }

  #[test]
  fn test_package_index_validate_version_range_out_of_bounds() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg];
    let mut interned: PackageIndex = packages.into();

    interned.version_ranges[0] = (0, 999);

    let result = interned.validate();

    assert!(result.is_err());

    assert!(result.unwrap_err().to_string().contains("ends at"));
  }

  #[test]
  fn test_package_index_serialization_round_trip() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let packages = vec![pkg.clone()];
    let interned: PackageIndex = packages.into();

    let serializable: SerializedPackageIndex = (&interned).into();

    let binary = bincode::encode_to_vec(&serializable, bincode::config::standard()).unwrap();

    let (deserialized, _): (SerializedPackageIndex, usize) =
      bincode::decode_from_slice(&binary, bincode::config::standard()).unwrap();

    let recovered: PackageIndex = deserialized.try_into().unwrap();

    assert_eq!(recovered.len(), 1);

    assert_eq!(recovered.resolve_name_at(0), Some("TestMod".to_string()));

    assert_eq!(
      recovered.resolve_full_name_at(0),
      Some("TestOwner-TestMod".to_string())
    );

    let recovered_pkg = recovered.get_package_at(0).unwrap();

    assert_eq!(recovered_pkg.name, pkg.name);

    assert_eq!(recovered_pkg.full_name, pkg.full_name);

    assert_eq!(recovered_pkg.versions.len(), pkg.versions.len());
  }

  #[test]
  fn test_version_name_and_full_name_are_version_level_not_package_level() {
    // Real Thunderstore data: a version's full_name carries the version suffix,
    // distinct from the package's own full_name.
    let mut pkg = create_test_package("CoolMod", "Author", "1.0.0");
    pkg.name = Some("CoolMod".to_string());
    pkg.full_name = Some("Author-CoolMod".to_string());
    pkg.versions[0].name = Some("Author-CoolMod".to_string());
    pkg.versions[0].full_name = Some("Author-CoolMod-1.0.0".to_string());

    let interned: PackageIndex = vec![pkg].into();

    // Materialized directly, the version keeps its own name/full_name rather than
    // inheriting the package's.
    let materialized = interned.get_package_at(0).unwrap();

    assert_eq!(materialized.full_name, Some("Author-CoolMod".to_string()));

    assert_eq!(
      materialized.versions[0].full_name,
      Some("Author-CoolMod-1.0.0".to_string())
    );

    assert_eq!(
      materialized.versions[0].name,
      Some("Author-CoolMod".to_string())
    );

    // The distinction also survives a serialize -> deserialize round trip.
    let serializable: SerializedPackageIndex = (&interned).into();

    let binary = bincode::encode_to_vec(&serializable, bincode::config::standard()).unwrap();

    let (deserialized, _): (SerializedPackageIndex, usize) =
      bincode::decode_from_slice(&binary, bincode::config::standard()).unwrap();

    let recovered: PackageIndex = deserialized.try_into().unwrap();
    let recovered_pkg = recovered.get_package_at(0).unwrap();

    assert_eq!(recovered_pkg.full_name, Some("Author-CoolMod".to_string()));

    assert_eq!(
      recovered_pkg.versions[0].full_name,
      Some("Author-CoolMod-1.0.0".to_string())
    );
  }

  #[test]
  fn test_resolve_report_lists_unresolved_packages() {
    let interned: PackageIndex = vec![create_test_package("ModA", "Owner1", "1.0.0")].into();

    let graph = DependencyGraph::new(vec!["Owner1-ModA".to_string(), "Ghost-Missing".to_string()]);

    let report = graph.resolve_report(&interned);

    // The present package resolves; the missing one is reported rather than
    // silently dropped.
    assert_eq!(report.urls.len(), 1);
    assert_eq!(report.unresolved, vec!["Ghost-Missing".to_string()]);

    // The convenience `resolve` still returns just the URL map.
    assert_eq!(graph.resolve(&interned).len(), 1);
  }

  #[test]
  fn test_resolve_report_flags_package_with_no_downloadable_version() {
    let mut pkg = create_test_package("ModA", "Owner1", "1.0.0");

    // Present in the index, but with nothing downloadable.
    pkg.versions.clear();

    let interned: PackageIndex = vec![pkg].into();

    let graph = DependencyGraph::new(vec!["Owner1-ModA".to_string()]);
    let report = graph.resolve_report(&interned);

    // It yields no URL, and rather than being silently dropped it is reported so
    // an empty `unresolved` can be trusted to mean a complete install.
    assert!(report.urls.is_empty());
    assert_eq!(report.unresolved, vec!["Owner1-ModA".to_string()]);
  }

  #[test]
  fn test_duplicate_string_table_returns_error_not_panic() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let interned: PackageIndex = vec![pkg].into();
    let mut serializable: SerializedPackageIndex = (&interned).into();

    // Push a duplicate string into the table and repoint a package name at its
    // (now out-of-real-range) index. The index passes `decode_key`'s range check
    // but dangles past the deduplicated interner; without the duplicate-table
    // guard this would panic later in `resolve` instead of erroring here.
    let dup = serializable.string_table[0].clone();

    serializable.string_table.push(dup);

    let last_index = serializable.string_table.len() as u32 - 1;

    serializable.names[0] = Some(last_index);

    let result = PackageIndex::try_from(serializable);

    assert!(result.is_err());
  }

  #[test]
  fn test_corrupt_cache_index_returns_error_not_panic() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let interned: PackageIndex = vec![pkg].into();
    let mut serializable: SerializedPackageIndex = (&interned).into();

    // Point a package name at a string-table index that does not exist; a
    // corrupt cache must surface as an error rather than panicking.
    let bad_index = serializable.string_table.len() as u32 + 5;
    serializable.names[0] = Some(bad_index);

    let result = PackageIndex::try_from(serializable);

    assert!(result.is_err());
  }

  #[test]
  fn test_package_index_dependency_graph_resolve() {
    let pkg1 = create_test_package("ModA", "Owner1", "1.0.0");
    let pkg2 = create_test_package_with_dependencies(
      "ModB",
      "Owner2",
      "2.0.0",
      vec!["Owner3-ModC".to_string()],
    );
    let pkg3 = create_test_package_with_dependencies(
      "ModC",
      "Owner3",
      "1.5.0",
      vec!["Owner4-ModD".to_string()],
    );
    let pkg4 = create_test_package("ModD", "Owner4", "0.9.0");

    let packages = vec![pkg1, pkg2, pkg3, pkg4];
    let interned: PackageIndex = packages.into();

    let dg1 = DependencyGraph::new(vec!["Owner1-ModA".to_string()]);
    let result1 = dg1.resolve(&interned);

    assert_eq!(result1.len(), 1);

    assert!(result1.contains_key("Owner1-ModA-1.0.0.zip"));

    let dg2 = DependencyGraph::new(vec!["Owner2-ModB".to_string()]);
    let result2 = dg2.resolve(&interned);

    assert_eq!(result2.len(), 3);

    assert!(result2.contains_key("Owner2-ModB-2.0.0.zip"));

    assert!(result2.contains_key("Owner3-ModC-1.5.0.zip"));

    assert!(result2.contains_key("Owner4-ModD-0.9.0.zip"));
  }

  #[test]
  fn test_package_index_dependency_graph_resolve_with_missing_dependency() {
    let pkg1 = create_test_package("ModA", "Owner1", "1.0.0");
    let pkg2 = create_test_package_with_dependencies(
      "ModB",
      "Owner2",
      "2.0.0",
      vec!["Owner3-NonExistent".to_string()],
    );

    let packages = vec![pkg1, pkg2];
    let interned: PackageIndex = packages.into();

    let dg = DependencyGraph::new(vec!["Owner2-ModB".to_string()]);
    let result = dg.resolve(&interned);

    assert_eq!(result.len(), 1);

    assert!(result.contains_key("Owner2-ModB-2.0.0.zip"));
  }

  #[test]
  fn test_package_index_dependency_graph_resolve_with_version_suffix() {
    let pkg1 = create_test_package("ModA", "Owner1", "1.0.0");

    let packages = vec![pkg1];
    let interned: PackageIndex = packages.into();

    let dg = DependencyGraph::new(vec!["Owner1-ModA-1.0.0".to_string()]);
    let result = dg.resolve(&interned);

    assert_eq!(result.len(), 1);

    assert!(result.contains_key("Owner1-ModA-1.0.0.zip"));
  }

  #[test]
  fn test_cache_rejects_unsupported_format_version() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let interned: PackageIndex = vec![pkg].into();
    let mut serializable: SerializedPackageIndex = (&interned).into();

    // A cache written by a future/incompatible format must be rejected as a
    // recoverable error (so the client re-fetches) rather than misdecoded.
    serializable.format_version = CACHE_FORMAT_VERSION + 1;

    let result = PackageIndex::try_from(serializable);

    assert!(matches!(result, Err(Error::UnsupportedCacheVersion { .. })));
  }

  #[test]
  fn test_cache_current_format_version_round_trips() {
    let pkg = create_test_package("TestMod", "TestOwner", "1.0.0");
    let interned: PackageIndex = vec![pkg].into();
    let serializable: SerializedPackageIndex = (&interned).into();

    assert_eq!(serializable.format_version, CACHE_FORMAT_VERSION);
    assert!(PackageIndex::try_from(serializable).is_ok());
  }

  #[test]
  fn test_validate_rejects_version_owned_by_wrong_package() {
    let pkg1 = create_test_package("ModA", "Owner1", "1.0.0");
    let pkg2 = create_test_package("ModB", "Owner2", "2.0.0");
    let mut interned: PackageIndex = vec![pkg1, pkg2].into();

    // Version 0 sits in package 0's range but claims to belong to package 1.
    interned.versions.package_indices[0] = 1;

    let result = interned.validate();

    assert!(result.is_err());

    assert!(result.unwrap_err().to_string().contains("package_index"));
  }

  #[test]
  fn test_validate_rejects_overlapping_version_ranges() {
    let pkg1 = create_test_package("ModA", "Owner1", "1.0.0");
    let pkg2 = create_test_package("ModB", "Owner2", "2.0.0");
    let mut interned: PackageIndex = vec![pkg1, pkg2].into();

    // Point both packages at the same version, leaving version 1 uncovered.
    interned.version_ranges[1] = (0, 1);

    let result = interned.validate();

    assert!(result.is_err());

    assert!(result.unwrap_err().to_string().contains("overlapping"));
  }

  #[test]
  fn test_package_index_validate_rejects_duplicate_full_names() {
    // Two packages sharing a full name: Thunderstore guarantees uniqueness, so a
    // cache with a duplicate is corrupt. Rejecting it keeps
    // `find_index_by_full_name` (first match) and `build_name_index` (last match)
    // from resolving the same name to different packages.
    let pkg1 = create_test_package("TestMod", "TestOwner", "1.0.0");
    let pkg2 = create_test_package("TestMod", "TestOwner", "2.0.0");
    let interned: PackageIndex = vec![pkg1, pkg2].into();

    let result = interned.validate();

    assert!(result.is_err());

    assert!(
      result
        .unwrap_err()
        .to_string()
        .contains("duplicate full name")
    );
  }

  #[test]
  fn test_package_index_dependency_graph_resolve_handles_cycles() {
    // ModA <-> ModB depend on each other, and ModC depends on itself. Without a
    // visited set, resolve() would re-insert these deps forever and never return.
    let pkg_a = create_test_package_with_dependencies(
      "ModA",
      "Owner1",
      "1.0.0",
      vec!["Owner2-ModB".to_string()],
    );
    let pkg_b = create_test_package_with_dependencies(
      "ModB",
      "Owner2",
      "2.0.0",
      vec!["Owner1-ModA".to_string()],
    );
    let pkg_c = create_test_package_with_dependencies(
      "ModC",
      "Owner3",
      "3.0.0",
      vec!["Owner3-ModC".to_string()],
    );

    let packages = vec![pkg_a, pkg_b, pkg_c];
    let interned: PackageIndex = packages.into();

    let dg = DependencyGraph::new(vec!["Owner1-ModA".to_string(), "Owner3-ModC".to_string()]);
    let result = dg.resolve(&interned);

    assert_eq!(result.len(), 3);

    assert!(result.contains_key("Owner1-ModA-1.0.0.zip"));

    assert!(result.contains_key("Owner2-ModB-2.0.0.zip"));

    assert!(result.contains_key("Owner3-ModC-3.0.0.zip"));
  }
}
