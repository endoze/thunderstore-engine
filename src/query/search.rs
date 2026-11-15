//! Best-match ranking for [`browse`](super::browse).

use super::{PackageRow, SortDirection};
use crate::models::PackageIndex;
use memchr::memmem::Finder;
use std::fmt;

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

impl Tier {
  const COUNT: usize = Tier::Typo as usize + 1;
}

/// Words shorter than this never match by typo: one edit to a short word
/// reaches too many unrelated names.
const MIN_TYPO_LEN: usize = 5;

/// How much a package's log rating counts against its log downloads when
/// ordering packages that match equally well. Small, so a well-rated package
/// passes one with a few more downloads but not one with many times more.
const RATING_WEIGHT: f64 = 0.25;

/// Ends every field in a [`SearchIndex`] buffer. Search words hold only
/// letters and digits, so a word found in a buffer never runs across two
/// fields.
const FIELD_END: char = '\n';

/// A search split into the forms packages are compared in.
#[derive(Debug)]
pub(super) struct Needle {
  squashed: String,
  ends: Vec<u32>,
  /// A finder for each word, in order.
  finders: Vec<Finder<'static>>,
  /// What a name or owner needs to be a typo of this search, when the search
  /// is long enough to match by typo.
  typo: Option<TypoAnchors>,
}

impl Needle {
  /// The needle for `search`, or `None` when it has no letters or digits.
  pub(super) fn new(search: &str) -> Option<Self> {
    let mut squashed = String::new();
    let mut ends = Vec::new();

    fold_into(search, &mut squashed, &mut ends);

    if ends.is_empty() {
      return None;
    }

    let words = Words::new(&squashed, &ends);
    let finders = words
      .clone()
      .map(|word| Finder::new(word).into_owned())
      .collect();
    let typo = Anchors::new(&squashed).map(|query| {
      let first = words.clone().next().unwrap_or_default();
      let first_word = match Anchors::new(first) {
        Some(anchors) => FirstWord::Anchors(anchors),
        None => FirstWord::Prefix(first.to_string()),
      };

      TypoAnchors { query, first_word }
    });

    Some(Self {
      squashed,
      ends,
      finders,
      typo,
    })
  }

  fn words(&self) -> Words<'_> {
    Words::new(&self.squashed, &self.ends)
  }
}

/// What a package's name or owner needs before it is worth checking for a
/// typo of a search: the search's anchors at the start of the name or owner,
/// or the first search word's at the start of a name word.
#[derive(Debug)]
struct TypoAnchors {
  query: Anchors,
  first_word: FirstWord,
}

/// What a name word needs to match the first search word by typo.
#[derive(Debug)]
enum FirstWord {
  /// The word is long enough for a typo, so the name word needs its anchors.
  Anchors(Anchors),
  /// The word is too short for a typo, so the name word has to start with it.
  Prefix(String),
}

impl FirstWord {
  fn fits(&self, text: &str) -> bool {
    match self {
      FirstWord::Anchors(anchors) => anchors.fit(text),
      FirstWord::Prefix(word) => text.starts_with(word.as_str()),
    }
  }
}

/// The two pieces of a typed word that a typo keeps. Cutting the word at
/// [`anchor_split`], one edit past the cut leaves the `head` before it at the
/// start, and one edit before the cut leaves the `tail` after it in place,
/// shifted by at most one, so text holding neither is no typo of the word.
#[derive(Debug)]
struct Anchors {
  head: String,
  tail: String,
  /// Where the tail sits in the typed word, less one: the head's length.
  split: usize,
}

impl Anchors {
  /// The anchors of `typed`, or `None` when it is too short to match by typo.
  fn new(typed: &str) -> Option<Self> {
    let len = typed.chars().count();
    let split = anchor_split(len);
    let at = |nth: usize| {
      typed
        .char_indices()
        .nth(nth)
        .map_or(typed.len(), |(at, _)| at)
    };

    (len >= MIN_TYPO_LEN).then(|| Self {
      head: typed[..at(split)].to_string(),
      tail: typed[at(split + 1)..].to_string(),
      split,
    })
  }

  /// Whether ASCII `text` keeps either anchor where one edit would leave it.
  fn fit(&self, text: &str) -> bool {
    let text = text.as_bytes();
    let tail = self.tail.as_bytes();

    text.starts_with(self.head.as_bytes())
      || (self.split..=self.split + 2).any(|at| text.get(at..at + tail.len()) == Some(tail))
  }
}

