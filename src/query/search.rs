//! Best-match ranking for [`browse`](super::browse).

use super::{PackageRow, SortDirection};
use crate::models::PackageIndex;

/// How well a package matches a search, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
  /// The search is the name.
  ExactName,
  /// The search is the `Owner-Name` identifier, as pasted from a URL.
  ExactFullName,
  /// The name starts with the search.
  NamePrefix,
  /// Every search word starts a word of the name, in any order.
  NameWords,
  /// The name contains the search.
  NameSubstring,
  /// The owner starts with the search.
  Owner,
  /// Every search word is a whole word of the name, owner, or description.
  WholeWords,
  /// Every search word is inside the name, owner, or description.
  Substrings,
  /// The search is one typo from the start of the name or owner, or each
  /// search word is one typo from the start of a word of the name.
  Typo,
}

/// Words shorter than this never match by typo: one edit to a short word
/// reaches too many unrelated names.
const MIN_TYPO_LEN: usize = 5;

/// How much a package's log rating counts against its log downloads when
/// ordering packages that match equally well. Small, so a well-rated package
/// passes one with a few more downloads but not one with many times more.
const RATING_WEIGHT: f64 = 0.25;

/// A search split into the forms packages are compared in.
#[derive(Debug)]
pub(super) struct Needle {
  words: Vec<String>,
  squashed: String,
}

impl Needle {
  /// The needle for `search`, or `None` when it has no letters or digits.
  pub(super) fn new(search: &str) -> Option<Self> {
    let words = fold_words(search);
    let squashed = words.concat();

    (!words.is_empty()).then_some(Self { words, squashed })
  }
}

/// Every package's searchable text, folded once per index so a search on each
/// keystroke only compares.
#[derive(Debug, Clone)]
pub(crate) struct SearchIndex {
  entries: Vec<Entry>,
}

impl SearchIndex {
  pub(crate) fn new(index: &PackageIndex) -> Self {
    Self {
      entries: (0..index.len())
        .map(|idx| Entry::new(PackageRow { index, idx }))
        .collect(),
    }
  }
}

/// A package's searchable text, folded once.
#[derive(Debug, Clone)]
struct Entry {
  name: Folded,
  owner: Folded,
  description: String,
  popularity: f64,
}

/// Text folded into lowercase words and the same words joined.
#[derive(Debug, Clone)]
struct Folded {
  words: Vec<String>,
  squashed: String,
}

impl Folded {
  fn new(text: &str) -> Self {
    let words = fold_words(text);
    let squashed = words.concat();

    Self { words, squashed }
  }

  /// Whether `word` is one of the words, or all of them run together.
  fn has_word(&self, word: &str) -> bool {
    self.squashed == word || self.words.iter().any(|own| own == word)
  }
}

impl Entry {
  fn new(row: PackageRow) -> Self {
    Self {
      name: Folded::new(row.name().unwrap_or_default()),
      owner: Folded::new(row.owner().unwrap_or_default()),
      description: row.description().unwrap_or_default().to_lowercase(),
      popularity: (1.0 + row.downloads() as f64).ln()
        + RATING_WEIGHT * (1.0 + f64::from(row.rating())).ln(),
    }
  }

  fn tier(&self, needle: &Needle) -> Option<Tier> {
    let query = needle.squashed.as_str();
    let name = &self.name;
    let every_word = |matches: &dyn Fn(&str) -> bool| needle.words.iter().all(|word| matches(word));

    let tier = if name.squashed == query {
      Tier::ExactName
    } else if self.is_full_name(query) {
      Tier::ExactFullName
    } else if name.squashed.starts_with(query) {
      Tier::NamePrefix
    } else if every_word(&|word| name.words.iter().any(|own| own.starts_with(word))) {
      Tier::NameWords
    } else if name.squashed.contains(query) {
      Tier::NameSubstring
    } else if self.owner.squashed.starts_with(query) {
      Tier::Owner
    } else if every_word(&|word| {
      name.has_word(word) || self.owner.has_word(word) || contains_word(&self.description, word)
    }) {
      Tier::WholeWords
    } else if every_word(&|word| {
      name.squashed.contains(word)
        || self.owner.squashed.contains(word)
        || self.description.contains(word)
    }) {
      Tier::Substrings
    } else if is_typo_of_start(query, &name.squashed)
      || is_typo_of_start(query, &self.owner.squashed)
      || every_word(&|word| {
        name
          .words
          .iter()
          .any(|own| own.starts_with(word) || is_typo_of_start(word, own))
      })
    {
      Tier::Typo
    } else {
      return None;
    };

    Some(tier)
  }

