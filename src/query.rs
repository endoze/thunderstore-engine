//! Pure read-side queries over the package index and a profile's installed
//! packages. Nothing here does IO.

#![deny(missing_docs)]

pub(crate) mod search;

use crate::intern::InternKey;
use crate::models::PackageIndex;
use crate::profile::modlist::{ProfileMod, SemVer};
use crate::util::full_name_prefix;
use std::collections::{HashMap, HashSet};
use time::OffsetDateTime;

/// An installed package the index has a newer version of.
///
/// `#[non_exhaustive]`: fields may be added, so downstream code cannot
/// construct or exhaustively destructure this directly.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OutdatedPackage {
  /// The package's `Owner-Name` identifier.
  pub full_name: String,
  /// The version recorded as installed.
  pub installed: SemVer,
  /// The index's most recently published version.
  pub latest: SemVer,
}

/// Every installed package whose latest version in `index` is newer than the
/// installed one, in `installed` order.
///
/// "Latest" is the most recently published version, the one
/// [`Package::latest_version`](crate::models::Package::latest_version) and the
/// resolver choose, so an update installs what this reports. "Newer" follows
/// the [`SemVer`] ordering, so a republished older version is never an update.
/// Packages absent from `index` are never outdated.
pub fn outdated(installed: &[ProfileMod], index: &PackageIndex) -> Vec<OutdatedPackage> {
  let by_name: HashMap<&str, usize> = (0..index.len())
    .filter_map(|idx| Some((index.full_name_at(idx)?, idx)))
    .collect();

  installed
    .iter()
    .filter_map(|package| {
      let &idx = by_name.get(package.name.as_str())?;
      let latest = SemVer::parse(index.latest_version_number_at(idx)?);

      (latest > package.version_number).then(|| OutdatedPackage {
        full_name: package.name.clone(),
        installed: package.version_number,
        latest,
      })
    })
    .collect()
}

/// Every installed package that requires `full_name` (`Owner-Name`), directly
/// or through other installed packages, in `installed` order.
///
/// Reads each package's recorded dependencies, so the result describes what is
/// installed rather than what the index currently publishes. The package
/// itself is never listed, even inside a dependency cycle.
pub fn dependants<'a>(installed: &'a [ProfileMod], full_name: &str) -> Vec<&'a ProfileMod> {
  let mut required: HashSet<&str> = HashSet::from([full_name]);
  let mut grew = true;

  while grew {
    grew = false;

    for package in installed {
      if required.contains(package.name.as_str()) {
        continue;
      }

      if package
        .dependencies
        .iter()
        .any(|dependency| required.contains(full_name_prefix(dependency)))
      {
        required.insert(&package.name);
        grew = true;
      }
    }
  }

  installed
    .iter()
    .filter(|package| package.name != full_name && required.contains(package.name.as_str()))
    .collect()
}

/// The column [`browse`] orders packages by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
  /// When the package was last updated.
  #[default]
  LastUpdated,
  /// The short package name, ignoring case.
  Name,
  /// Downloads summed over every version.
  Downloads,
  /// The rating score. Unset reads as zero.
  Rating,
  /// How well a package matches the [search](BrowseQuery::search), best
  /// first in [`Descending`](SortDirection::Descending) order, ignoring
  /// community pins. With no search words this sorts like
  /// [`LastUpdated`](SortKey::LastUpdated), pins included.
  ///
  /// The first best-match search on an index folds every package's text and
  /// keeps it with the index, so later searches only compare.
  BestMatch,
}

/// Which way [`browse`] orders its [`SortKey`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortDirection {
  /// Smallest, earliest, or `A` first.
  Ascending,
  /// Largest, latest, or `Z` first.
  #[default]
  Descending,
}

/// What [`browse`] keeps from the package index and in what order.
#[derive(Debug, Clone)]
pub struct BrowseQuery<'a> {
  search: &'a str,
  show_deprecated: bool,
  show_nsfw: bool,
  categories: Vec<&'a str>,
  sort: SortKey,
  direction: SortDirection,
  pinned_first: bool,
}

impl Default for BrowseQuery<'_> {
  fn default() -> Self {
    Self {
      search: "",
      show_deprecated: false,
      show_nsfw: false,
      categories: Vec::new(),
      sort: SortKey::default(),
      direction: SortDirection::default(),
      pinned_first: true,
    }
  }
}

