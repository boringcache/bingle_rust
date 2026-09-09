//! Store-and-forward end-to-end against a deployed Sidewinder cluster on **TestNet** (epic #200,
//! story #226). The same path as the LocalNet e2e — two `bingle_local` instances: offline send →
//! give-up post → reconnect poll → read/decrypt — asserting the epic invariants (no double-post, no
//! re-read, sorted by sent time), but pointed at an out-of-band cluster via configuration rather than
//! standing a node up.
//!
//! Gated behind the `sidewinder_testnet` cargo feature (off by default, never in the per-PR gate).
//! When the feature is enabled it **fails, not skips** if the configuration is incomplete.
//!
//! ## Configuration (environment; file fallbacks ease local runs)
//!
//! | Variable | Meaning | Fallback |
//! |---|---|---|
//! | `SIDEWINDER_TESTNET_URL` | cluster client REST base URL, e.g. `http://[<ipv6>]:1080` | — (required) |
//! | `SIDEWINDER_TESTNET_TOKEN` | bearer token | file at `SIDEWINDER_TESTNET_TOKEN_FILE` (default `tmp/testnet_token.txt`) |
//! | `SIDEWINDER_TESTNET_NODE_FILE` | Bingle node file (TestNet algod + app/asset ids) | — (required) |
//! | `SIDEWINDER_TESTNET_SENDER_MNEMONIC` | the sender account's 25-word mnemonic | file `tmp/tn_sender.mnemonic` |
//! | `SIDEWINDER_TESTNET_RECEIVER_MNEMONIC` | the receiver account's 25-word mnemonic | file `tmp/tn_receiver.mnemonic` |
//!
//! ## Precondition (one-time provisioning, see #226)
//! The sender and receiver accounts must be **enrolled as Mailbox callers** in the deployed cluster's
//! allowlist and **funded** on TestNet. Their handles are registered on the TestNet BingleDapp by this
//! test at runtime (idempotent — skipped if already registered).
//!
//! ## Run
//!   `cargo test -p bingle_cli --features sidewinder_testnet --test integration -- --test-threads 1 --nocapture`

use algo_ops::{AlgoChainConfig, AlgoOps};
use bingle_core::blockchain::algo_bingle::AlgoBingle;
use bingle_core::util::config_utils::parse_node_file_with_ids;
use bingle_local::api::sidewinder::MailboxConfig;
use bingle_local::api::{BingleApiLocalImpl, BingleLocalApi, LocalApiConfig};
use bingle_test::localnet::test_util;
use std::path::{Path, PathBuf};
use std::time::Duration;

const SENDER_HANDLE: &str = "tn-sf-sender";
const RECEIVER_HANDLE: &str = "tn-sf-receiver";

/// TestNet anchors to the parent chain on a batch cadence (the `v0_0_3` profile, K=64 rounds ≈
/// minutes), far slower than LocalNet — so wait generously for each transaction to finalise.
const TESTNET_FINALITY: Duration = Duration::from_secs(300);

/// The cluster Mailbox connection for this run, with the TestNet finality timeout applied.
/// `SIDEWINDER_TESTNET_FINALITY_SECS` overrides the wait (handy for a quick stage probe).
fn mailbox_config(cfg: &TestnetConfig) -> MailboxConfig {
    let mut mc = MailboxConfig::new(cfg.api_url.clone(), cfg.token.clone());
    mc.finality_timeout = std::env::var("SIDEWINDER_TESTNET_FINALITY_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(TESTNET_FINALITY);
    mc
}

/// The workspace root (`bingle_cli`'s parent). `cargo test` runs with the crate dir as the working
/// directory, so the `tmp/…` fallbacks and any relative node-file path are resolved against this.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root (bingle_cli parent)")
        .to_path_buf()
}

/// Resolve a possibly-relative path against the workspace root; leave absolute paths as given.
fn resolve_repo_path(p: &str) -> String {
    let path = Path::new(p);
    if path.is_absolute() {
        p.to_string()
    } else {
        repo_root().join(path).to_string_lossy().into_owned()
    }
}

/// The resolved TestNet configuration.
struct TestnetConfig {
    api_url: String,
    token: String,
    /// TestNet algod config with the BingleDapp app/asset ids applied.
    algo_config: AlgoChainConfig,
    app_id: u64,
    asset_id: u64,
    sender_mnemonic: String,
    receiver_mnemonic: String,
}

