//! Store-and-forward end-to-end against a real Sidewinder node on LocalNet (epic #200, story #216).
//!
//! A case in the `integration` suite, gated behind the `sidewinder_localnet` cargo feature so it is
//! **not** compiled or run by the normal suites/CI. It stands up a one-node Sidewinder Mailbox network
//! on algokit LocalNet via `scripts/e2e_sidewinder_localnet.sh` (which drives the installed `sw-node`
//! binary), then drives the store-and-forward path against it. Per the LocalNet-suite policy it
//! **fails, not skips**, when the prerequisites are missing.
//!
//! Prerequisites (see the script header):
//!   - `sw-node` on PATH (`cargo install --path sw-node` in the sidewinder repo),
//!   - algokit LocalNet up (`algokit localnet start`), and `curl`.
//!
//! Run it explicitly (single-threaded — the node binds fixed ports):
//!   `cargo test -p bingle_cli --features sidewinder_localnet --test integration -- --test-threads 1`
//!
//! Stage A (this file) proves the harness + node: a message posted to a recipient's Mailbox is popped
//! back through the [`Mailbox`] client. Stage B (the full two-`bingle_local` offline-send → reconnect
//! flow with handle registration and the epic invariants) builds on this same harness.

use algo_ops::{AlgoChainConfig, AlgoOps};
use bingle_local::api::sidewinder::{Mailbox, MailboxConfig};
use bingle_local::api::{BingleApiLocalImpl, BingleLocalApi, LocalApiConfig};
use bingle_test::localnet::test_util;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Access details printed by `e2e_sidewinder_localnet.sh up`.
struct Endpoints {
    api_url: String,
    token: String,
    sender_mnemonic: String,
    receiver_mnemonic: String,
    receiver_address: String,
}

/// A running one-node Sidewinder Mailbox network, torn down (`script down`) on drop.
struct Cluster {
    script: PathBuf,
    work: PathBuf,
    endpoints: Endpoints,
}

impl Cluster {
    /// Stand up the network via the script, failing (not skipping) with the script's diagnostics when
    /// a prerequisite is missing.
    fn up() -> Cluster {
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/e2e_sidewinder_localnet.sh");
        assert!(
            script.exists(),
            "harness script not found at {} — it is vendored from the sidewinder repo",
            script.display()
        );
        let work = Path::new(env!("CARGO_TARGET_TMPDIR")).join("sw-e2e");

        let output = Command::new("sh")
            .arg(&script)
            .arg("up")
            .env("WORK", &work)
            .output()
            .expect("run e2e_sidewinder_localnet.sh up");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "e2e_sidewinder_localnet.sh up failed (is sw-node installed and algokit localnet running?).\n\
             --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
        );
        let endpoints = parse_endpoints(&stdout)
            .unwrap_or_else(|| panic!("could not parse endpoints from script output:\n{stdout}"));
        Cluster {
            script,
            work,
            endpoints,
        }
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        let _ = Command::new("sh")
            .arg(&self.script)
            .arg("down")
            .env("WORK", &self.work)
            .output();
    }
}

/// Parse the `key : value` block the script prints (endpoint, token, and the sender/receiver address
/// + mnemonic under their `[sender]` / `[receiver]` sections).
fn parse_endpoints(stdout: &str) -> Option<Endpoints> {
    let mut api_url = None;
    let mut token = None;
    let mut sender_mnemonic = None;
    let mut receiver_mnemonic = None;
    let mut receiver_address = None;
    let mut section = "";

    for line in stdout.lines() {
        let trimmed = line.trim();
        match trimmed {
            "[sender]" => section = "sender",
            "[receiver]" => section = "receiver",
            _ => {}
        }
        if let Some(value) = after_key(trimmed, "API endpoint") {
            api_url = Some(value);
        } else if let Some(value) = after_key(trimmed, "Bearer token") {
            token = Some(value);
        } else if let Some(value) = after_key(trimmed, "mnemonic") {
            match section {
                "sender" => sender_mnemonic = Some(value),
                "receiver" => receiver_mnemonic = Some(value),
                _ => {}
            }
        } else if let Some(value) = after_key(trimmed, "address") {
            if section == "receiver" {
                receiver_address = Some(value);
            }
        }
    }

    Some(Endpoints {
        api_url: api_url?,
        token: token?,
        sender_mnemonic: sender_mnemonic?,
        receiver_mnemonic: receiver_mnemonic?,
        receiver_address: receiver_address?,
    })
}

