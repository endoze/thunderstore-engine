#![deny(missing_docs)]

use lasso::{Key, Rodeo, Spur};

/// A string interner that stores unique strings and returns small integer keys
/// to retrieve them later, reducing memory usage for repeated strings.
///
/// This is a thin newtype over the backing `lasso` interner so that the
/// dependency stays an implementation detail and a future `lasso` major version
/// is not a breaking change for users of this crate.
#[derive(Debug, Clone, Default)]
pub struct StringInterner(Rodeo);

/// A compact integer identifier referencing a string stored in a
/// [`StringInterner`].
///
/// Newtyped over the backing `lasso` key for the same stability reason as
/// [`StringInterner`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InternKey(Spur);

impl StringInterner {
  /// Interns `s`, returning its key (existing key if already interned).
  pub fn get_or_intern(&mut self, s: &str) -> InternKey {
    InternKey(self.0.get_or_intern(s))
  }

  /// Resolves a key back to its string.
  ///
  /// # Panics
  ///
  /// Panics if `key` did not come from this interner. Keys are not namespaced by
  /// interner, so mixing keys between two interners is a caller bug rather than a
  /// recoverable condition.
  pub fn resolve(&self, key: &InternKey) -> &str {
    self.0.resolve(&key.0)
  }

  /// Iterates over the interned strings in insertion order.
  pub fn strings(&self) -> impl Iterator<Item = &str> {
    self.0.strings()
  }

  /// Returns the number of unique strings stored in the interner.
  pub fn len(&self) -> usize {
    self.0.len()
  }
}

impl InternKey {
  /// The key's stable integer index, used when serializing to a string table.
  pub(crate) fn to_index(self) -> usize {
    self.0.into_usize()
  }

  /// Reconstructs a key from a stable integer index, or `None` if the index is
  /// not representable as a key.
  pub(crate) fn from_index(index: usize) -> Option<Self> {
    Spur::try_from_usize(index).map(InternKey)
  }
}

/// Interns an optional string, preserving absence: `None` in, `None` out.
///
/// The package model stores most fields as optional interned keys, so this keeps
/// "field not present" distinct from "field present and empty".
pub fn intern_option(interner: &mut StringInterner, s: Option<&str>) -> Option<InternKey> {
  s.map(|val| interner.get_or_intern(val))
}

/// The inverse of [`intern_option`], allocating an owned `String` per resolved
/// key.
///
/// # Panics
///
/// Panics if `key` is `Some` and did not come from `interner`, per
/// [`StringInterner::resolve`].
pub fn resolve_option(interner: &StringInterner, key: Option<InternKey>) -> Option<String> {
  key.map(|k| interner.resolve(&k).to_string())
}

/// Interns each string in order, so the returned keys line up index-for-index
/// with `strings`.
///
/// Duplicates collapse to the same key, which is the point: a dependency list
/// repeated across thousands of packages costs one copy of each string.
pub fn intern_vec(interner: &mut StringInterner, strings: &[String]) -> Vec<InternKey> {
  strings
    .iter()
    .map(|s| interner.get_or_intern(s.as_str()))
    .collect()
}

/// The inverse of [`intern_vec`], allocating one owned `String` per key.
///
/// Deduplication is not preserved on the way out: two keys pointing at the same
/// interned string yield two separate allocations.
///
/// # Panics
///
/// Panics if any key did not come from `interner`, per
/// [`StringInterner::resolve`].
pub fn resolve_vec(interner: &StringInterner, keys: &[InternKey]) -> Vec<String> {
  keys
    .iter()
    .map(|k| interner.resolve(k).to_string())
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_intern_option_some() {
    let mut interner = StringInterner::default();
    let key = intern_option(&mut interner, Some("test"));
    assert!(key.is_some());
    assert_eq!(resolve_option(&interner, key), Some("test".to_string()));
  }

  #[test]
  fn test_intern_option_none() {
    let mut interner = StringInterner::default();
    let key = intern_option(&mut interner, None);
    assert!(key.is_none());
    assert_eq!(resolve_option(&interner, key), None);
  }

  #[test]
  fn test_intern_vec() {
    let mut interner = StringInterner::default();
    let strings = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    let keys = intern_vec(&mut interner, &strings);
    assert_eq!(keys.len(), 3);
    assert_eq!(resolve_vec(&interner, &keys), strings);
  }

  #[test]
  fn test_intern_vec_empty() {
    let mut interner = StringInterner::default();
    let strings: Vec<String> = vec![];
    let keys = intern_vec(&mut interner, &strings);
    assert!(keys.is_empty());
    assert!(resolve_vec(&interner, &keys).is_empty());
  }

  #[test]
  fn test_deduplication() {
    let mut interner = StringInterner::default();
    let key1 = interner.get_or_intern("duplicate");
    let key2 = interner.get_or_intern("duplicate");
    assert_eq!(key1, key2);
    assert_eq!(interner.resolve(&key1), "duplicate");
  }

  #[test]
  fn test_multiple_strings() {
    let mut interner = StringInterner::default();
    let key_a = interner.get_or_intern("a");
    let key_b = interner.get_or_intern("b");
    let key_a2 = interner.get_or_intern("a");

    assert_eq!(key_a, key_a2);
    assert_ne!(key_a, key_b);
    assert_eq!(interner.resolve(&key_b), "b");
  }
}