/// Where [`Anchors`] cut a typed word of `len` characters: near the middle,
/// so both pieces are long enough to rule most text out.
fn anchor_split(len: usize) -> usize {
  len.saturating_sub(1) / 2
}

/// Every package's searchable text, folded once per index into two buffers so
/// a search on each keystroke scans them instead of visiting each package.
#[derive(Clone)]
pub(crate) struct SearchIndex {
  /// Each package's folded name and folded owner, each followed by
  /// [`FIELD_END`], one package after another in index order.
  names: String,
  /// Each package's lowercased description followed by [`FIELD_END`], one
  /// package after another in index order.
  descriptions: String,
  /// Where each word of a folded name or owner ends, counted from the start of
  /// that field.
  word_ends: Vec<u32>,
  /// Where each package's fields sit in the buffers and `word_ends`.
  bounds: Vec<Bounds>,
  /// Package indices from most to least popular, equal ones in index order.
  most_popular: Vec<u32>,
  /// Package indices from least to most popular, equal ones in index order.
  least_popular: Vec<u32>,
}

/// Where one package's fields start in [`SearchIndex::names`],
/// [`SearchIndex::descriptions`], and [`SearchIndex::word_ends`]. Each text
/// field ends just before the [`FIELD_END`] in front of the next start.
#[derive(Debug, Clone, Copy)]
struct Bounds {
  name: u32,
  owner: u32,
  names_end: u32,
  description: u32,
  description_end: u32,
  name_words: u32,
  owner_words: u32,
  words_end: u32,
  /// Whether the folded name and owner are ASCII, so a character's position
  /// in them is its byte position.
  ascii: bool,
}

impl SearchIndex {
  pub(crate) fn new(index: &PackageIndex) -> Self {
    let mut names = String::new();
    let mut descriptions = String::new();
    let mut word_ends = Vec::new();
    let mut bounds = Vec::with_capacity(index.len());
    let mut popularity = Vec::with_capacity(index.len());

    for idx in 0..index.len() {
      let row = PackageRow { index, idx };
      let name = (offset(names.len()), offset(word_ends.len()));

      fold_into(row.name().unwrap_or_default(), &mut names, &mut word_ends);
      names.push(FIELD_END);

      let owner = (offset(names.len()), offset(word_ends.len()));

      fold_into(row.owner().unwrap_or_default(), &mut names, &mut word_ends);
      names.push(FIELD_END);

      let description = offset(descriptions.len());

      push_lowercase(row.description().unwrap_or_default(), &mut descriptions);
      descriptions.push(FIELD_END);
      bounds.push(Bounds {
        name: name.0,
        owner: owner.0,
        names_end: offset(names.len()),
        description,
        description_end: offset(descriptions.len()),
        name_words: name.1,
        owner_words: owner.1,
        words_end: offset(word_ends.len()),
        ascii: names[name.0 as usize..].is_ascii(),
      });
      popularity.push(
        (1.0 + row.downloads() as f64).ln() + RATING_WEIGHT * (1.0 + f64::from(row.rating())).ln(),
      );
    }

    let mut most_popular: Vec<u32> = (0..offset(index.len())).collect();

    most_popular.sort_by(|&a, &b| popularity[b as usize].total_cmp(&popularity[a as usize]));

    let mut least_popular: Vec<u32> = most_popular.iter().rev().copied().collect();

    least_popular
      .chunk_by_mut(|&a, &b| popularity[a as usize] == popularity[b as usize])
      .for_each(<[u32]>::reverse);
    names.shrink_to_fit();
    descriptions.shrink_to_fit();
    word_ends.shrink_to_fit();

    Self {
      names,
      descriptions,
      word_ends,
      bounds,
      most_popular,
      least_popular,
    }
  }

  /// The package at `idx`, read in place.
  fn entry(&self, idx: usize) -> EntryRef<'_> {
    let bounds = self.bounds[idx];
    let name = |start: u32, next: u32| &self.names[start as usize..next as usize - 1];
    let ends = |start: u32, next: u32| &self.word_ends[start as usize..next as usize];