  /// Whether `squashed` is this package's owner and name run together, as a
  /// pasted `Owner-Name` folds to.
  fn is_full_name(&self, squashed: &str) -> bool {
    squashed.len() == self.owner.squashed.len() + self.name.squashed.len()
      && squashed.starts_with(self.owner.squashed.as_str())
      && squashed.ends_with(self.name.squashed.as_str())
  }
}

/// The packages among `candidates` that match `needle`, best match first in
/// [`Descending`](SortDirection::Descending) order.
pub(super) fn rank(
  index: &PackageIndex,
  needle: &Needle,
  direction: SortDirection,
  candidates: impl Iterator<Item = usize>,
) -> Vec<usize> {
  let entries = &index.search_index().entries;
  let mut ranked: Vec<(Tier, f64, usize)> = candidates
    .filter_map(|idx| {
      let entry = &entries[idx];

      Some((entry.tier(needle)?, entry.popularity, idx))
    })
    .collect();

  ranked.sort_by(|(a_tier, a_popularity, _), (b_tier, b_popularity, _)| {
    let best_first = a_tier
      .cmp(b_tier)
      .then_with(|| b_popularity.total_cmp(a_popularity));

    match direction {
      SortDirection::Descending => best_first,
      SortDirection::Ascending => best_first.reverse(),
    }
  });

  ranked.into_iter().map(|(_, _, idx)| idx).collect()
}

/// Whether `typed` is at least [`MIN_TYPO_LEN`] characters and one insertion,
/// deletion, substitution, or swap of adjacent characters away from `meant`
/// or from the start of it, so a typo still matches while the rest of the
/// word is unwritten.
fn is_typo_of_start(typed: &str, meant: &str) -> bool {
  if typed.chars().count() < MIN_TYPO_LEN {
    return false;
  }

  if typed.is_ascii() && meant.is_ascii() {
    return starts_within_one_edit(typed.as_bytes(), meant.as_bytes());
  }

  let typed: Vec<char> = typed.chars().collect();
  let meant: Vec<char> = meant.chars().collect();

  starts_within_one_edit(&typed, &meant)
}

/// Whether some prefix of `text` is within one edit of `typed`.
fn starts_within_one_edit<T: PartialEq>(typed: &[T], text: &[T]) -> bool {
  let len = typed.len();

  (len.saturating_sub(1)..=len + 1)
    .any(|prefix| prefix <= text.len() && within_one_edit(typed, &text[..prefix]))
}

/// Whether `a` and `b` differ by at most one insertion, deletion,
/// substitution, or swap of adjacent elements.
fn within_one_edit<T: PartialEq>(a: &[T], b: &[T]) -> bool {
  let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };

  if long.len() - short.len() > 1 {
    return false;
  }

  let same = short.iter().zip(long).take_while(|(a, b)| a == b).count();

  if same == short.len() {
    return true;
  }

  if short.len() < long.len() {
    return short[same..] == long[same + 1..];
  }

  let substituted = short[same + 1..] == long[same + 1..];
  let swapped = same + 1 < short.len()
    && short[same] == long[same + 1]
    && short[same + 1] == long[same]
    && short[same + 2..] == long[same + 2..];

  substituted || swapped
}

/// Whether `word` occurs in `text` with no letter or digit on either side.
fn contains_word(text: &str, word: &str) -> bool {
  text.match_indices(word).any(|(at, _)| {
    let before = text[..at].chars().next_back();
    let after = text[at + word.len()..].chars().next();

    !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
  })
}

