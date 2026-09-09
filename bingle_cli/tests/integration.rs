//! Integration tests for `bingle_cli chat`: first-run registration (issue #59), engine start-up
//! (issue #60), and the interactive REPL (issue #61).
//!
//! The offline cases exercise the compiled binary on the paths that need no blockchain (they hinge
//! on a "no keypair" status, which is resolved without a chain read) and assert exit codes/messages.
//!
//! The localnet cases (module `localnet`) are **self-provisioning**: they stand up a full algokit
//! localnet environment via `bingle_test::localnet` (funded accounts, a deployed Bingle app + asset,
//! registered handles, two root relays and STUN servers) and drive the compiled `chat` binary
//! against it — first-run registration, second-run start-up with no credentials, sending from the
//! REPL, and the inbound receive/print path. They **skip cleanly (pass)** when no localnet is
//! reachable, so a plain `cargo test` without a localnet is unaffected. Running them for real needs
//! a live `algokit localnet` (algod `localhost:4001`, indexer `localhost:8980`) and the
//! `algokit`/`goal` CLIs on `PATH`.

use std::process::Command;

// Store-and-forward end-to-end against a real Sidewinder node on LocalNet (epic #200, story #216).
// Gated behind the `sidewinder_localnet` feature so it only compiles/runs when explicitly enabled: it
// needs a live `sw-node` (via scripts/e2e_sidewinder_localnet.sh) + algokit localnet and FAILS (not skips)
// when they are absent. See the module's own docs for how to run it.
#[cfg(feature = "sidewinder_localnet")]
#[path = "store_and_forward_e2e.rs"]
mod store_and_forward_e2e;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_bingle_cli"))
        .args(args)
        .output()
        .expect("failed to run bingle_cli binary")
}

#[test]
#[cfg(not(target_os = "ios"))]
fn chat_with_empty_state_file_and_no_credentials_exits_2() {
    // A state file that does not exist yet defers the handle, so parsing succeeds and we reach the
    // registration decision: no keypair and no credentials -> NeedCredentials. This resolves as a
    // "no keypair" status without touching the chain, so it is deterministic offline.
    let dir = tempfile::tempdir().expect("tempdir");
    let state = dir.path().join("new.json").to_string_lossy().into_owned();
    let out = run(&["chat", "--state_file", &state]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "no credentials should exit 2; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no registered account"),
        "should explain a registered account is required; got: {stderr}"
    );
}

#[test]
#[cfg(not(target_os = "ios"))]
fn chat_with_handle_but_no_passphrase_exits_2() {
    // A handle alone cannot create an account; still needs a funded passphrase (or a saved account).
    let out = run(&["chat", "--handle", "alice"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "handle without passphrase should exit 2; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no registered account"),
        "should point at the missing credentials; got: {stderr}"
    );
}

#[test]
#[cfg(not(target_os = "ios"))]
fn chat_help_still_exits_0() {
    let out = run(&["chat", "--help"]);
    assert!(out.status.success(), "chat --help should exit 0");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("Usage: bingle_cli chat"),
        "help should print the chat usage line"
    );
}

/// Self-provisioning localnet end-to-end tests for `bingle_cli chat`. Replaces the previous
/// env-var-gated `#[ignore]`d live tests (which needed hand-set `BINGLE_IT_*` and a pre-registered
/// account) with a test that stands up the whole environment itself.
#[cfg(not(target_os = "ios"))]
mod localnet {
    use std::io::{BufRead, Write};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use bingle_core::api::bingle_api::{BingleApi, OnMessageHandler};
    use bingle_core::api::bingle_api_impl::BingleApiImpl;
    use bingle_core::engine::BingleAccessUnsafeForTests;
    use bingle_test::localnet::{provision, relay_test_util, setup_localnet, test_util};

    // A funded localnet account used as the in-process receiver/echo peer (not a relay account).
    const RECEIVER_ADDRESS: &str = "P577OS2FPV7COU3Y43PCTS2IIZ5HAXHBZRHINAATVA5ECCEYKFSEVIYTHE";
    const RECEIVER_PASSPHRASE: &str = "lift all minute first hair appear panel unfold pony property also dinosaur start robot board erupt tent pink essence stem protect ugly orphan absent dust";

    /// Install an OnMessage handler on the in-process peer that records the first message + its text
    /// and echoes `Echo: <text>` back to the sender, so the CLI's inbound receive/print path is
    /// exercised (mirrors `bingle_cli run --echo`).
    fn install_echo_handler(
        api: &Arc<BingleApiImpl>,
        received: &Arc<AtomicBool>,
        text: &Arc<Mutex<Option<String>>>,
    ) {
        let received = received.clone();
        let text_store = text.clone();
        let echo_api = api.clone();
        let handler: Arc<OnMessageHandler> = Arc::new(move |sender, sender_handle, message| {
            let text = message
                .get("text")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            tracing::info!(
                "[e2e peer] got message from {sender} ({sender_handle}): {text:?}; echoing back"
            );
            if let Some(t) = &text
                && let Ok(mut g) = text_store.lock()
            {
                *g = Some(t.clone());
            }
            received.store(true, Ordering::SeqCst);
            let reply =
                serde_json::json!({ "text": format!("Echo: {}", text.unwrap_or_default()) });
            if let Err(e) = echo_api.send_message_to_id(&sender, reply, None) {
                tracing::warn!("[e2e peer] echo send back to {sender} failed: {e:?}");
            }
        });
        api.access_unsafe_for_tests(|c: &mut BingleApiImpl| c.set_on_message(Some(handler)));
    }