    EntryRef {
      name: FoldedRef {
        squashed: name(bounds.name, bounds.owner),
        ends: ends(bounds.name_words, bounds.owner_words),
      },
      owner: FoldedRef {
        squashed: name(bounds.owner, bounds.names_end),
        ends: ends(bounds.owner_words, bounds.words_end),
      },
      description: &self.descriptions
        [bounds.description as usize..bounds.description_end as usize - 1],
      ascii: bounds.ascii,
    }
  }

  /// Whether each package has every word of `needle` somewhere in its name,
  /// owner, or description, found by scanning both buffers once per word.
  fn has_every_word(&self, needle: &Needle) -> Vec<bool> {
    let mut found = vec![0_usize; self.bounds.len()];
    let mut words = 0;

    for finder in &needle.finders {
      let in_names = self.packages_containing(&self.names, |bounds| bounds.names_end, finder);
      let in_descriptions =
        self.packages_containing(&self.descriptions, |bounds| bounds.description_end, finder);

      for idx in in_names.chain(in_descriptions) {
        if found[idx] == words {
          found[idx] += 1;
        }
      }

      words += 1;
    }

    found.into_iter().map(|count| count == words).collect()
  }

  /// Whether the package at `idx` has an owner and name as long together as
  /// `needle`, which a pasted `Owner-Name` needs.
  fn could_be_full_name(&self, idx: usize, needle: &Needle) -> bool {
    let bounds = self.bounds[idx];

    (bounds.names_end - bounds.name - 2) as usize == needle.squashed.len()
  }

  /// Whether each package's name or owner holds one of the [`Anchors`] a typo
  /// of `needle` keeps, or `None` when the needle is too short to match by
  /// typo.
  fn could_be_typo(&self, needle: &Needle) -> Option<Vec<bool>> {
    let anchors = needle.typo.as_ref()?;
    let mut patterns = vec![anchors.query.head.as_str(), anchors.query.tail.as_str()];
    let mut could = vec![false; self.bounds.len()];

    match &anchors.first_word {
      FirstWord::Anchors(first) => patterns.extend([first.head.as_str(), first.tail.as_str()]),
      FirstWord::Prefix(word) => patterns.push(word),
    }

    patterns.sort_unstable();
    patterns.dedup();

    for pattern in patterns {
      let finder = Finder::new(pattern);

      for idx in self.packages_containing(&self.names, |bounds| bounds.names_end, &finder) {
        could[idx] = true;
      }
    }

    Some(could)
  }

  /// The index of each package whose part of `text` contains what `finder`
  /// looks for. `end` reads where a package's part of `text` ends.
  fn packages_containing<'s>(
    &'s self,
    text: &'s str,
    end: impl Fn(&Bounds) -> u32 + 's,
    finder: &'s Finder<'s>,
  ) -> impl Iterator<Item = usize> + 's {
    let text = text.as_bytes();
    let mut idx = 0;
    let mut from = 0;

    std::iter::from_fn(move || {
      let at = from + finder.find(&text[from..])?;

      while end(&self.bounds[idx]) as usize <= at {
        idx += 1;
      }

      from = end(&self.bounds[idx]) as usize;

      Some(idx)
    })
  }
}

impl fmt::Debug for SearchIndex {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_list()
      .entries((0..self.bounds.len()).map(|idx| self.entry(idx)))
      .finish()
  }
}

/// A byte position in the search text as stored.
///
/// # Panics
///
/// When the folded text of an index passes 4 GiB.
fn offset(at: usize) -> u32 {
  u32::try_from(at).expect("search text fits in 4 GiB")
}

/// One package's searchable text, borrowed from a [`SearchIndex`].
#[derive(Debug, Clone, Copy)]
struct EntryRef<'a> {
  name: FoldedRef<'a>,
  owner: FoldedRef<'a>,
  description: &'a str,
  /// Whether the name and owner are ASCII.
  ascii: bool,
}

/// Text folded into lowercase words run together, and where each word ends.
#[derive(Clone, Copy)]
struct FoldedRef<'a> {
  squashed: &'a str,
  ends: &'a [u32],
}

impl<'a> FoldedRef<'a> {
  fn words(self) -> Words<'a> {
    Words::new(self.squashed, self.ends)
  }

  /// The text from the start of each word to the end.
  fn word_starts(self) -> impl Iterator<Item = &'a str> {
    let starts = std::iter::once(0).chain(self.ends.iter().map(|&end| end as usize));

    starts
      .take(self.ends.len())
      .map(move |start| &self.squashed[start..])
  }

  /// Whether `word` is one of the words, or all of them run together.
  fn has_word(self, word: &str) -> bool {
    self.squashed == word || self.words().any(|own| own == word)
  }
}

impl fmt::Debug for FoldedRef<'_> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_list().entries(self.words()).finish()
  }
}

/// The words of folded text, cut at their recorded ends.
#[derive(Clone)]
struct Words<'a> {
  squashed: &'a str,
  ends: std::slice::Iter<'a, u32>,
  start: usize,
}

