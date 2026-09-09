use algo_ops::{AlgoOps, ScannedAccount};
use bingle_core::blockchain::algo_bingle::{AccountsCache, AlgoBingle, QueryMode};
use std::sync::{Arc, Mutex};

// Build a cache entry for `address` with no local state (the decode callback under test here only
// looks at the address). The cache stores each opted-in account as a `(address, ScannedAccount)`.
fn cached_account(address: &str) -> (String, ScannedAccount) {
    (
        address.to_string(),
        ScannedAccount {
            address: address.to_string(),
            local_state: Vec::new(),
        },
    )
}

#[test]
pub fn test_cache_only_mode() {
    let cache = Arc::new(Mutex::new(AccountsCache::default()));
    {
        let mut c = cache.lock().unwrap();
        c.entries.push(cached_account("ADDR1"));
    }

    // Placeholder AlgoOps - using dummy address
    let ops = AlgoOps::new_for_algorand(
        None,
        Some("P577PSTDICQ6PQFBR5YMDMJ2YVK7LT5V4GOPNVDLCEDJIL7XGRWC5BRFWA".to_string()),
        None,
    );
    let ab = AlgoBingle::new_with_cache(ops, 123, 0, cache.clone());

    let mut count = 0;
    ab.indexer_query_opted_in_accounts_sync(123, QueryMode::CacheOnly, None, |acct| {
        count += 1;
        assert_eq!(acct.address, "ADDR1");
        Ok(())
    })
    .unwrap();

    assert_eq!(count, 1, "Should have returned 1 account from cache");
}

#[test]
pub fn test_cache_lifetime_fallback() {
    let cache = Arc::new(Mutex::new(AccountsCache::default()));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    {
        let mut c = cache.lock().unwrap();
        c.entries.push(cached_account("ADDR1"));
        c.last_updated = now - 30; // updated 30s ago
        c.last_round = 100;
    }

    let ops = AlgoOps::new_for_algorand(
        None,
        Some("P577PSTDICQ6PQFBR5YMDMJ2YVK7LT5V4GOPNVDLCEDJIL7XGRWC5BRFWA".to_string()),
        None,
    );
    let ab = AlgoBingle::new_with_cache(ops, 123, 0, cache.clone());

    // Lifetime is 60s, so 30s ago is "fresh"
    let mut count = 0;
    ab.indexer_query_opted_in_accounts_sync(123, QueryMode::Refresh, Some(60), |_| {
        count += 1;
        Ok(())
    })
    .unwrap();

    assert_eq!(
        count, 1,
        "Should have used cache instead of hitting network (which would fail anyway without real indexer)"
    );
}