/// Splits `text` into lowercase words, breaking at every character that is
/// not a letter or digit and at camelCase boundaries (`BepInExPack` is `bep`,
/// `in`, `ex`, `pack`; `UIFixes` is `ui`, `fixes`).
fn fold_words(text: &str) -> Vec<String> {
  let chars: Vec<char> = text.chars().collect();
  let mut words = Vec::new();
  let mut word = String::new();

  for (at, &c) in chars.iter().enumerate() {
    let prev = at.checked_sub(1).map(|prev| chars[prev]);
    let next = chars.get(at + 1);
    let camel_break = c.is_uppercase()
      && prev.is_some_and(|prev| {
        prev.is_lowercase() || (prev.is_uppercase() && next.is_some_and(|next| next.is_lowercase()))
      });

    if (!c.is_alphanumeric() || camel_break) && !word.is_empty() {
      words.push(std::mem::take(&mut word));
    }

    if c.is_alphanumeric() {
      word.extend(c.to_lowercase());
    }
  }

  if !word.is_empty() {
    words.push(word);
  }

  words
}

#[cfg(test)]
mod tests {
  use crate::models::{Package, PackageIndex};
  use crate::query::tests::{browse_names, ranked};
  use crate::query::{BrowseQuery, SortDirection, SortKey};

  /// A package updated `updated_day` days after the epoch whose one version
  /// was downloaded `downloads` times and is described as `description`.
  fn listed(full_name: &str, updated_day: i64, downloads: u32, description: &str) -> Package {
    let mut listed = ranked(full_name, updated_day, 0, &[downloads]);
    listed.versions[0].description = Some(description.to_string());

    listed
  }

  fn best(index: &PackageIndex, search: &str) -> Vec<String> {
    browse_names(
      index,
      &BrowseQuery::new()
        .search(search)
        .sort(SortKey::BestMatch, SortDirection::Descending),
    )
  }

  #[test]
  fn best_match_puts_an_exact_name_ahead_of_newer_and_more_popular_mentions() {
    let index: PackageIndex = vec![
      listed("Owner1-Popular", 9, 900, "Requires Jotunn"),
      listed("ValheimModding-Jotunn", 1, 10, "The Valheim library"),
      listed("Owner2-Newest", 10, 5, "Built on jotunn"),
    ]
    .into();

    assert_eq!(
      best(&index, "jotunn"),
      ["ValheimModding-Jotunn", "Owner1-Popular", "Owner2-Newest"]
    );
  }

  #[test]
  fn best_match_compares_names_ignoring_case_separators_and_camel_case() {
    let index: PackageIndex = vec![
      listed("Owner1-Popular", 0, 900, "A ValheimPlus fork"),
      listed("Owner2-ValheimPlus", 0, 10, ""),
    ]
    .into();

    for search in [
      "valheim plus",
      "ValheimPlus",
      "valheim_plus",
      "VALHEIM-PLUS",
      "valheimplus",
    ] {
      assert_eq!(
        best(&index, search),
        ["Owner2-ValheimPlus", "Owner1-Popular"],
        "{search}"
      );
    }
  }

  #[test]
  fn best_match_puts_a_pasted_owner_name_after_exact_names_and_before_prefixes() {
    let index: PackageIndex = vec![
      listed("Other-RandyKnappEpicLootPatch", 0, 900, ""),
      listed("RandyKnapp-EpicLoot", 0, 10, ""),
      listed("Someone-RandyKnappEpicLoot", 0, 1, ""),
    ]
    .into();

    assert_eq!(
      best(&index, "RandyKnapp-EpicLoot"),
      [
        "Someone-RandyKnappEpicLoot",
        "RandyKnapp-EpicLoot",
        "Other-RandyKnappEpicLootPatch"
      ]
    );
  }

  #[test]
  fn best_match_ranks_a_name_prefix_then_word_starts_then_a_substring() {
    let index: PackageIndex = vec![
      listed("Owner1-Popular", 0, 900, "An epic loot addon"),
      listed("Owner2-SuperepicLootbox", 0, 100, ""),
      listed("Owner3-LootOfEpicProportions", 0, 50, ""),
      listed("Owner4-EpicLootPatch", 0, 1, ""),
    ]
    .into();

    assert_eq!(
      best(&index, "epic loot"),
      [
        "Owner4-EpicLootPatch",
        "Owner3-LootOfEpicProportions",
        "Owner2-SuperepicLootbox",
        "Owner1-Popular"
      ]
    );
  }