impl<'a> BrowseQuery<'a> {
  /// A query with r2modman's browse defaults: deprecated and NSFW packages
  /// hidden, community-pinned packages first, then most recently updated
  /// first.
  pub fn new() -> Self {
    Self::default()
  }

  /// Puts packages the community pinned ahead of the rest when `first` is
  /// true. Each group is sorted on its own.
  pub fn pinned_first(mut self, first: bool) -> Self {
    self.pinned_first = first;

    self
  }

  /// Orders packages by `key` in `direction`. Packages with equal keys keep
  /// their index order.
  pub fn sort(mut self, key: SortKey, direction: SortDirection) -> Self {
    self.sort = key;
    self.direction = direction;

    self
  }

  /// Keeps packages marked deprecated when `show` is true.
  pub fn show_deprecated(mut self, show: bool) -> Self {
    self.show_deprecated = show;

    self
  }

  /// Keeps packages marked as having NSFW content when `show` is true.
  pub fn show_nsfw(mut self, show: bool) -> Self {
    self.show_nsfw = show;

    self
  }

  /// Keeps packages matching every word of `search`, split on whitespace and
  /// underscores. A word matches when the package's name, owner, or latest
  /// description contains it, ignoring case.
  ///
  /// Under [`SortKey::BestMatch`], keeps every package that matches by name,
  /// owner, description, or a one-letter typo in a word of five or more
  /// letters, and ranks them: exact name, exact `Owner-Name`, name prefix,
  /// search words starting name words, name substring, owner prefix, whole
  /// words, substrings, typos. Packages that rank equally lead with more
  /// downloads and a better rating. Case, `_`, `-`, spaces, and camelCase
  /// breaks are ignored, so `valheim plus` matches `ValheimPlus`.
  pub fn search(mut self, search: &'a str) -> Self {
    self.search = search;

    self
  }

  /// Keeps packages in at least one of `categories`. No categories keeps
  /// every package.
  pub fn categories(mut self, categories: impl IntoIterator<Item = &'a str>) -> Self {
    self.categories = categories.into_iter().collect();

    self
  }
}

/// The indices of the packages in `index` that `query` keeps, in its order.
///
/// Reads only package-level columns and each package's latest version, so it
/// never builds a [`Package`](crate::models::Package).
pub fn browse(index: &PackageIndex, query: &BrowseQuery) -> Vec<usize> {
  let categories: Vec<InternKey> = query
    .categories
    .iter()
    .filter_map(|category| index.interner.get(category))
    .collect();
  let shown = |&idx: &usize| {
    let row = PackageRow { index, idx };

    if (row.is_deprecated() && !query.show_deprecated) || (row.is_nsfw() && !query.show_nsfw) {
      return false;
    }

    query.categories.is_empty()
      || index.categories[idx]
        .iter()
        .any(|category| categories.contains(category))
  };

  if query.sort == SortKey::BestMatch
    && let Some(needle) = search::Needle::new(query.search)
  {
    return search::rank(index, &needle, query.direction, |idx| shown(&idx));
  }

  let search = match query.sort {
    SortKey::BestMatch => "",
    _ => query.search,
  };
  let words: Vec<String> = search
    .split(|c: char| c.is_whitespace() || c == '_')
    .filter(|word| !word.is_empty())
    .map(str::to_lowercase)
    .collect();

  let mut kept: Vec<usize> = (0..index.len())
    .filter(shown)
    .filter(|&idx| {
      let row = PackageRow { index, idx };

      words.iter().all(|word| {
        let matches =
          |field: Option<&str>| field.is_some_and(|field| contains_ignoring_case(field, word));

        matches(row.full_name())
          || matches(row.name())
          || matches(row.owner())
          || matches(row.description())
      })
    })
    .collect();

  match query.sort {
    SortKey::LastUpdated | SortKey::BestMatch => {
      sort_rows(&mut kept, index, query, |row| row.date_updated())
    }
    SortKey::Name => sort_rows(&mut kept, index, query, |row| {
      IgnoringCase(row.name().unwrap_or_default())
    }),
    SortKey::Downloads => sort_rows(&mut kept, index, query, |row| row.downloads()),
    SortKey::Rating => sort_rows(&mut kept, index, query, |row| row.rating()),
  }

  kept
}

