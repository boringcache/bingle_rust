use algo_ops::AlgoChainConfig;
use bingle_core::relay::discovery::indexer_discover_closure;

/// Regression: the discovery closure used to `panic!` when the indexer query failed,
/// which unwound and killed the pending-message drain thread (no sends ever again after
/// an indexer outage). It must instead return an empty list so the caller retries.
#[test]
#[cfg(not(target_os = "ios"))]
fn discover_closure_returns_empty_not_panic_on_indexer_error() {
    // Point the indexer at a dead local port so the query fails fast (connection refused).
    let cfg = AlgoChainConfig {
        client_api_url: "http://127.0.0.1".to_string(),
        client_api_port: 1,
        indexer_api_url: "http://127.0.0.1".to_string(),
        indexer_api_port: 1,
        token: None,
        token_key: None,
        app_id: Some(1),
        asset_id: None,
        ..Default::default()
    };

    let discover = indexer_discover_closure(1, Some(cfg), None);
    // Must not panic; an unreachable indexer yields an empty relay list.
    let relays = discover();
    assert!(
        relays.is_empty(),
        "unreachable indexer should yield an empty relay list, got {:?}",
        relays
    );
}