  #[test]
  fn best_match_ranks_an_owner_after_names_and_before_descriptions() {
    let index: PackageIndex = vec![
      listed("Owner1-Popular", 0, 900, "A fork of RandyKnapp's slots"),
      listed("RandyKnapp-Jam", 0, 5, ""),
      listed("RandyKnapp-EpicLoot", 0, 500, ""),
      listed("Fan-ILoveRandyKnapp", 0, 1, ""),
    ]
    .into();

    assert_eq!(
      best(&index, "randyknapp"),
      [
        "Fan-ILoveRandyKnapp",
        "RandyKnapp-EpicLoot",
        "RandyKnapp-Jam",
        "Owner1-Popular"
      ]
    );
    assert_eq!(
      best(&index, "randyk"),
      [
        "Fan-ILoveRandyKnapp",
        "RandyKnapp-EpicLoot",
        "RandyKnapp-Jam",
        "Owner1-Popular"
      ],
      "an owner prefix matches while typing"
    );
  }

  #[test]
  fn best_match_ranks_whole_words_ahead_of_substrings() {
    let index: PackageIndex = vec![
      listed(
        "Owner1-Popular",
        0,
        900,
        "Clamps and knappsacks for the lootbox",
      ),
      listed("Owner2-Flashlight", 0, 10, "A brighter lamp"),
      listed("RandyKnapp-EpicLoot", 0, 1, ""),
    ]
    .into();

    assert_eq!(
      best(&index, "lamp"),
      ["Owner2-Flashlight", "Owner1-Popular"]
    );
    assert_eq!(
      best(&index, "knapp loot"),
      ["RandyKnapp-EpicLoot", "Owner1-Popular"],
      "whole words count in the owner and name too"
    );
  }

  #[test]
  fn best_match_tolerates_one_typo_in_a_long_word_below_every_real_match() {
    let index: PackageIndex = vec![
      listed("Owner1-Mention", 0, 1, "For epicloott fans"),
      listed("RandyKnapp-EpicLoot", 0, 500, ""),
      listed("ValheimModding-Jotunn", 0, 500, ""),
    ]
    .into();

    assert_eq!(
      best(&index, "epicloott"),
      ["Owner1-Mention", "RandyKnapp-EpicLoot"]
    );
    assert_eq!(best(&index, "jotnun"), ["ValheimModding-Jotunn"]);
    assert_eq!(best(&index, "epic lootz"), ["RandyKnapp-EpicLoot"]);
    assert_eq!(
      best(&index, "loat"),
      Vec::<String>::new(),
      "too short to guess"
    );
  }

  #[test]
  fn best_match_tolerates_a_typo_in_a_name_prefix_or_an_owner() {
    let index: PackageIndex = vec![
      listed("denikson-BepInExPack_Valheim", 0, 500, ""),
      listed("RandyKnapp-EpicLoot", 0, 500, ""),
    ]
    .into();

    assert_eq!(best(&index, "bepinx"), ["denikson-BepInExPack_Valheim"]);
    assert_eq!(best(&index, "randykanpp"), ["RandyKnapp-EpicLoot"]);
  }

  #[test]
  fn best_match_breaks_ties_by_downloads_nudged_by_rating() {
    let index: PackageIndex = vec![
      ranked("Unrated-Jotunn", 0, 0, &[600, 400]),
      ranked("Rated-Jotunn", 0, 50, &[1000]),
      ranked("Downloaded-Jotunn", 0, 0, &[100_000]),
    ]
    .into();

    assert_eq!(
      best(&index, "jotunn"),
      ["Downloaded-Jotunn", "Rated-Jotunn", "Unrated-Jotunn"]
    );
  }