/// Read `key` from the environment, or fall back to the contents of `file` (trimmed) if it exists.
fn env_or_file(key: &str, file: &str) -> Option<String> {
    if let Ok(v) = std::env::var(key) {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return Some(v);
        }
    }
    std::fs::read_to_string(file)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn env_required(key: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| panic!("{key} must be set to run the TestNet store-and-forward e2e"))
}

fn load_config() -> TestnetConfig {
    let api_url = env_required("SIDEWINDER_TESTNET_URL");
    let token_file = std::env::var("SIDEWINDER_TESTNET_TOKEN_FILE")
        .map(|p| resolve_repo_path(&p))
        .unwrap_or_else(|_| {
            repo_root()
                .join("tmp/testnet_token.txt")
                .to_string_lossy()
                .into_owned()
        });
    let token = env_or_file("SIDEWINDER_TESTNET_TOKEN", &token_file)
        .unwrap_or_else(|| panic!("set SIDEWINDER_TESTNET_TOKEN or provide {token_file}"));

    let node_file = resolve_repo_path(&env_required("SIDEWINDER_TESTNET_NODE_FILE"));
    let (_network, mut algo_config, app_id, asset_id) = parse_node_file_with_ids(&node_file)
        .unwrap_or_else(|e| panic!("bad SIDEWINDER_TESTNET_NODE_FILE '{node_file}': {e}"));
    let app_id = app_id.expect("the TestNet node file must carry an app_id");
    let asset_id = asset_id.expect("the TestNet node file must carry an asset_id");
    algo_config.app_id = Some(app_id);
    algo_config.asset_id = Some(asset_id);

    let sender_default = repo_root().join("tmp/tn_sender.mnemonic");
    let sender_mnemonic = env_or_file(
        "SIDEWINDER_TESTNET_SENDER_MNEMONIC",
        &sender_default.to_string_lossy(),
    )
    .expect("set SIDEWINDER_TESTNET_SENDER_MNEMONIC or provide tmp/tn_sender.mnemonic");
    let receiver_default = repo_root().join("tmp/tn_receiver.mnemonic");
    let receiver_mnemonic = env_or_file(
        "SIDEWINDER_TESTNET_RECEIVER_MNEMONIC",
        &receiver_default.to_string_lossy(),
    )
    .expect("set SIDEWINDER_TESTNET_RECEIVER_MNEMONIC or provide tmp/tn_receiver.mnemonic");

    TestnetConfig {
        api_url,
        token,
        algo_config,
        app_id,
        asset_id,
        sender_mnemonic,
        receiver_mnemonic,
    }
}

/// Register `handle` for the account on the BingleDapp if it is not already registered (idempotent so
/// the test can be re-run against the same accounts).
fn ensure_registered(cfg: &TestnetConfig, address: &str, mnemonic: &str, handle: &str) {
    let ops = test_util::ops_from_mnemonic(address, mnemonic, cfg.algo_config.clone());
    let ab = AlgoBingle::new(ops.clone(), cfg.app_id, cfg.asset_id);
    if let Ok(Some(existing)) = ab.handle_for_address(cfg.app_id, address) {
        if existing == handle {
            return; // already registered with this handle
        }
    }
    test_util::register_client_on_blockchain(
        address,
        mnemonic,
        handle,
        cfg.app_id,
        cfg.asset_id,
        &ops,
        cfg.algo_config.clone(),
    );
}

/// Build a bingle_local config for TestNet + this cluster with the `send`/`receive` gates as given.
fn local_config(cfg: &TestnetConfig, send: bool, receive: bool) -> LocalApiConfig {
    LocalApiConfig {
        algo_config: cfg.algo_config.clone(),
        app_id: cfg.app_id,
        asset_id: cfg.asset_id,
        sidewinder: Some(mailbox_config(cfg)),
        store_and_forward_send: send,
        store_and_forward_receive: receive,
        ..LocalApiConfig::default()
    }
}