    /// Full chat epic over a real localnet, driving the compiled `chat` binary:
    ///  1. **first-run registration** — `chat --passphrase --handle` registers "sender" on-chain and
    ///     writes its state file (folds in the old `live_first_run_*` test);
    ///  2. **second run needs no credentials** — the send run below starts from only `--state_file`
    ///     (no `--passphrase`), and reaching delivery proves the engine started (folds in
    ///     `live_chat_starts_engine_and_reaches_started`);
    ///  3. **send + receive/print** — a line typed into the REPL is delivered to the in-process peer,
    ///     which echoes back; the CLI prints the reply, which we assert on its stdout (folds in
    ///     `live_repl_send_to_echo_peer_prints_reply`).
    #[ntest::timeout(300_000)]
    #[serial_test::serial(localnet_chat)]
    #[test]
    fn chat_registers_sends_and_receives_over_localnet() {
        if !provision::localnet_available() {
            eprintln!(
                "skipping localnet chat e2e: algokit localnet not reachable at localhost:4001"
            );
            return;
        }
        test_util::init_test_logging();

        let cfg = test_util::localnet_config();
        // Fund every account we use (relays + sender + receiver).
        setup_localnet::ensure_localnet_accounts_funded(
            &cfg,
            &[
                test_util::ADDRESS_SPEND,
                test_util::ADDRESS_RECEIVE,
                test_util::ADDRESS_10MIL,
                RECEIVER_ADDRESS,
            ],
        )
        .expect("fund localnet accounts");

        // Deploy app + asset, register two root relays, bring up STUN.
        let creator = test_util::ops_from_mnemonic(
            test_util::ADDRESS_SPEND,
            test_util::PASSPHRASE_SPEND,
            cfg.clone(),
        );
        let (app_id, asset_id) =
            test_util::deploy_bingle_app_and_asset(&creator, "BINGLE$", 1_000_000);

        let r1_port = test_util::find_unused_loopback_port();
        let r2_port = test_util::find_unused_loopback_port();
        let relay1_addr = std::net::SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), r1_port);
        let relay2_addr = std::net::SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), r2_port);
        provision::register_relays(app_id, asset_id, relay1_addr, relay2_addr);

        let relay1 = test_util::start_root_relay(
            "relay1",
            relay1_addr,
            test_util::PASSPHRASE_SPEND,
            app_id,
            cfg.clone(),
        );
        let relay2 = test_util::start_root_relay(
            "relay2",
            relay2_addr,
            test_util::PASSPHRASE_RECEIVE,
            app_id,
            cfg.clone(),
        );

        let (mut s1, mut s2, stun_list) = provision::setup_stun_servers(false);
        let stun_arg = format!("{},{}", stun_list[0], stun_list[1]);

        // Register the receiver ("receiver") on-chain and start it as an in-process echo peer.
        test_util::register_client_on_blockchain(
            RECEIVER_ADDRESS,
            RECEIVER_PASSPHRASE,
            "receiver",
            app_id,
            asset_id,
            &creator,
            cfg.clone(),
        );
        let receiver = provision::start_client(
            "receiver",
            RECEIVER_PASSPHRASE,
            stun_list.clone(),
            app_id,
            cfg.clone(),
        );
        let received = Arc::new(AtomicBool::new(false));
        let got_text: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        install_echo_handler(&receiver, &received, &got_text);
        assert!(
            test_util::wait_for_registered(&receiver, Duration::from_secs(180)),
            "receiver did not reach Registered state"
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let sender_state = dir.path().join("sender.state.json");
        let sender_state_s = sender_state.to_string_lossy().into_owned();
        let node_file = dir.path().join("localnet.node.json");
        provision::write_localnet_node_file(&node_file, app_id, asset_id);
        let node_file_s = node_file.to_string_lossy().into_owned();

        // (1) First run: register "sender" on-chain from a funded passphrase and write the state
        // file. stdin is closed (EOF), so after registration the engine starts and the REPL exits
        // cleanly with status 0.
        let reg = Command::new(env!("CARGO_BIN_EXE_bingle_cli"))
            .args([
                "chat",
                "--info",
                "--state_file",
                &sender_state_s,
                "--node-file",
                &node_file_s,
                "--stun-servers",
                &stun_arg,
                "--passphrase",
                test_util::PASSPHRASE_10MIL,
                "--handle",
                "sender",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("run first-run registration");
        assert!(
            reg.status.success(),
            "first-run registration should exit 0; stderr:\n{}",
            String::from_utf8_lossy(&reg.stderr)
        );
        assert!(
            sender_state.exists(),
            "sender state file should be written on first run"
        );
        // Make sure the freshly registered handle is visible before the second run resolves it.
        assert!(
            relay_test_util::wait_for_handles_visible(
                cfg.clone(),
                app_id,
                &["sender"],
                Duration::from_secs(60),
            ),
            "sender handle did not become visible via indexer within 60s"
        );

        // (2)+(3) Second run: start from only the saved state file (no --passphrase), send a line
        // from the REPL to the peer, and expect the echoed reply back in the CLI's stdout. Keep
        // stdin open so the REPL + background retry can complete before teardown.
        let mut child = Command::new(env!("CARGO_BIN_EXE_bingle_cli"))
            .args([
                "chat",
                "--info",
                "--state_file",
                &sender_state_s,
                "--node-file",
                &node_file_s,
                "--stun-servers",
                &stun_arg,
                "--to",
                "receiver",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn bingle_cli chat");

        let mut child_stdin = child.stdin.take().expect("child stdin");
        let child_stdout = child.stdout.take().expect("child stdout");
        // Drain stdout on a background thread, accumulating the transcript and flipping `ready` once
        // the CLI reports it is listening (issue #91). We assert on the transcript after teardown and
        // use `ready` to time the first send precisely instead of a blind sleep.
        let transcript_buf = Arc::new(Mutex::new(String::new()));
        let ready = Arc::new(AtomicBool::new(false));
        let stdout_reader = {
            let transcript_buf = transcript_buf.clone();
            let ready = ready.clone();
            std::thread::spawn(move || {
                let mut reader = std::io::BufReader::new(child_stdout);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) => break, // EOF (child exited / stdout closed)
                        Ok(_) => {
                            if line.contains("ready to chat") {
                                ready.store(true, Ordering::SeqCst);
                            }
                            if let Ok(mut buf) = transcript_buf.lock() {
                                buf.push_str(&line);
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        // Wait for the CLI to reach the listening state (the readiness gate from issue #91) before
        // sending, rather than a blind sleep — this exercises the new gate. Fall back after 60s so a
        // stuck connection surfaces as a normal assertion failure below.
        let ready_deadline = Instant::now();
        while ready_deadline.elapsed() < Duration::from_secs(60) {
            if ready.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        assert!(
            ready.load(Ordering::SeqCst),
            "CLI did not report 'ready to chat' (listening) within 60s"
        );
        writeln!(child_stdin, "Hello from CLI").expect("write message to chat stdin");
        child_stdin.flush().ok();

        // Wait for the peer to receive it (the pending-retry model keeps trying while the relay path
        // settles).
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(120) {
            if received.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        // Give the echo reply time to travel back and print in the CLI transcript.
        std::thread::sleep(Duration::from_secs(10));

        // Tear down the chat process (drop stdin first so the REPL sees EOF), then collect stdout.
        drop(child_stdin);
        let _ = child.kill();
        let _ = child.wait();
        let _ = stdout_reader.join();
        let transcript = transcript_buf.lock().map(|b| b.clone()).unwrap_or_default();

        let got = received.load(Ordering::SeqCst);
        let text = got_text.lock().expect("lock text").clone();

        // Tear down infra before asserting so a failure doesn't leak relays/STUN.
        relay1.access_unsafe_for_tests(|r: &mut BingleApiImpl| r.stop());
        relay2.access_unsafe_for_tests(|r: &mut BingleApiImpl| r.stop());
        receiver.access_unsafe_for_tests(|c: &mut BingleApiImpl| c.stop());
        s1.stop();
        s2.stop();

        assert!(
            got,
            "peer did not get the message from the chat CLI within the timeout"
        );
        assert_eq!(
            text.as_deref(),
            Some("Hello from CLI"),
            "peer got an unexpected message payload"
        );
        // The readiness gate (issue #91) must announce it is connecting and then confirm it is ready
        // before the prompt/first send — so the first message is not lost.
        assert!(
            transcript.contains("Connecting to the network"),
            "CLI transcript should show the connecting notice; stdout was:\n{transcript}"
        );
        assert!(
            transcript.contains("ready to chat"),
            "CLI transcript should confirm it reached the listening state; stdout was:\n{transcript}"
        );
        assert!(
            transcript.contains("receiver: Echo: Hello from CLI"),
            "CLI transcript should show the peer's echoed reply; stdout was:\n{transcript}"
        );

        // The receive path persists each inbound message to the --state_file (record_message +
        // save_state), so the echoed reply must be durable in the sender's state after the run — the
        // "persist" half of the epic's send -> receive -> persist loop.
        let state_json = std::fs::read_to_string(&sender_state).expect("read sender state file");
        let state: serde_json::Value =
            serde_json::from_str(&state_json).expect("parse sender state file as JSON");
        let messages = state
            .get("messages")
            .and_then(|m| m.as_array())
            .expect("state file should have a messages array");
        assert!(
            messages.iter().any(|m| {
                m.get("sender_handle").and_then(|v| v.as_str()) == Some("receiver")
                    && m.get("text").and_then(|v| v.as_str()) == Some("Echo: Hello from CLI")
            }),
            "the echoed reply should be stored in the --state_file; messages were:\n{messages:#?}"
        );
    }
}