  #[test]
  fn best_match_keeps_the_filters_and_leads_with_pinned_only_without_a_search() {
    let mut pinned = listed("Pinned-Helper", 1, 1, "A jotunn helper");
    pinned.is_pinned = Some(true);
    let mut deprecated = listed("Old-Jotunn", 9, 1000, "");
    deprecated.is_deprecated = Some(true);
    let index: PackageIndex = vec![
      pinned,
      listed("ValheimModding-Jotunn", 3, 10, ""),
      deprecated,
      listed("Owner1-Newer", 5, 10, "Unrelated"),
    ]
    .into();
    let best_match = BrowseQuery::new().sort(SortKey::BestMatch, SortDirection::Descending);

    assert_eq!(
      best(&index, "jotunn"),
      ["ValheimModding-Jotunn", "Pinned-Helper"]
    );
    assert_eq!(
      browse_names(
        &index,
        &best_match.clone().search("jotunn").show_deprecated(true)
      ),
      ["Old-Jotunn", "ValheimModding-Jotunn", "Pinned-Helper"]
    );
    assert_eq!(
      browse_names(
        &index,
        &best_match
          .clone()
          .search("jotunn")
          .sort(SortKey::BestMatch, SortDirection::Ascending)
      ),
      ["Pinned-Helper", "ValheimModding-Jotunn"]
    );
    assert_eq!(
      browse_names(&index, &best_match.search(" - ")),
      ["Pinned-Helper", "Owner1-Newer", "ValheimModding-Jotunn"],
      "no search words sorts like LastUpdated"
    );
  }

  /// Rows from the Valheim index in November 2026: `Owner-Name`, downloads,
  /// rating, deprecated, and description.
  const VALHEIM: &[(&str, u32, u32, bool, &str)] = &[
    (
      "ValheimModding-Jotunn",
      4_961_152,
      377,
      false,
      "Jötunn (/ˈjɔːtʊn/, 'giant'), the Valheim Library was created with the goal of making the lives of mod developers easier.",
    ),
    (
      "JotunnLibTeam-JotunnLib",
      67_229,
      14,
      true,
      "JotunnLib is a modding library for Valheim, with the goal of making the lives of mod developers easier.",
    ),
    (
      "sixefyle-BiggerItemStack",
      50_141,
      6,
      false,
      "Configurable item stack sizes, item weights and player carry capacity, with Jotunn server sync.",
    ),
    (
      "Ostrix-AdvancedTerrainModifiersCompatible",
      15_293,
      2,
      false,
      "Maintained AdvancedTerrainModifiers compatibility fork for current Valheim, Jotunn and EpicLoot.",
    ),
    (
      "cjayride-Vigor",
      1_843,
      2,
      false,
      "Vitality + Constitution in one 1.0 Jotunn plugin.",
    ),
    (
      "RandyKnapp-EpicLoot",
      2_448_991,
      367,
      false,
      "Adds loot drops, magic items, and enchanting to Valheim.",
    ),
    (
      "RandyKnapp-EquipmentAndQuickSlots",
      1_644_798,
      341,
      false,
      "Give equipped items their own dedicated inventory slots, plus up to six hotkeyable quick slots.",
    ),
    (
      "RandyKnapp-Jam",
      45_571,
      15,
      false,
      "Adds more jam to the game.",
    ),
    (
      "JewelHeim-RelicHeim",
      184_554,
      36,
      true,
      "Public Modpack using EpicLoot that's balanced and focuses on enchanting instead of mass drops.",
    ),
    (
      "JewelHeim-EpicLoot_Therzie",
      179_948,
      35,
      true,
      "Patches made for Therzies mods, Warfare+, Monstrum+, Wizardry and Armory.",
    ),
    (
      "Tenemo-EpicLoot_ProgressionFix",
      57_156,
      4,
      false,
      "Opinionated EpicLoot balance, crafting progression, combat, loot and inventory fixes.",
    ),
    (
      "Digitalroot-Bounties",
      96_380,
      16,
      false,
      "Adds 275+ bounties to Epic Loot's Adventure mode.",
    ),
    (
      "ViperAlpha-EquipmentAndQuickSlotsByViper",
      825,
      0,
      false,
      "Unofficial fork of RandyKnapp's EquipmentAndQuickSlots 3.1.3 for Valheim 1.0.",
    ),
    (
      "OdinPlus-TeleportEverything",
      802_005,
      133,
      false,
      "Teleport Everything 3 with UI is the official OdinPlus modularized pack for Teleport Creatures.",
    ),
    (
      "Grantapher-ValheimPlus_Grantapher_Temporary",
      400_197,
      68,
      false,
      "A (not so) temporary Valheim Plus fork. Provides many configurable game tweaks.",
    ),
    ("ValheimPlus-ValheimPlus", 362_984, 79, true, "Valheim Plus"),
    (
      "ValheimPlus1-ValheimPlus",
      22_482,
      1,
      true,
      "This is a just a version of Valheim with a few small QOL mods",
    ),
    (
      "Azumatt-Valheim_Minus",
      21_109,
      15,
      true,
      "Patcher plugin to remove ValheimPlus on startup even before it loads.",
    ),
    (
      "KillerGoldFisch-ValheimFPSPlus",
      36_062,
      10,
      false,
      "This Valheim mod allows you to change the quality settings Valheim doesn't give you access to.",
    ),
    (
      "denikson-BepInExPack_Valheim",
      7_712_819,
      729,
      false,
      "BepInEx pack for Valheim. Preconfigured with the correct entry point for mods and preferred defaults for the community.",
    ),
    (
      "Azumatt-Official_BepInEx_ConfigurationManager",
      823_102,
      127,
      true,
      "Mod to assist with configuration of BepInEx mods",
    ),
    (
      "ValheimModding-HookGenPatcher",
      718_232,
      56,
      false,
      "Generates MonoMod.RuntimeDetour.HookGen's MMHOOK file during the BepInEx preloader phase.",
    ),
    (
      "TJzilla-BepInEx_ConfigurationManager",
      73_582,
      16,
      true,
      "Mod to assist with configuration of BepInEx mods",
    ),
    (
      "1F31A-BepInEx_Valheim_Full",
      39_125,
      6,
      true,
      "BepInEx pack for Valheim.",
    ),
  ];