/// Drive a failed direct delivery for message `timestamp` twice, as the retry loop would; the second
/// failure must not double-post (idempotency).
fn fail_delivery_twice(sender: &mut BingleApiLocalImpl, timestamp: i64) {
    let reason = || Some("Recipient unreachable — will keep retrying".to_string());
    sender
        .update_message_status(timestamp, 0.5, reason(), None)
        .expect("first failure");
    sender
        .update_message_status(timestamp, 0.5, reason(), None)
        .expect("second failure (retry) — must not double-post");
}

/// Drain the receiver's Mailbox before the run so the assertions count only what this test posts, and
/// so a re-run starts clean. Returns without error if the Mailbox is already empty.
fn drain_receiver(cfg: &TestnetConfig) {
    use bingle_local::api::sidewinder::Mailbox;
    let mailbox = Mailbox::new(
        AlgoOps::new_for_algorand(Some(cfg.receiver_mnemonic.clone()), None, None),
        mailbox_config(cfg),
    )
    .expect("receiver mailbox");
    while mailbox.pop().expect("drain pop").is_some() {}
}

/// The full store-and-forward path against a deployed TestNet cluster.
#[serial_test::serial(sidewinder_e2e)]
#[test]
fn offline_send_survives_and_is_read_on_reconnect_on_testnet() {
    // Surface the submitted transaction ids (info) and their stage transitions (debug) with
    // `--nocapture`; RUST_LOG overrides. Handy for tracking a txn on the node while it finalises.
    test_util::init_test_logging_with_filter("info,bingle_local::api::sidewinder=debug");
    let cfg = load_config();

    // One-time-ish: make sure both accounts have their handles on-chain, then start from an empty box.
    ensure_registered(
        &cfg,
        &address_of(&cfg.sender_mnemonic),
        &cfg.sender_mnemonic,
        SENDER_HANDLE,
    );
    ensure_registered(
        &cfg,
        &address_of(&cfg.receiver_mnemonic),
        &cfg.receiver_mnemonic,
        RECEIVER_HANDLE,
    );
    drain_receiver(&cfg);

    // Sender: two offline sends give up and are posted to the receiver's Mailbox. The earlier-sent
    // message is added second and posted first, so a correct read must reorder by sent time.
    let mut sender = BingleApiLocalImpl::new(local_config(&cfg, true, false));
    sender
        .import_keypair(cfg.sender_mnemonic.clone())
        .expect("import sender keypair");
    let earlier_ts = 1_726_600_000_000;
    let later_ts = 1_726_600_005_000;
    sender
        .add_message(
            SENDER_HANDLE.to_string(),
            vec![RECEIVER_HANDLE.to_string()],
            earlier_ts,
            "first, by sent time".to_string(),
            None,
        )
        .expect("add earlier message");
    sender
        .add_message(
            SENDER_HANDLE.to_string(),
            vec![RECEIVER_HANDLE.to_string()],
            later_ts,
            "second, by sent time".to_string(),
            None,
        )
        .expect("add later message");
    fail_delivery_twice(&mut sender, later_ts);
    fail_delivery_twice(&mut sender, earlier_ts);

    // Receiver: reconnect, poll, read, decrypt.
    let mut receiver = BingleApiLocalImpl::new(local_config(&cfg, false, true));
    receiver
        .import_keypair(cfg.receiver_mnemonic.clone())
        .expect("import receiver keypair");
    let read = receiver.poll_mailbox().expect("poll");

    assert_eq!(
        read.len(),
        2,
        "exactly the two posted messages are read (no double-post)"
    );
    assert_eq!(read[0].text, "first, by sent time");
    assert_eq!(read[1].text, "second, by sent time");
    assert_eq!(read[0].sent_time, Some(earlier_ts));
    assert_eq!(read[1].sent_time, Some(later_ts));
    assert!(read[0].sent_time < read[1].sent_time);
    assert_eq!(read[0].sender_handle, SENDER_HANDLE);
    assert!(read.iter().all(|m| m.delivered_time.is_some()));

    let again = receiver.poll_mailbox().expect("second poll");
    assert!(
        again.is_empty(),
        "delivered messages are dropped from the sidechain and not re-read"
    );
}

/// The Algorand address for a mnemonic.
fn address_of(mnemonic: &str) -> String {
    AlgoOps::address_from_passphrase(mnemonic).expect("address from mnemonic")
}
