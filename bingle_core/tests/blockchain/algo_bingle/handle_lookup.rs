// tests/blockchain/algo_bingle/handle_lookup.rs
use algo_ops::ScannedAccount;
use bingle_core::blockchain::algo_bingle::AlgoBingle;

// Build an opted-in account with its (already-decoded) local state for the scanned app. algo_ops
// scopes the scan to one app, so `local_state` here is the key/values for that single app —
// exactly what `extract_handle_match` / `handle_prefix_match` now receive.
fn account(address: &str, local_state: &[(&str, &str)]) -> ScannedAccount {
    ScannedAccount {
        address: address.to_string(),
        local_state: local_state
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    }
}

#[test]
#[cfg(not(target_os = "ios"))]
pub fn test_extract_handle_match_found() {
    let handle = "alice";
    let acct = account("ADDR1", &[("Handle", "alice"), ("HandleTime", "1000")]);

    let mut matches = Vec::new();
    AlgoBingle::extract_handle_match(&acct, handle, &mut matches);

    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].0, "ADDR1");
    assert_eq!(matches[0].1, 1000);
}

#[test]
#[cfg(not(target_os = "ios"))]
pub fn test_extract_handle_match_wrong_handle() {
    let handle = "bob";
    let acct = account("ADDR1", &[("Handle", "alice"), ("HandleTime", "1000")]);

    let mut matches = Vec::new();
    AlgoBingle::extract_handle_match(&acct, handle, &mut matches);

    assert!(matches.is_empty());
}

#[test]
#[cfg(not(target_os = "ios"))]
pub fn test_pick_oldest_match() {
    let matches = vec![
        ("ADDR2".to_string(), 2000),
        ("ADDR1".to_string(), 1000),
        ("ADDR3".to_string(), 3000),
    ];

    let result = AlgoBingle::pick_oldest_match(matches);
    assert_eq!(result, Some("ADDR1".to_string()));
}

#[test]
#[cfg(not(target_os = "ios"))]
pub fn test_extract_handle_match_scoped_local_state() {
    // The scan is app-scoped upstream in algo_ops, so the callback only sees the target app's local
    // state. A handle in that scoped state matches; state for other apps never reaches this function.
    let handle = "alice";
    let acct = account("ADDR1", &[("Handle", "alice"), ("HandleTime", "1000")]);

    let mut matches = Vec::new();
    AlgoBingle::extract_handle_match(&acct, handle, &mut matches);

    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].0, "ADDR1");
}

#[test]
#[cfg(not(target_os = "ios"))]
pub fn test_normalize_handle_lowercase() {
    assert_eq!(AlgoBingle::normalize_handle("Fred123"), "fred123");
}

#[test]
#[cfg(not(target_os = "ios"))]
pub fn test_normalize_handle_dots() {
    assert_eq!(AlgoBingle::normalize_handle("James.Jones"), "jamesjones");
}

#[test]
#[cfg(not(target_os = "ios"))]
pub fn test_normalize_handle_dashes() {
    assert_eq!(AlgoBingle::normalize_handle("james-jones"), "jamesjones");
}

#[test]
#[cfg(not(target_os = "ios"))]
pub fn test_normalize_handle_special_chars() {
    assert_eq!(AlgoBingle::normalize_handle("#user$100"), "user100");
}

#[test]
#[cfg(not(target_os = "ios"))]
pub fn test_extract_handle_match_case_insensitive() {
    // Stored as "Alice" (registered form), looked up as "alice" (normalised)
    let handle = "alice";
    let acct = account("ADDR1", &[("Handle", "Alice"), ("HandleTime", "1000")]);
    let mut matches = Vec::new();
    AlgoBingle::extract_handle_match(&acct, handle, &mut matches);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].0, "ADDR1");
}

