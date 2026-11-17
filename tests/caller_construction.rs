//! The `#[non_exhaustive]` configuration and model types can still be built
//! and read from outside the crate.

use std::time::Duration;
use thunderstore_engine::RetryPolicy;
use thunderstore_engine::extract::ExtractLimits;
use thunderstore_engine::manifest::Manifest;

#[test]
fn extract_limits_start_from_default_and_take_new_values() {
  let mut limits = ExtractLimits::default();
  limits.max_entries = 10;
  limits.max_file_bytes = 1024;
  limits.max_total_bytes = 4096;

  assert_eq!(
    (
      limits.max_entries,
      limits.max_file_bytes,
      limits.max_total_bytes
    ),
    (10, 1024, 4096)
  );
}

#[test]
fn a_retry_policy_starts_from_default_or_none_and_takes_new_values() {
  let mut policy = RetryPolicy::default();
  policy.max_retries = 5;
  policy.base_delay = Duration::from_millis(10);
  policy.max_delay = Duration::from_secs(1);

  assert_eq!(policy.max_retries, 5);
  assert_eq!(RetryPolicy::none().max_retries, 0);
}

#[test]
fn a_manifest_parses_from_json() {
  let manifest: Manifest = serde_json::from_str(
    r#"{"name": "ModA", "version_number": "1.0.0", "dependencies": ["Owner-ModB-2.0.0"]}"#,
  )
  .unwrap();

  assert_eq!(manifest.version_number, "1.0.0");
  assert_eq!(manifest.dependencies, ["Owner-ModB-2.0.0"]);
}