/// Stably sorts `kept` by `key` in the query's direction, behind the pinned
/// group when the query asks for one. Reads each key once.
fn sort_rows<'a, K: Ord>(
  kept: &mut [usize],
  index: &'a PackageIndex,
  query: &BrowseQuery,
  key: impl Fn(PackageRow<'a>) -> K,
) {
  let mut keyed: Vec<(bool, K, usize)> = kept
    .iter()
    .map(|&idx| {
      let row = PackageRow { index, idx };

      (query.pinned_first && row.is_pinned(), key(row), idx)
    })
    .collect();

  keyed.sort_by(|(a_pinned, a, _), (b_pinned, b, _)| {
    b_pinned.cmp(a_pinned).then_with(|| match query.direction {
      SortDirection::Ascending => a.cmp(b),
      SortDirection::Descending => b.cmp(a),
    })
  });

  for (slot, (_, _, idx)) in kept.iter_mut().zip(keyed) {
    *slot = idx;
  }
}

/// A string that compares by its lowercase form.
struct IgnoringCase<'a>(&'a str);

impl PartialEq for IgnoringCase<'_> {
  fn eq(&self, other: &Self) -> bool {
    self.cmp(other).is_eq()
  }
}

impl Eq for IgnoringCase<'_> {}

impl Ord for IgnoringCase<'_> {
  fn cmp(&self, other: &Self) -> std::cmp::Ordering {
    self
      .0
      .chars()
      .flat_map(char::to_lowercase)
      .cmp(other.0.chars().flat_map(char::to_lowercase))
  }
}

impl PartialOrd for IgnoringCase<'_> {
  fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
    Some(self.cmp(other))
  }
}

/// Whether `haystack` contains `needle`, which must already be lowercase.
///
/// Allocates only for a non-ASCII needle: a search runs on every keystroke
/// over the whole index.
fn contains_ignoring_case(haystack: &str, needle: &str) -> bool {
  if !needle.is_ascii() {
    return haystack.to_lowercase().contains(needle);
  }

  haystack
    .as_bytes()
    .windows(needle.len())
    .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

/// The fields a package list row shows, read from the index on demand.
///
/// Borrows the index and builds nothing up front, so making one per visible
/// row is free.
#[derive(Debug, Clone, Copy)]
pub struct PackageRow<'a> {
  index: &'a PackageIndex,
  idx: usize,
}

impl<'a> PackageRow<'a> {
  /// The row for the package at `idx`, or `None` if `idx` is out of range.
  pub fn new(index: &'a PackageIndex, idx: usize) -> Option<Self> {
    (idx < index.len()).then_some(Self { index, idx })
  }

  /// The package's position in the index.
  pub fn index(&self) -> usize {
    self.idx
  }