impl<'a> Words<'a> {
  fn new(squashed: &'a str, ends: &'a [u32]) -> Self {
    Self {
      squashed,
      ends: ends.iter(),
      start: 0,
    }
  }
}

impl<'a> Iterator for Words<'a> {
  type Item = &'a str;

  fn next(&mut self) -> Option<&'a str> {
    let end = *self.ends.next()? as usize;
    let word = &self.squashed[self.start..end];

    self.start = end;

    Some(word)
  }
}

impl EntryRef<'_> {
  /// How well this package matches `needle`, given whether it has every
  /// needle word somewhere in its text and whether its name or owner passed
  /// [`SearchIndex::could_be_typo`]. Every tier but
  /// [`ExactFullName`](Tier::ExactFullName) and [`Typo`](Tier::Typo) needs
  /// every word, so a package without them skips straight to those two.
  fn tier(self, needle: &Needle, has_every_word: bool, could_be_typo: bool) -> Option<Tier> {
    let query = needle.squashed.as_str();
    let name = self.name;
    let every_word = |matches: &dyn Fn(&str) -> bool| needle.words().all(matches);

    if !has_every_word {
      let tier = if self.is_full_name(query) {
        Tier::ExactFullName
      } else if could_be_typo && self.is_typo(needle) {
        Tier::Typo
      } else {
        return None;
      };

      return Some(tier);
    }

    let tier = if name.squashed == query {
      Tier::ExactName
    } else if self.is_full_name(query) {
      Tier::ExactFullName
    } else if name.squashed.starts_with(query) {
      Tier::NamePrefix
    } else if every_word(&|word| name.words().any(|own| own.starts_with(word))) {
      Tier::NameWords
    } else if name.squashed.contains(query) {
      Tier::NameSubstring
    } else if self.owner.squashed.starts_with(query) {
      Tier::Owner
    } else if needle.words().zip(&needle.finders).all(|(word, finder)| {
      name.has_word(word) || self.owner.has_word(word) || contains_word(self.description, finder)
    }) {
      Tier::WholeWords
    } else {
      Tier::Substrings
    };

    Some(tier)
  }

  /// Whether `squashed` is this package's owner and name run together, as a
  /// pasted `Owner-Name` folds to.
  fn is_full_name(self, squashed: &str) -> bool {
    squashed.len() == self.owner.squashed.len() + self.name.squashed.len()
      && squashed.starts_with(self.owner.squashed)
      && squashed.ends_with(self.name.squashed)
  }

  /// Whether `needle` is one typo from the start of the name or owner, or
  /// each of its words starts a word of the name or is one typo from it.
  fn is_typo(self, needle: &Needle) -> bool {
    let Some(anchors) = &needle.typo else {
      return false;
    };

    if self.ascii
      && !anchors.query.fit(self.name.squashed)
      && !anchors.query.fit(self.owner.squashed)
      && !self
        .name
        .word_starts()
        .any(|start| anchors.first_word.fits(start))
    {
      return false;
    }

    let query = needle.squashed.as_str();

    is_typo_of_start(query, self.name.squashed)
      || is_typo_of_start(query, self.owner.squashed)
      || needle.words().all(|word| {
        self
          .name
          .words()
          .any(|own| own.starts_with(word) || is_typo_of_start(word, own))
      })
  }
}