  #[test]
  fn best_match_puts_the_package_a_player_means_first_in_the_valheim_index() {
    let index: PackageIndex = VALHEIM
      .iter()
      .enumerate()
      .map(
        |(day, &(full_name, downloads, rating, deprecated, description))| {
          let mut listed = ranked(full_name, day as i64, rating, &[downloads]);
          listed.versions[0].description = Some(description.to_string());
          listed.is_deprecated = Some(deprecated);

          listed
        },
      )
      .collect::<Vec<_>>()
      .into();
    let first = |search: &str, show_deprecated: bool| {
      browse_names(
        &index,
        &BrowseQuery::new()
          .search(search)
          .show_deprecated(show_deprecated)
          .sort(SortKey::BestMatch, SortDirection::Descending),
      )
      .into_iter()
      .next()
    };

    let golden = [
      ("jotunn", "ValheimModding-Jotunn"),
      ("jotun", "ValheimModding-Jotunn"),
      ("jotnun", "ValheimModding-Jotunn"),
      ("epic loot", "RandyKnapp-EpicLoot"),
      ("loot epic", "RandyKnapp-EpicLoot"),
      ("epiclot", "RandyKnapp-EpicLoot"),
      ("RandyKnapp-EpicLoot", "RandyKnapp-EpicLoot"),
      ("randyknapp", "RandyKnapp-EpicLoot"),
      ("randykanpp", "RandyKnapp-EpicLoot"),
      (
        "valheim plus",
        "Grantapher-ValheimPlus_Grantapher_Temporary",
      ),
      ("ValheimPlus", "Grantapher-ValheimPlus_Grantapher_Temporary"),
      (
        "valheim_plus",
        "Grantapher-ValheimPlus_Grantapher_Temporary",
      ),
      ("bepinex", "denikson-BepInExPack_Valheim"),
      ("bepinx", "denikson-BepInExPack_Valheim"),
    ];

    for (search, expected) in golden {
      assert_eq!(first(search, false).as_deref(), Some(expected), "{search}");
    }

    assert_eq!(
      first("valheim plus", true).as_deref(),
      Some("ValheimPlus-ValheimPlus"),
      "the original leads once deprecated packages show"
    );
  }
}