  /// The short package name.
  pub fn name(&self) -> Option<&'a str> {
    self.index.name_at(self.idx)
  }

  /// The `Owner-Name` identifier.
  pub fn full_name(&self) -> Option<&'a str> {
    self.index.full_name_at(self.idx)
  }

  /// The package owner.
  pub fn owner(&self) -> Option<&'a str> {
    self.index.owner_at(self.idx)
  }

  /// The latest version's description.
  pub fn description(&self) -> Option<&'a str> {
    let ver_idx = self.index.get_latest_version_at(self.idx)?;
    let key = self.index.versions.descriptions[ver_idx]?;

    Some(self.index.interner.resolve(&key))
  }

  /// The latest version's number, such as `1.2.0`.
  pub fn version_number(&self) -> Option<&'a str> {
    self.index.latest_version_number_at(self.idx)
  }

  /// The latest version's icon URL.
  pub fn icon(&self) -> Option<&'a str> {
    let ver_idx = self.index.get_latest_version_at(self.idx)?;
    let key = self.index.versions.icons[ver_idx]?;

    Some(self.index.interner.resolve(&key))
  }

  /// The package's categories, in index order.
  pub fn categories(&self) -> impl Iterator<Item = &'a str> + use<'a> {
    let index = self.index;

    index.categories[self.idx]
      .iter()
      .map(move |key| index.interner.resolve(key))
  }

  /// When the package was last updated.
  pub fn date_updated(&self) -> OffsetDateTime {
    self.index.dates_updated[self.idx]
  }

  /// Downloads summed over every version.
  pub fn downloads(&self) -> u64 {
    let (start, end) = self.index.version_ranges[self.idx];

    self.index.versions.downloads[start..end]
      .iter()
      .map(|count| u64::from(count.unwrap_or(0)))
      .sum()
  }

  /// The rating score. Unset reads as zero.
  pub fn rating(&self) -> u32 {
    self.index.rating_scores[self.idx].unwrap_or(0)
  }

  /// Whether the community pinned the package. Unset reads as false.
  pub fn is_pinned(&self) -> bool {
    self.index.is_pinned[self.idx].unwrap_or(false)
  }

  /// Whether the package is marked deprecated. Unset reads as false.
  pub fn is_deprecated(&self) -> bool {
    self.index.is_deprecated[self.idx].unwrap_or(false)
  }

  /// Whether the package is marked as having NSFW content. Unset reads as
  /// false.
  pub fn is_nsfw(&self) -> bool {
    self.index.has_nsfw_content[self.idx].unwrap_or(false)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::models::{Package, PackageIndex, Version};
  use crate::profile::modlist::SemVer;
  use time::{Duration, OffsetDateTime};

  /// A package whose versions are published in slice order (last is most recent).
  fn package(full_name: &str, versions: &[&str]) -> Package {
    let (owner, name) = full_name.split_once('-').unwrap();
    let start = OffsetDateTime::UNIX_EPOCH;

    Package {
      name: Some(name.to_string()),
      full_name: Some(full_name.to_string()),
      owner: Some(owner.to_string()),
      package_url: None,
      date_created: start,
      date_updated: start,
      uuid4: None,
      rating_score: None,
      is_pinned: None,
      is_deprecated: None,
      has_nsfw_content: None,
      categories: vec![],
      versions: versions
        .iter()
        .enumerate()
        .map(|(day, version)| Version {
          name: Some(name.to_string()),
          full_name: Some(format!("{full_name}-{version}")),
          description: None,
          icon: None,
          version_number: Some(version.to_string()),
          dependencies: vec![],
          download_url: None,
          downloads: None,
          date_created: start + Duration::days(day as i64),
          website_url: None,
          is_active: Some(true),
          uuid4: None,
          file_size: None,
        })
        .collect(),
    }
  }

  fn update(full_name: &str, installed: &str, latest: &str) -> OutdatedPackage {
    OutdatedPackage {
      full_name: full_name.to_string(),
      installed: SemVer::parse(installed),
      latest: SemVer::parse(latest),
    }
  }

  fn installed(full_name: &str, version: &str, dependencies: &[&str]) -> ProfileMod {
    let (author, name) = full_name.split_once('-').unwrap();

    ProfileMod {
      manifest_version: 1,
      name: full_name.to_string(),
      author_name: author.to_string(),
      website_url: String::new(),
      display_name: name.to_string(),
      description: String::new(),
      game_version: "0".to_string(),
      network_mode: "both".to_string(),
      package_type: "other".to_string(),
      install_mode: "managed".to_string(),
      installed_at_time: 0,
      loaders: vec![],
      dependencies: dependencies.iter().map(|d| d.to_string()).collect(),
      incompatibilities: vec![],
      optional_dependencies: vec![],
      version_number: SemVer::parse(version),
      enabled: true,
      icon: String::new(),
      online_source: true,
      trusted_package: false,
    }
  }

  fn names(mods: &[&ProfileMod]) -> Vec<String> {
    mods.iter().map(|m| m.name.clone()).collect()
  }

  fn profile() -> Vec<ProfileMod> {
    vec![
      installed("bbepis-BepInExPack", "5.4.2100", &[]),
      installed("Owner1-Core", "1.0.0", &["bbepis-BepInExPack-5.4.2100"]),
      installed("Owner2-UsesCore", "2.0.0", &["Owner1-Core-1.0.0"]),
      installed("Owner3-Standalone", "1.0.0", &[]),
      installed(
        "Owner4-UsesBoth",
        "1.0.0",
        &["Owner2-UsesCore-2.0.0", "Owner3-Standalone-1.0.0"],
      ),
    ]
  }

  /// A one-version package listed with `description`, with every flag unset.
  fn listing(full_name: &str, description: &str) -> Package {
    let mut listed = package(full_name, &["1.0.0"]);
    listed.versions[0].description = Some(description.to_string());

    listed
  }

  pub(super) fn browse_names(index: &PackageIndex, query: &BrowseQuery) -> Vec<String> {
    browse(index, query)
      .into_iter()
      .map(|idx| index.full_name_at(idx).unwrap().to_string())
      .collect()
  }

  #[test]
  fn browse_search_matches_name_owner_or_description_ignoring_case() {
    let index: PackageIndex = vec![
      listing("Owner1-MoreCompany", "Lets more players join"),
      listing("Sigurd-Skins", "Cosmetics"),
      listing("Owner2-Flashlight", "A brighter LAMP for the ship"),
      listing("Owner3-Unrelated", "Nothing here"),
      listing("Owner4-Accents", "ÜBER Café mod"),
    ]
    .into();

    let names = |search: &str| browse_names(&index, &BrowseQuery::new().search(search));

    assert_eq!(names("morecomp"), ["Owner1-MoreCompany"]);
    assert_eq!(names("SIGURD"), ["Sigurd-Skins"]);
    assert_eq!(names("lamp"), ["Owner2-Flashlight"]);
    assert_eq!(names("ship brighter"), ["Owner2-Flashlight"]);
    assert_eq!(names("more_company"), ["Owner1-MoreCompany"]);
    assert_eq!(names("über CAFÉ"), ["Owner4-Accents"]);
    assert_eq!(
      names("sigurd-SKINS"),
      ["Sigurd-Skins"],
      "a pasted Owner-Name"
    );
    assert_eq!(names("brighter sigurd"), Vec::<String>::new());
    assert_eq!(names("  ").len(), 5);
  }

  #[test]
  fn browse_hides_deprecated_and_nsfw_packages_until_shown() {
    let mut deprecated = listing("Owner1-Old", "");
    deprecated.is_deprecated = Some(true);
    let mut nsfw = listing("Owner2-Lewd", "");
    nsfw.has_nsfw_content = Some(true);
    let mut current = listing("Owner3-Current", "");
    current.is_deprecated = Some(false);
    current.has_nsfw_content = Some(false);
    let index: PackageIndex = vec![deprecated, nsfw, current, listing("Owner4-Unset", "")].into();

    let shown = |query: BrowseQuery| browse_names(&index, &query);

    assert_eq!(
      shown(BrowseQuery::new()),
      ["Owner3-Current", "Owner4-Unset"]
    );
    assert_eq!(
      shown(BrowseQuery::new().show_deprecated(true)),
      ["Owner1-Old", "Owner3-Current", "Owner4-Unset"]
    );
    assert_eq!(
      shown(BrowseQuery::new().show_nsfw(true)),
      ["Owner2-Lewd", "Owner3-Current", "Owner4-Unset"]
    );
  }

  #[test]
  fn browse_keeps_packages_in_any_chosen_category() {
    let in_categories = |full_name: &str, categories: &[&str]| {
      let mut listed = listing(full_name, "");
      listed.categories = categories.iter().map(|c| c.to_string()).collect();

      listed
    };
    let index: PackageIndex = vec![
      in_categories("Owner1-Suits", &["Mods", "Suits"]),
      in_categories("Owner2-Pack", &["Modpacks"]),
      in_categories("Owner3-Music", &["Audio", "Mods"]),
      in_categories("Owner4-None", &[]),
    ]
    .into();

    let kept = |categories: &[&str]| {
      browse_names(
        &index,
        &BrowseQuery::new().categories(categories.iter().copied()),
      )
    };

    assert_eq!(kept(&["Suits"]), ["Owner1-Suits"]);
    assert_eq!(
      kept(&["Audio", "Modpacks"]),
      ["Owner2-Pack", "Owner3-Music"]
    );
    assert_eq!(kept(&["NotACategory"]), Vec::<String>::new());
    assert_eq!(kept(&[]).len(), 4);
  }

  /// A package updated `updated_day` days after the epoch, whose versions were
  /// each downloaded `downloads` times.
  pub(super) fn ranked(
    full_name: &str,
    updated_day: i64,
    rating: u32,
    downloads: &[u32],
  ) -> Package {
    let numbers: Vec<String> = (0..downloads.len()).map(|n| format!("1.0.{n}")).collect();
    let numbers: Vec<&str> = numbers.iter().map(String::as_str).collect();
    let mut listed = package(full_name, &numbers);
    listed.date_updated = OffsetDateTime::UNIX_EPOCH + Duration::days(updated_day);
    listed.rating_score = Some(rating);

    for (version, &count) in listed.versions.iter_mut().zip(downloads) {
      version.downloads = Some(count);
    }

    listed
  }

  #[test]
  fn browse_sorts_by_each_key_in_both_directions() {
    let index: PackageIndex = vec![
      ranked("Owner1-bravo", 2, 10, &[50, 60]),
      ranked("Owner2-Alpha", 3, 30, &[100]),
      ranked("Owner3-charlie", 1, 20, &[5, 5, 5]),
    ]
    .into();

    let sorted = |key: SortKey, direction: SortDirection| {
      browse_names(&index, &BrowseQuery::new().sort(key, direction))
    };

    use SortDirection::{Ascending, Descending};

    assert_eq!(
      sorted(SortKey::LastUpdated, Descending),
      ["Owner2-Alpha", "Owner1-bravo", "Owner3-charlie"]
    );
    assert_eq!(
      sorted(SortKey::LastUpdated, Ascending),
      ["Owner3-charlie", "Owner1-bravo", "Owner2-Alpha"]
    );
    assert_eq!(
      sorted(SortKey::Name, Ascending),
      ["Owner2-Alpha", "Owner1-bravo", "Owner3-charlie"]
    );
    assert_eq!(
      sorted(SortKey::Name, Descending),
      ["Owner3-charlie", "Owner1-bravo", "Owner2-Alpha"]
    );
    assert_eq!(
      sorted(SortKey::Downloads, Descending),
      ["Owner1-bravo", "Owner2-Alpha", "Owner3-charlie"]
    );
    assert_eq!(
      sorted(SortKey::Downloads, Ascending),
      ["Owner3-charlie", "Owner2-Alpha", "Owner1-bravo"]
    );
    assert_eq!(
      sorted(SortKey::Rating, Descending),
      ["Owner2-Alpha", "Owner3-charlie", "Owner1-bravo"]
    );
    assert_eq!(
      sorted(SortKey::Rating, Ascending),
      ["Owner1-bravo", "Owner3-charlie", "Owner2-Alpha"]
    );
    assert_eq!(
      browse_names(&index, &BrowseQuery::new()),
      ["Owner2-Alpha", "Owner1-bravo", "Owner3-charlie"],
      "newest update first by default"
    );
  }

  #[test]
  fn browse_keeps_index_order_between_equal_keys_in_either_direction() {
    let index: PackageIndex = vec![
      ranked("Owner1-First", 1, 5, &[1]),
      ranked("Owner2-Second", 1, 5, &[1]),
      ranked("Owner3-Top", 1, 9, &[1]),
    ]
    .into();

    assert_eq!(
      browse_names(
        &index,
        &BrowseQuery::new().sort(SortKey::Rating, SortDirection::Descending)
      ),
      ["Owner3-Top", "Owner1-First", "Owner2-Second"]
    );
    assert_eq!(
      browse_names(
        &index,
        &BrowseQuery::new().sort(SortKey::Rating, SortDirection::Ascending)
      ),
      ["Owner1-First", "Owner2-Second", "Owner3-Top"]
    );
  }

  #[test]
  fn browse_leads_with_pinned_packages_sorted_within_each_group() {
    let pinned = |full_name: &str, rating: u32| {
      let mut listed = ranked(full_name, 1, rating, &[1]);
      listed.is_pinned = Some(true);

      listed
    };
    let index: PackageIndex = vec![
      ranked("Owner1-Popular", 1, 90, &[1]),
      pinned("Owner2-PinnedLow", 10),
      ranked("Owner3-Middling", 1, 50, &[1]),
      pinned("Owner4-PinnedHigh", 20),
    ]
    .into();
    let by_rating = BrowseQuery::new().sort(SortKey::Rating, SortDirection::Descending);

    assert_eq!(
      browse_names(&index, &by_rating),
      [
        "Owner4-PinnedHigh",
        "Owner2-PinnedLow",
        "Owner1-Popular",
        "Owner3-Middling"
      ]
    );
    assert_eq!(
      browse_names(&index, &by_rating.pinned_first(false)),
      [
        "Owner1-Popular",
        "Owner3-Middling",
        "Owner4-PinnedHigh",
        "Owner2-PinnedLow"
      ]
    );
  }

  #[test]
  fn package_row_reads_the_list_fields_of_a_package() {
    let mut listed = ranked("Owner1-Suits", 4, 12, &[3, 4, 5]);
    listed.versions[0].description = Some("First release".to_string());
    listed.versions[2].description = Some("Latest release".to_string());
    listed.versions[0].icon = Some("https://cdn.example/first.png".to_string());
    listed.versions[2].icon = Some("https://cdn.example/latest.png".to_string());
    listed.categories = vec!["Mods".to_string(), "Suits".to_string()];
    listed.is_pinned = Some(true);
    listed.is_deprecated = Some(true);
    listed.has_nsfw_content = Some(false);
    let index: PackageIndex = vec![listing("Owner0-Other", "Other"), listed].into();

    let row = PackageRow::new(&index, 1).unwrap();

    assert_eq!(row.index(), 1);
    assert_eq!(row.name(), Some("Suits"));
    assert_eq!(row.full_name(), Some("Owner1-Suits"));
    assert_eq!(row.owner(), Some("Owner1"));
    assert_eq!(row.description(), Some("Latest release"));
    assert_eq!(row.version_number(), Some("1.0.2"));
    assert_eq!(row.icon(), Some("https://cdn.example/latest.png"));
    assert_eq!(row.categories().collect::<Vec<_>>(), ["Mods", "Suits"]);
    assert_eq!(row.downloads(), 12);
    assert_eq!(row.rating(), 12);
    assert_eq!(
      row.date_updated(),
      OffsetDateTime::UNIX_EPOCH + Duration::days(4)
    );
    assert!(row.is_pinned());
    assert!(row.is_deprecated());
    assert!(!row.is_nsfw());
    assert!(PackageRow::new(&index, 2).is_none());
  }

  #[test]
  fn dependants_include_direct_and_transitive_requirers() {
    let mods = profile();

    assert_eq!(
      names(&dependants(&mods, "bbepis-BepInExPack")),
      ["Owner1-Core", "Owner2-UsesCore", "Owner4-UsesBoth"]
    );
    assert_eq!(
      names(&dependants(&mods, "Owner3-Standalone")),
      ["Owner4-UsesBoth"]
    );
  }

  #[test]
  fn dependants_are_empty_for_a_leaf_or_an_unknown_package() {
    let mods = profile();

    assert!(dependants(&mods, "Owner4-UsesBoth").is_empty());
    assert!(dependants(&mods, "Nobody-Missing").is_empty());
  }

  #[test]
  fn dependants_terminate_on_a_cycle_and_omit_the_package_itself() {
    let mods = vec![
      installed("Owner1-A", "1.0.0", &["Owner2-B-1.0.0"]),
      installed("Owner2-B", "1.0.0", &["Owner1-A-1.0.0"]),
      installed("Owner3-C", "1.0.0", &["Owner2-B-1.0.0"]),
    ];

    assert_eq!(
      names(&dependants(&mods, "Owner1-A")),
      ["Owner2-B", "Owner3-C"]
    );
  }

  #[test]
  fn outdated_lists_packages_with_a_newer_version_in_the_index() {
    let mods = vec![
      installed("Owner1-Core", "1.0.9", &[]),
      installed("Owner2-Current", "2.0.0", &[]),
      installed("Owner3-Behind", "1.2", &[]),
      installed("Local-NotInIndex", "1.0.0", &[]),
    ];
    let index: PackageIndex = vec![
      package("Owner3-Behind", &["1.2.0", "1.2.1"]),
      package("Owner1-Core", &["1.0.8", "1.0.9", "1.0.10"]),
      package("Owner2-Current", &["1.0.0", "2.0.0"]),
    ]
    .into();

    assert_eq!(
      outdated(&mods, &index),
      [
        update("Owner1-Core", "1.0.9", "1.0.10"),
        update("Owner3-Behind", "1.2.0", "1.2.1"),
      ]
    );
  }

  #[test]
  fn outdated_uses_the_most_recently_published_version_as_latest() {
    let mods = vec![
      installed("Owner1-OnHighest", "2.0.0", &[]),
      installed("Owner2-Behind", "1.0.0", &[]),
      installed("Owner3-AheadOfIndex", "3.0.0", &[]),
    ];
    let index: PackageIndex = vec![
      package("Owner1-OnHighest", &["1.0.0", "2.0.0", "1.5.0"]),
      package("Owner2-Behind", &["1.0.0", "2.0.0", "1.5.0"]),
      package("Owner3-AheadOfIndex", &["2.0.0"]),
    ]
    .into();

    assert_eq!(
      outdated(&mods, &index),
      [update("Owner2-Behind", "1.0.0", "1.5.0")]
    );
  }
}