/// The packages that `shown` keeps and that match `needle`, best match first
/// in [`Descending`](SortDirection::Descending) order. Equal matches go by
/// popularity, then index order.
pub(super) fn rank(
  index: &PackageIndex,
  needle: &Needle,
  direction: SortDirection,
  shown: impl Fn(usize) -> bool,
) -> Vec<usize> {
  let search = index.search_index();
  let has_every_word = search.has_every_word(needle);
  let could_be_typo = search.could_be_typo(needle);
  let order = match direction {
    SortDirection::Descending => &search.most_popular,
    SortDirection::Ascending => &search.least_popular,
  };
  let mut tiers: [Vec<usize>; Tier::COUNT] = Default::default();

  for idx in order.iter().map(|&idx| idx as usize) {
    let has_every_word = has_every_word[idx];
    let could_be_typo = could_be_typo.as_ref().is_some_and(|could| could[idx]);

    if !has_every_word && !could_be_typo && !search.could_be_full_name(idx, needle) {
      continue;
    }

    if let Some(tier) = search
      .entry(idx)
      .tier(needle, has_every_word, could_be_typo)
      && shown(idx)
    {
      tiers[tier as usize].push(idx);
    }
  }

  match direction {
    SortDirection::Descending => tiers.into_iter().flatten().collect(),
    SortDirection::Ascending => tiers.into_iter().rev().flatten().collect(),
  }
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

  let split = anchor_split(len);

  if let (Some(head), Some(tail)) = (typed.get(..split), typed.get(split + 1..))
    && !text.starts_with(head)
    && !(split..=split + 2).any(|at| text.get(at..at + tail.len()) == Some(tail))
  {
    return false;
  }

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

/// Whether the word `finder` looks for occurs in `text` with no letter or
/// digit on either side.
fn contains_word(text: &str, finder: &Finder) -> bool {
  let len = finder.needle().len();

  finder.find_iter(text.as_bytes()).any(|at| {
    let before = text[..at].chars().next_back();
    let after = text[at + len..].chars().next();

    !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
  })
}

/// Appends `text` to `out` folded into lowercase letters and digits, and the
/// end of each word, counted from where `out` ended, to `ends`. Words break
/// at every character that is not a letter or digit and at camelCase
/// boundaries (`BepInExPack` is `bep`, `in`, `ex`, `pack`; `UIFixes` is `ui`,
/// `fixes`).
fn fold_into(text: &str, out: &mut String, ends: &mut Vec<u32>) {
  let start = out.len();
  let mut chars = text.chars().peekable();
  let mut prev = None;
  let mut in_word = false;

  while let Some(c) = chars.next() {
    let next = chars.peek().copied();
    let camel_break = c.is_uppercase()
      && prev.is_some_and(|prev: char| {
        prev.is_lowercase() || (prev.is_uppercase() && next.is_some_and(char::is_lowercase))
      });

    if (!c.is_alphanumeric() || camel_break) && in_word {
      ends.push(offset(out.len() - start));
      in_word = false;
    }

    if c.is_alphanumeric() {
      out.extend(c.to_lowercase());
      in_word = true;
    }

    prev = Some(c);
  }

  if in_word {
    ends.push(offset(out.len() - start));
  }
}

/// Appends `text` to `out` in lowercase, character by character, as search
/// words are.
fn push_lowercase(text: &str, out: &mut String) {
  if text.is_ascii() {
    let start = out.len();

    out.push_str(text);
    out[start..].make_ascii_lowercase();

    return;
  }

  out.extend(text.chars().flat_map(char::to_lowercase));
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
  fn best_match_keeps_index_order_between_equal_matches_in_either_direction() {
    let index: PackageIndex = vec![
      listed("First-Jotunn", 0, 10, ""),
      listed("Second-Jotunn", 0, 10, ""),
      listed("Popular-Jotunn", 0, 900, ""),
    ]
    .into();
    let ascending = BrowseQuery::new()
      .search("jotunn")
      .sort(SortKey::BestMatch, SortDirection::Ascending);

    assert_eq!(
      best(&index, "jotunn"),
      ["Popular-Jotunn", "First-Jotunn", "Second-Jotunn"]
    );
    assert_eq!(
      browse_names(&index, &ascending),
      ["First-Jotunn", "Second-Jotunn", "Popular-Jotunn"]
    );
  }

  #[test]
  fn best_match_finds_a_full_name_pasted_without_its_separator() {
    let index: PackageIndex = vec![
      listed("Fan-ValheimModdingJotunnAddon", 0, 900, ""),
      listed("ValheimModding-Jotunn", 0, 1, ""),
    ]
    .into();

    assert_eq!(
      best(&index, "valheimmoddingjotunn"),
      ["ValheimModding-Jotunn", "Fan-ValheimModdingJotunnAddon"]
    );
  }

  #[test]
  fn best_match_compares_text_that_grows_when_lowercased() {
    let index: PackageIndex = vec![
      listed("Owner1-Helper", 0, 900, "İİ Mod helper for Ünïcode"),
      listed("Owner2-İstanbulMod", 0, 1, ""),
    ]
    .into();

    assert_eq!(best(&index, "mod"), ["Owner2-İstanbulMod", "Owner1-Helper"]);
    assert_eq!(best(&index, "ünïcode"), ["Owner1-Helper"]);
    assert_eq!(best(&index, "stanbul"), ["Owner2-İstanbulMod"]);
  }

  #[test]
  fn prepare_search_builds_the_search_on_another_thread() {
    let index: PackageIndex = vec![listed("ValheimModding-Jotunn", 0, 1, "")].into();

    std::thread::scope(|scope| {
      scope.spawn(|| index.prepare_search());
    });

    assert_eq!(best(&index, "jotunn"), ["ValheimModding-Jotunn"]);
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
