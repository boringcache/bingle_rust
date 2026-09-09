use algo_ops::AlgoChainConfig;
use bingle_core::blockchain::algo_bingle::{AccountsCache, AlgoBingle, QueryMode};
use serial_test::serial;
use std::sync::{Arc, Mutex};

use crate::setup_localnet;
use crate::util::test_util;
use crate::util::test_util::init_test_logging;
use test_util::{ADDRESS_SPEND, PASSPHRASE_SPEND, localnet_config, ops_from_mnemonic};

#[test]
#[serial]
pub fn test_indexer_cache_force_full_and_cache_only() {
    unsafe {
        std::env::set_var("BINGLE_ALGO_DEBUG", "true");
    }
    init_test_logging();

    test_util::assert_localnet_available();
    let cfg: AlgoChainConfig = localnet_config();
    setup_localnet::ensure_localnet_accounts_funded(&cfg, &[ADDRESS_SPEND])
        .expect("localnet funded");

    let ops = ops_from_mnemonic(ADDRESS_SPEND, PASSPHRASE_SPEND, cfg.clone());
    let (app_id, asset_id) = test_util::deploy_bingle_app_and_asset(&ops, "BINGLE", 1000);

    // Opt-in sender to app so there's at least one account
    ops.opt_in_app(app_id).expect("opt-in app");

    let cache = Arc::new(Mutex::new(AccountsCache::default()));
    let ab = AlgoBingle::new_with_cache(ops.clone(), app_id, asset_id, cache.clone());

    // 1. ForceFull should populate cache (with retries for Indexer lag)
    let mut count = 0;
    for _ in 0..15 {
        count = 0;
        ab.indexer_query_opted_in_accounts_sync(app_id, QueryMode::ForceFull, None, |_| {
            count += 1;
            Ok(())
        })
        .expect("ForceFull success");
        if count >= 1 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1000));
    }

    assert!(count >= 1, "Should have found at least the sender account");
    {
        let c = cache.lock().unwrap();
        assert_eq!(
            c.entries.len(),
            count as usize,
            "Cache should match found account count"
        );
        assert!(c.last_round > 0, "last_round should be set");
        assert!(
            c.entries.iter().any(|(addr, _)| addr == ADDRESS_SPEND),
            "Cache should contain the opted-in account"
        );
    }

    let last_round_first = { cache.lock().unwrap().last_round };

    // 2. CacheOnly should use cache and NOT hit indexer (implicitly verified by it working without errors or by checking it returns same count)
    let mut count2 = 0;
    ab.indexer_query_opted_in_accounts_sync(app_id, QueryMode::CacheOnly, None, |_| {
        count2 += 1;
        Ok(())
    })
    .expect("CacheOnly success");

    assert_eq!(
        count2, count,
        "CacheOnly should return same number of accounts as ForceFull"
    );
    assert_eq!(
        cache.lock().unwrap().last_round,
        last_round_first,
        "CacheOnly should not change last_round"
    );
}