#[test]
#[cfg(not(target_os = "ios"))]
pub fn test_extract_handle_match_with_dots_in_stored() {
    // Stored as "james.jones", looked up as "jamesjones"
    let handle = "jamesjones";
    let acct = account(
        "ADDR1",
        &[("Handle", "james.jones"), ("HandleTime", "1000")],
    );
    let mut matches = Vec::new();
    AlgoBingle::extract_handle_match(&acct, handle, &mut matches);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].0, "ADDR1");
}

#[test]
#[cfg(not(target_os = "ios"))]
pub fn test_handle_prefix_match_found() {
    // Prefix "al" matches stored "Alice"; canonical handle preserved as written.
    let acct = account("ADDR1", &[("Handle", "Alice"), ("HandleTime", "1000")]);

    let mut matches = Vec::new();
    AlgoBingle::handle_prefix_match(&acct, "al", &mut matches);

    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].0, "ADDR1");
    assert_eq!(matches[0].1, "Alice"); // canonical handle as written
    assert_eq!(matches[0].2, 1000);
}

#[test]
#[cfg(not(target_os = "ios"))]
pub fn test_handle_prefix_match_normalised() {
    // Input "abc" should match stored "ab_cd" (normalisation strips the underscore),
    // and the canonical handle "ab_cd" is returned as written.
    let acct = account("ADDR1", &[("Handle", "ab_cd"), ("HandleTime", "5")]);

    let mut matches = Vec::new();
    AlgoBingle::handle_prefix_match(&acct, "abc", &mut matches);

    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].1, "ab_cd");
}

#[test]
#[cfg(not(target_os = "ios"))]
pub fn test_handle_prefix_match_no_match() {
    let acct = account("ADDR1", &[("Handle", "Alice"), ("HandleTime", "1000")]);

    let mut matches = Vec::new();
    AlgoBingle::handle_prefix_match(&acct, "bob", &mut matches);
    assert!(matches.is_empty());
}

#[test]
#[cfg(not(target_os = "ios"))]
pub fn test_handle_prefix_match_empty_prefix_never_matches() {
    let acct = account("ADDR1", &[("Handle", "Alice"), ("HandleTime", "1000")]);

    let mut matches = Vec::new();
    // Empty and punctuation-only inputs normalise to empty and must not match everything.
    AlgoBingle::handle_prefix_match(&acct, "", &mut matches);
    AlgoBingle::handle_prefix_match(&acct, "!!", &mut matches);
    assert!(matches.is_empty());
}

#[test]
#[cfg(not(target_os = "ios"))]
pub fn test_pick_oldest_prefix_match() {
    let matches = vec![
        ("ADDR2".to_string(), "Bob_smith".to_string(), 2000),
        ("ADDR1".to_string(), "Bob_jones".to_string(), 1000),
        ("ADDR3".to_string(), "Bobby".to_string(), 3000),
    ];

    let result = AlgoBingle::pick_oldest_prefix_match(matches);
    assert_eq!(result, Some(("ADDR1".to_string(), "Bob_jones".to_string())));
}

#[test]
#[cfg(not(target_os = "ios"))]
pub fn test_pick_oldest_prefix_match_empty() {
    assert_eq!(AlgoBingle::pick_oldest_prefix_match(Vec::new()), None);
}

#[test]
#[cfg(not(target_os = "ios"))]
pub fn test_pick_oldest_match_collision() {
    // Two accounts register the same handle in the same block (same timestamp)
    let matches = vec![("ADDR2".to_string(), 1000), ("ADDR1".to_string(), 1000)];

    let result = AlgoBingle::pick_oldest_match(matches.clone());

    // Now pick_oldest_match tie-breaks by address if timestamps are equal.
    // "ADDR1" < "ADDR2", so "ADDR1" should be picked regardless of input order.
    assert_eq!(result, Some("ADDR1".to_string()));

    // If the order was different:
    let matches_rev = vec![("ADDR1".to_string(), 1000), ("ADDR2".to_string(), 1000)];
    let result_rev = AlgoBingle::pick_oldest_match(matches_rev);
    assert_eq!(result_rev, Some("ADDR1".to_string()));
}