/// If `line` is `<key> ... : <value>`, return the trimmed value. Tolerates the variable spacing the
/// script uses before the colon (`address  :`, `mnemonic :`).
fn after_key(line: &str, key: &str) -> Option<String> {
    let rest = line.strip_prefix(key)?;
    let value = rest.trim_start().strip_prefix(':')?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Whether `popped` carries `message`, tolerating an ARC-4 `byte[]` length prefix around the payload
/// (the wire may length-prefix the declared `byte[]` return; the FIFO primitive stores it raw).
fn payload_matches(popped: &[u8], message: &[u8]) -> bool {
    if popped == message {
        return true;
    }
    if popped.len() == message.len() + 2 {
        let len = u16::from_be_bytes([popped[0], popped[1]]) as usize;
        return len == message.len() && &popped[2..] == message;
    }
    false
}

/// Stage A: a message posted to the receiver's Mailbox is popped back — the harness stands up a real
/// node and the [`Mailbox`] client round-trips a message through it.
#[serial_test::serial(sidewinder_e2e)]
#[test]
fn mailbox_round_trips_against_a_real_localnet_node() {
    let cluster = Cluster::up();
    let e = &cluster.endpoints;

    // Sender posts to the receiver's Mailbox (keyed by the receiver's address).
    let sender = Mailbox::new(
        AlgoOps::new_for_algorand(Some(e.sender_mnemonic.clone()), None, None),
        MailboxConfig::new(e.api_url.clone(), e.token.clone()),
    )
    .expect("sender mailbox");
    let message = b"store-and-forward end-to-end #216 (stage A)".to_vec();
    sender
        .post(&e.receiver_address, &message)
        .expect("post to the receiver's mailbox");

    // Receiver pops its own Mailbox and reads the message back.
    let receiver = Mailbox::new(
        AlgoOps::new_for_algorand(Some(e.receiver_mnemonic.clone()), None, None),
        MailboxConfig::new(e.api_url.clone(), e.token.clone()),
    )
    .expect("receiver mailbox");
    let popped = receiver
        .pop()
        .expect("pop succeeds")
        .expect("a message is queued for the receiver");
    assert!(
        payload_matches(&popped, &message),
        "the receiver reads the posted message back: got {popped:?}"
    );

    // Read-and-drop: a second pop finds nothing.
    let drained = receiver.pop().expect("second pop succeeds");
    assert!(
        drained.is_none(),
        "the message is dropped from the sidechain and not re-read"
    );
}

/// Build a bingle_local config for LocalNet + this cluster: the localnet algod (with the deployed
/// app/asset ids) and the cluster's Sidewinder Mailbox, with `send`/`receive` set as given.
fn local_config(
    api_url: &str,
    token: &str,
    app_id: u64,
    asset_id: u64,
    send: bool,
    receive: bool,
) -> LocalApiConfig {
    let algo_config = AlgoChainConfig {
        app_id: Some(app_id),
        asset_id: Some(asset_id),
        ..test_util::localnet_config()
    };
    LocalApiConfig {
        algo_config,
        app_id,
        asset_id,
        sidewinder: Some(MailboxConfig::new(api_url.to_string(), token.to_string())),
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

/// Stage B: the full store-and-forward path across two `bingle_local` instances against a real node.
/// An offline send gives up and is posted to the recipient's Mailbox; the recipient reconnects,
/// polls, reads, decrypts, and the messages land on its store sorted by sent time and gone from the
/// sidechain — asserting the epic invariants (no double-post, no re-read, sorted by sent time).
#[serial_test::serial(sidewinder_e2e)]
#[test]
fn offline_send_survives_and_is_read_on_reconnect() {
    let cluster = Cluster::up();
    let e = &cluster.endpoints;
    let cfg = test_util::localnet_config();

    // Deploy the Bingle app + asset, and register a handle for the sender and the receiver so the
    // sender's give-up post can resolve the recipient handle → address (and the reader can attribute
    // the sender). The two accounts are the cluster's enrolled Mailbox callers.
    let sender_ops = test_util::ops_from_mnemonic(
        // address is derived from the mnemonic; a placeholder label is fine here.
        &AlgoOps::address_from_passphrase(&e.sender_mnemonic).expect("sender address"),
        &e.sender_mnemonic,
        cfg.clone(),
    );
    let (app_id, asset_id) =
        test_util::deploy_bingle_app_and_asset(&sender_ops, "Bingle$", 1_000_000);

    let sender_address = AlgoOps::address_from_passphrase(&e.sender_mnemonic).expect("sender addr");
    test_util::register_client_on_blockchain(
        &sender_address,
        &e.sender_mnemonic,
        "sf-sender",
        app_id,
        asset_id,
        &sender_ops,
        cfg.clone(),
    );
    test_util::register_client_on_blockchain(
        &e.receiver_address,
        &e.receiver_mnemonic,
        "sf-receiver",
        app_id,
        asset_id,
        &sender_ops,
        cfg.clone(),
    );

    // ── sender: two offline sends give up and are posted to the receiver's Mailbox ──────────────
    let mut sender = BingleApiLocalImpl::new(local_config(
        &e.api_url, &e.token, app_id, asset_id, true, false,
    ));
    sender
        .import_keypair(e.sender_mnemonic.clone())
        .expect("import sender keypair");

    // The earlier-sent message is added second and posted first, so a correct read must reorder by
    // sent time, not by Mailbox (FIFO) arrival order.
    let earlier_ts = 1_726_600_000_000;
    let later_ts = 1_726_600_005_000;
    sender
        .add_message(
            "sf-sender".to_string(),
            vec!["sf-receiver".to_string()],
            earlier_ts,
            "first, by sent time".to_string(),
            None,
        )
        .expect("add earlier message");
    sender
        .add_message(
            "sf-sender".to_string(),
            vec!["sf-receiver".to_string()],
            later_ts,
            "second, by sent time".to_string(),
            None,
        )
        .expect("add later message");

    fail_delivery_twice(&mut sender, later_ts);
    fail_delivery_twice(&mut sender, earlier_ts);

    // ── receiver: reconnect, poll, read, decrypt ─────────────────────────────────────────────────
    let mut receiver = BingleApiLocalImpl::new(local_config(
        &e.api_url, &e.token, app_id, asset_id, false, true,
    ));
    receiver
        .import_keypair(e.receiver_mnemonic.clone())
        .expect("import receiver keypair");

    let read = receiver.poll_mailbox().expect("poll");

    // No double-post: exactly two messages despite each delivery failing twice.
    assert_eq!(
        read.len(),
        2,
        "exactly the two posted messages are read (no double-post)"
    );
    // Sorted by sent time: the earlier-sent message comes first, though it was posted second.
    assert_eq!(read[0].text, "first, by sent time");
    assert_eq!(read[1].text, "second, by sent time");
    assert_eq!(read[0].sent_time, Some(earlier_ts));
    assert_eq!(read[1].sent_time, Some(later_ts));
    assert!(read[0].sent_time < read[1].sent_time);
    // The sender is attributed by its registered handle (reverse-resolved from the signed address).
    assert_eq!(read[0].sender_handle, "sf-sender");
    assert!(read.iter().all(|m| m.delivered_time.is_some()));

    // The decrypted messages are on the receiver's local store.
    let stored = receiver.get_messages().expect("messages");
    assert!(
        stored.iter().any(|m| m.text == "first, by sent time")
            && stored.iter().any(|m| m.text == "second, by sent time")
    );

    // No re-read: the delivered messages are gone from the sidechain, so a second poll is empty.
    let again = receiver.poll_mailbox().expect("second poll");
    assert!(
        again.is_empty(),
        "delivered messages are dropped from the sidechain and not re-read"
    );
}