#[test]
#[serial]
pub fn test_indexer_cache_refresh_incremental() {
    unsafe {
        std::env::set_var("BINGLE_ALGO_DEBUG", "true");
    }
    init_test_logging();

    test_util::assert_localnet_available();
    let cfg: AlgoChainConfig = localnet_config();
    setup_localnet::ensure_localnet_accounts_funded(&cfg, &[ADDRESS_SPEND])
        .expect("localnet funded");

    let ops = ops_from_mnemonic(ADDRESS_SPEND, PASSPHRASE_SPEND, cfg.clone());
    let (app_id, asset_id) = test_util::deploy_bingle_app_and_asset(&ops, "BINGLE_REF", 1000);

    let cache = Arc::new(Mutex::new(AccountsCache::default()));
    let ab = AlgoBingle::new_with_cache(ops.clone(), app_id, asset_id, cache.clone());

    // 1. Initial Refresh should do a full scan (because last_round is 0)
    let mut count = 0;
    ab.indexer_query_opted_in_accounts_sync(app_id, QueryMode::Refresh, None, |_| {
        count += 1;
        Ok(())
    })
    .expect("Refresh success");
    assert_eq!(count, 0, "Initially no accounts opted in");

    let round_after_initial = { cache.lock().unwrap().last_round };
    assert!(round_after_initial > 0);

    // 2. Opt-in an account
    ops.opt_in_app(app_id).expect("opt-in app");

    // Wait for indexer to catch up with the transaction
    std::thread::sleep(std::time::Duration::from_millis(2000));

    // 3. Refresh should find the new account incrementally
    let mut count2 = 0;
    for _ in 0..15 {
        count2 = 0;
        ab.indexer_query_opted_in_accounts_sync(app_id, QueryMode::Refresh, None, |_| {
            count2 += 1;
            Ok(())
        })
        .expect("Refresh success");
        if count2 == 1 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1000));
    }

    assert_eq!(count2, 1, "Should have found 1 account after opt-in");
    {
        let c = cache.lock().unwrap();
        assert!(
            c.last_round > round_after_initial,
            "last_round should have advanced"
        );
        assert!(
            c.entries.iter().any(|(addr, _)| addr == ADDRESS_SPEND),
            "Cache should contain the newly opted-in account"
        );
    }

    // 4. Opt-out (Clear State)
    ops.clear_state_app(app_id).expect("clear state app");

    // Wait for indexer
    std::thread::sleep(std::time::Duration::from_millis(2000));

    // 5. Refresh should remove the account incrementally
    let mut count3 = 0;
    for _ in 0..15 {
        count3 = 0;
        ab.indexer_query_opted_in_accounts_sync(app_id, QueryMode::Refresh, None, |_| {
            count3 += 1;
            Ok(())
        })
        .expect("Refresh success");
        if count3 == 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1000));
    }

    assert_eq!(count3, 0, "Should have 0 accounts after opt-out");
    assert_eq!(
        cache.lock().unwrap().entries.len(),
        0,
        "Cache should be empty"
    );
}

#[test]
#[serial]
pub fn test_indexer_cache_clear_and_lifetime() {
    use bingle_core::api::bingle_api::{BingleApi, StartOptions};
    use bingle_core::api::bingle_api_impl::BingleApiImpl;

    init_test_logging();
    test_util::assert_localnet_available();
    let cfg: AlgoChainConfig = localnet_config();
    setup_localnet::ensure_localnet_accounts_funded(&cfg, &[ADDRESS_SPEND])
        .expect("localnet funded");

    let ops = ops_from_mnemonic(ADDRESS_SPEND, PASSPHRASE_SPEND, cfg.clone());
    let (app_id, asset_id) = test_util::deploy_bingle_app_and_asset(&ops, "BINGLE_CLR", 1000);
    ops.opt_in_app(app_id).expect("opt-in app");

    let mut options = StartOptions::new("test".to_string());
    options.app_id = Some(app_id);
    options.asset_id = Some(asset_id);
    options.algo_provider_config = Some(cfg);

    let api = BingleApiImpl::new(&options);
    let cache = api.get_accounts_cache().expect("cache should be available");

    // 1. Initial populate
    let ab = AlgoBingle::new_with_cache(ops.clone(), app_id, asset_id, cache.clone());

    let mut count = 0;
    for _ in 0..15 {
        count = 0;
        ab.indexer_query_opted_in_accounts_sync(app_id, QueryMode::ForceFull, None, |_| {
            count += 1;
            Ok(())
        })
        .expect("ForceFull success");
        if count >= 1 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1000));
    }
    assert!(count >= 1);

    let last_updated = { cache.lock().unwrap().last_updated };
    assert!(last_updated > 0);

    // 2. Test lifetime - should NOT hit network (verified by logs if running with --nocapture, but here we just check it returns same)
    let mut count2 = 0;
    ab.indexer_query_opted_in_accounts_sync(app_id, QueryMode::Refresh, Some(60), |_| {
        count2 += 1;
        Ok(())
    })
    .expect("Refresh with lifetime success");
    assert_eq!(count2, count);
    assert_eq!(
        cache.lock().unwrap().last_updated,
        last_updated,
        "last_updated should NOT have changed"
    );

    // 3. Test clear_accounts_cache
    api.clear_accounts_cache();
    {
        let c = cache.lock().unwrap();
        assert_eq!(c.entries.len(), 0);
        assert_eq!(c.last_round, 0);
        assert_eq!(c.last_updated, 0);
    }

    // 4. Refresh after clear should do full scan
    let mut count3 = 0;
    ab.indexer_query_opted_in_accounts_sync(app_id, QueryMode::Refresh, None, |_| {
        count3 += 1;
        Ok(())
    })
    .expect("Refresh after clear success");
    assert!(count3 >= 1);
    let final_last_updated = cache.lock().unwrap().last_updated;
    assert!(final_last_updated >= last_updated);
    assert!(final_last_updated > 0);
}
