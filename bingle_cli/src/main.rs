mod module_version;

use std::fs;
use std::io::Write;
use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};

use algo_ops::error::{AlgoError, AlgoErrorKind};
use algo_ops::{AlgoChainConfig, AlgoOps};
use bingle_core::api::bingle_api::{
    BingleApi, BingleApiInternal, BingleError, OnConnectHandler, OnListeningHandler,
    OnMessageHandler,
};
use bingle_core::api::bingle_api_impl::BingleApiImpl;
use bingle_core::blockchain::algo_bingle::AlgoBingle;
use bingle_core::ddb::{AdvertRecord, InetSocketAddress};
use bingle_core::engine::BingleAccess;
use bingle_core::util::cli_utils::{args_request_auto_migrate, parse_start_options_from_args};
use bingle_core::util::config_utils::{
    parse_algos_decimal_to_microalgos, parse_node_file_with_ids, resolve_app_asset_ids,
};
use bingle_core::util::logging::{BingleFormatter, HandleLayer, LogMode};
use chrono::Utc;
use ed25519_dalek::SigningKey;
use tracing::warn;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;

fn init_logger_from_args(args: &mut Vec<String>) {
    // Parse and strip global logging flags from args, choose the last-specified level if multiple are present
    let mut chosen: Option<LevelFilter> = None;
    let mut chosen_mode: Option<LogMode> = None;
    let mut i = 0usize;
    while i < args.len() {
        let a = args[i].as_str();
        // Support --log-level <level> and --log-level=<level>
        if a == "--log-level" {
            if i + 1 < args.len() {
                let val = args[i + 1].to_ascii_lowercase();
                let lvl = match val.as_str() {
                    "trace" => LevelFilter::TRACE,
                    "debug" => LevelFilter::DEBUG,
                    "info" => LevelFilter::INFO,
                    "warn" | "warning" => LevelFilter::WARN,
                    "error" => LevelFilter::ERROR,
                    _ => LevelFilter::INFO,
                };
                chosen = Some(lvl);
                // Remove flag and its value
                args.remove(i); // remove "--log-level"
                args.remove(i); // remove value now at same index
                continue; // re-check current index
            } else {
                // No value provided; drop the flag and continue
                args.remove(i);
                continue;
            }
        } else if let Some(rest) = a.strip_prefix("--log-level=") {
            let val = rest.to_ascii_lowercase();
            let lvl = match val.as_str() {
                "trace" => LevelFilter::TRACE,
                "debug" => LevelFilter::DEBUG,
                "info" => LevelFilter::INFO,
                "warn" | "warning" => LevelFilter::WARN,
                "error" => LevelFilter::ERROR,
                _ => LevelFilter::INFO,
            };
            chosen = Some(lvl);
            args.remove(i);
            continue;
        } else if a == "--log-mode" {
            if i + 1 < args.len() {
                let val = args[i + 1].to_ascii_lowercase();
                let mode = match val.as_str() {
                    "plain" => LogMode::Plain,
                    "ansi" => LogMode::ANSI,
                    "aws" => LogMode::AWS,
                    "js" => LogMode::JS,
                    _ => LogMode::Plain,
                };
                chosen_mode = Some(mode);
                args.remove(i);
                args.remove(i);
                continue;
            } else {
                args.remove(i);
                continue;
            }
        } else if let Some(rest) = a.strip_prefix("--log-mode=") {
            let val = rest.to_ascii_lowercase();
            let mode = match val.as_str() {
                "plain" => LogMode::Plain,
                "ansi" => LogMode::ANSI,
                "aws" => LogMode::AWS,
                "js" => LogMode::JS,
                _ => LogMode::Plain,
            };
            chosen_mode = Some(mode);
            args.remove(i);
            continue;
        }

        let matched = match a {
            // `--warn` is a friendly alias for `--log-warn`/`-q`; `--info` for `--log-info`; and
            // `--debug` for `--log-debug`. These are consumed here (stripped from args) so subcommand
            // parsers never see them and every command honours them uniformly.
            "--log-warn" | "--warn" | "-q" => {
                chosen = Some(LevelFilter::WARN);
                true
            }
            "--log-info" | "--info" => {
                chosen = Some(LevelFilter::INFO);
                true
            }
            "--log-debug" | "--debug" | "-v" => {
                chosen = Some(LevelFilter::DEBUG);
                true
            }
            "--log-trace" | "--vv" | "-vv" => {
                chosen = Some(LevelFilter::TRACE);
                true
            }
            _ => false,
        };
        if matched {
            args.remove(i);
        } else {
            i += 1;
        }
    }
    // With all logging flags stripped, the first remaining arg is the subcommand. `chat` is an
    // interactive REPL, so it defaults to WARN to keep the prompt clean (use `--info`/`--debug` to
    // restore verbose logs); every other subcommand keeps the INFO default.
    let default_level = if args.first().map(String::as_str) == Some("chat") {
        LevelFilter::WARN
    } else {
        LevelFilter::INFO
    };
    let level = chosen.unwrap_or(default_level);
    let mode = chosen_mode.unwrap_or(LogMode::Plain);
    let fmt_layer = tracing_subscriber::fmt::layer().event_format(BingleFormatter { mode });

    let subscriber = tracing_subscriber::registry()
        .with(level)
        .with(HandleLayer)
        .with(fmt_layer);

    let _ = tracing::subscriber::set_global_default(subscriber);
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    // Version is informational and must print cleanly to stdout regardless of any --log-level
    // filtering, so handle it before the logger is initialized and consumes no other args.
    if args.iter().any(|a| a == "-V" || a == "--version") {
        print_version_and_exit();
    }

    // Top-level help (no subcommand yet) is likewise printed cleanly to stdout before the logger
    // starts. A subcommand's own --help (e.g. `run --help`) is handled inside that command.
    if matches!(args.first().map(String::as_str), Some("--help" | "-h")) {
        print_usage_and_exit(0);
    }

    // Initialize logger from global flags and strip them from args (default Info)
    init_logger_from_args(&mut args);

    tracing::info!("Bingle CLI starting. Version: {}", version_string());

    if args.is_empty() {
        print_usage_and_exit(2);
    }

    // Support top-level help appearing after global flags (e.g. `-v --help`)
    if args[0] == "--help" || args[0] == "-h" {
        print_usage_and_exit(0);
    }

    let sub = args.remove(0);
    match sub.as_str() {
        "run" => cmd_run(args),
        "chat" => cmd_chat(args),
        "register" => cmd_register(args),
        "buybingle" => cmd_buybingle(args),
        "sellbingle" => cmd_sellbingle(args),
        "checkrelays" => cmd_checkrelays(args),
        "--help" | "-h" => print_usage_and_exit(0),
        other => {
            warn!("Unknown subcommand: {}", other);
            std::process::exit(2);
        }
    }
}

fn print_usage_and_exit(code: i32) -> ! {
    let usage = "Usage: bingle_cli <run|chat|register|buybingle|sellbingle|checkrelays> [options]\n  Common options (for all commands): -h|--help | -V|--version | --log-warn|--warn|-q | --log-info|--info | --log-debug|--debug|-v | --log-trace|--vv|-vv | --log-mode <Plain|ANSI|AWS|JS> | --stun-servers <list> | --stun-servers-file <file>\n  Note: chat defaults to WARN-level logs to keep the prompt clean; use --info or --debug to see more.\n  bingle_cli run [--handle <handle>|<handle>] [--passphrase <text>] [--relay] [--static-ip <ip:port>] [--stun-servers <list>] [--stun-servers-file <file>] [--node-file <file>] [--app-id <id>] [--asset-id <id>] [--sentinel-file <path>] [--echo] [--auto-migrate] [--log-mode <Plain|ANSI|AWS|JS>]\n  bingle_cli chat [--handle <handle>|<handle>] [--passphrase <text>] [--to <handle> | --to-id <id>] [--state_file <file>] [--node-file <file>] [--app-id <id>] [--asset-id <id>] [--stun-servers <list>] [--stun-servers-file <file>] [--no-retries] [--info|--debug]\n  bingle_cli register --handle <handle> --passphrase <text> --app-id <id> --asset-id <id> --price-units <n> [--node-file <file>] [--stun-servers <list>] [--stun-servers-file <file>] [--log-mode <Plain|ANSI|AWS|JS>]\n  bingle_cli buybingle <price_algos> --passphrase <text> --app-id <id> --asset-id <id> [--node-file <file>] [--stun-servers <list>] [--stun-servers-file <file>] [--log-mode <Plain|ANSI|AWS|JS>]\n  bingle_cli sellbingle <amount_units> <price_algos> --passphrase <text> --app-id <id> --asset-id <id> [--node-file <file>] [--stun-servers <list>] [--stun-servers-file <file>] [--log-mode <Plain|ANSI|AWS|JS>]\n  bingle_cli checkrelays --passphrase <text> [--node-file <file>] [--app-id <id>] [--asset-id <id>] [--interval-ms <n>] [--stun-servers <list>] [--stun-servers-file <file>] [--log-mode <Plain|ANSI|AWS|JS>]";
    // Help (exit 0) is user-requested output and goes to stdout; a usage error (non-zero) goes to
    // stderr. Both bypass the tracing logger so they are never suppressed by the active log level.
    if code == 0 {
        println!("{usage}");
    } else {
        eprintln!("{usage}");
    }
    std::process::exit(code);
}

/// Compact one-line version summary for the startup log.
fn version_string() -> String {
    let v = module_version::get_version();
    match v.git_sha.as_deref() {
        Some(sha) => format!("{} (git {sha}, built {})", v.version, v.build_timestamp),
        None => format!("{} (built {})", v.version, v.build_timestamp),
    }
}

/// Print version information to stdout and exit successfully. Backs `-V` / `--version`.
fn print_version_and_exit() -> ! {
    let cli = module_version::get_version();
    let core = bingle_core::module_version::get_version();
    println!("bingle_cli {}", cli.version);
    println!("bingle_core {}", core.version);
    if let Some(sha) = cli.git_sha.as_deref() {
        println!("git sha: {sha}");
    }
    println!("built: {}", cli.build_timestamp);
    std::process::exit(0);
}

fn cmd_run(mut args: Vec<String>) {
    // Support subcommand help
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "Usage: bingle_cli run [--handle <handle>|<handle>] [--passphrase <text>] [--relay] [--static-ip <ip:port>] [--stun-servers <list>] [--stun-servers-file <file>] [--node-file <file>] [--sentinel-file <path>] [--echo] [--auto-migrate]"
        );
        std::process::exit(0);
    }

    // Extract and remove optional --sentinel-file <path> and --echo from args before StartOptions parsing
    let mut sentinel_file: Option<String> = None;
    let mut echo_mode = false;
    let mut i = 0usize;
    while i < args.len() {
        if args[i] == "--sentinel-file" {
            if i + 1 >= args.len() {
                warn!("--sentinel-file requires a <path> value");
                std::process::exit(2);
            }
            sentinel_file = Some(args[i + 1].clone());
            // Remove the flag and its value
            args.remove(i); // remove flag
            args.remove(i); // remove value now at same index
            // Do not increment i; continue scanning
            continue;
        }
        if args[i] == "--echo" {
            echo_mode = true;
            args.remove(i);
            continue;
        }
        i += 1;
    }

    // Whether to migrate local state from an ancestor app on startup (see below). Detected from
    // the raw args rather than StartOptions so the flag does not perturb StartOptions constructions.
    let auto_migrate = args_request_auto_migrate(&args);

    // Parse CLI args into StartOptions
    let opts = match parse_start_options_from_args(args.clone()) {
        Ok(o) => o,
        Err(e) => {
            warn!(
                "Error: {}\nUsage: bingle_cli run [--handle <handle>|<handle>] [--passphrase <text>] [--relay] [--static-ip <ip:port>] [--stun-servers <list>] [--stun-servers-file <file>] [--node-file <file>] [--sentinel-file <path>] [--auto-migrate]",
                e
            );
            std::process::exit(2);
        }
    };

    // Verify the app we are being started with (resolved from --app-id, --node-file, or the
    // APP_ID env) is still supported before doing anything on-chain. When the creator supersedes
    // an app with a newer one, a stale client must upgrade rather than run against the dead app —
    // its state-changing methods are hard-blocked on-chain anyway (see spec/block_old_app.md).
    let app_id_for_check = opts.app_id.or_else(|| {
        std::env::var("APP_ID")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
    });
    if let Some(app_id) = app_id_for_check {
        let ops = AlgoOps::new_for_algorand(None, None, opts.algo_provider_config.clone());
        let asset_id_for_ctor = opts
            .asset_id
            .or_else(|| opts.algo_provider_config.as_ref().and_then(|c| c.asset_id))
            .unwrap_or(0);
        let bingle = AlgoBingle::new(ops, app_id, asset_id_for_ctor);
        // Read the successor pointer, retrying while the node is unreachable (mirrors the start
        // loop); any other read error is treated as best-effort and lets start proceed.
        let successor = loop {
            match bingle.successor_app(app_id) {
                Ok(s) => break s,
                Err(e) => {
                    if let Some(ae) = e.downcast_ref::<AlgoError>()
                        && ae.kind == AlgoErrorKind::HostUnreachable
                    {
                        tracing::error!("Algorand node unreachable: {}. Retrying in 60s...", ae);
                        std::thread::sleep(Duration::from_secs(60));
                        continue;
                    }
                    warn!(
                        "Could not verify app {} is supported ({}); continuing to start",
                        app_id, e
                    );
                    break None;
                }
            }
        };
        if let AppSupport::Superseded { app_id, successor } =
            resolve_app_support(Some(app_id), successor)
        {
            warn!(
                "App {} has been superseded by app {}; this client is out of date. Please upgrade to the latest Bingle. Refusing to start.",
                app_id, successor
            );
            std::process::exit(1);
        }
    }

    // Auto-migrate: after the successor check (so we only run against a live app), optionally copy
    // the user's local state from the creator-blessed ancestor app into the current app. This is a
    // one-time, idempotent `migrate_local` — once the user holds a Handle on the current app it is a
    // no-op. Best-effort: a read/transaction hiccup must not stop the client from starting.
    if auto_migrate {
        match (app_id_for_check, opts.algo_passphrase.as_ref()) {
            (Some(app_id), Some(passphrase)) => {
                let ops = AlgoOps::new_for_algorand(
                    Some(passphrase.clone()),
                    None,
                    opts.algo_provider_config.clone(),
                );
                let asset_id_for_ctor = opts
                    .asset_id
                    .or_else(|| opts.algo_provider_config.as_ref().and_then(|c| c.asset_id))
                    .unwrap_or(0);
                let bingle = AlgoBingle::new(ops, app_id, asset_id_for_ctor);
                match bingle.ensure_local_migrated(app_id) {
                    Ok(Some(txid)) => tracing::info!(
                        "auto-migrate: migrated local state to app {} (txid {})",
                        app_id,
                        txid
                    ),
                    Ok(None) => {
                        tracing::info!("auto-migrate: nothing to migrate for app {}", app_id)
                    }
                    Err(e) => warn!("auto-migrate: skipped ({})", e),
                }
            }
            (_, None) => {
                warn!("--auto-migrate set but no --passphrase; skipping local migration")
            }
            _ => {}
        }
    }

    // The compact record this process registered on-chain at startup; used on shutdown
    // to make sure we only clear our own registration (see resolve_shutdown_action).
    let mut registered_static_record: Option<String> = None;

    // If a static IP was provided, attempt to register it on-chain for discovery BEFORE starting the protocol
    if let Some(static_addr) = opts.static_ip {
        // Resolve app_id from StartOptions or APP_ID env
        warn!(
            "Registering static IP {} for on-chain discovery, opts={}",
            static_addr, opts
        );
        let app_id_opt = opts.app_id.or_else(|| {
            std::env::var("APP_ID")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
        });
        match app_id_opt {
            Some(app_id) => {
                // Require a passphrase to sign the transaction
                if opts.algo_passphrase.is_none() {
                    warn!(
                        "--static-ip was provided but no Algorand passphrase (--passphrase) was set; skipping on-chain register_endpoint"
                    );
                } else {
                    let ops = AlgoOps::new_for_algorand(
                        opts.algo_passphrase.clone(),
                        None,
                        opts.algo_provider_config.clone(),
                    );
                    let asset_id_for_ctor = opts
                        .asset_id
                        .or_else(|| opts.algo_provider_config.as_ref().and_then(|c| c.asset_id))
                        .unwrap_or(0);
                    let bingle = AlgoBingle::new(ops.clone(), app_id, asset_id_for_ctor);

                    // Create a compact, signed AdvertRecord for on-chain registration
                    let addr_opt = ops.address.clone();
                    let sk_res = ops.private_key_bytes();

                    if let (Some(addr), Ok(sk_bytes)) = (addr_opt, sk_res) {
                        let id = addr;
                        let sk_arr: [u8; 32] =
                            sk_bytes.try_into().expect("Secret key must be 32 bytes");
                        let signing_key = SigningKey::from_bytes(&sk_arr);
                        let date = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

                        let record = AdvertRecord::new(
                            id,
                            Some(InetSocketAddress::from(static_addr)),
                            Some(opts.am_relay),
                            None,
                            None,
                            date,
                            &signing_key,
                        );
                        let compact = record.serialize_csv();

                        match bingle.register_endpoint(app_id, &compact) {
                            Ok(txid) => {
                                tracing::info!(
                                    "Registered static endpoint {} for app_id {} (tx: {})",
                                    static_addr,
                                    app_id,
                                    txid
                                );
                                registered_static_record = Some(compact);
                            }
                            Err(e) => {
                                warn!(
                                    "Failed to register static endpoint '{}': {}",
                                    static_addr, e
                                );
                            }
                        }
                    } else {
                        warn!(
                            "--static-ip was provided but could not derive account from passphrase; skipping on-chain register_endpoint"
                        );
                    }
                }
            }
            None => {
                warn!(
                    "--static-ip was provided but app_id is missing; set --node-file with app_id or APP_ID env to enable on-chain register_endpoint"
                );
            }
        }
    }

    // Initialize API
    let api = BingleApiImpl::new(&opts);

    // Install handlers (requires mutable access to the Arc contents; CLI owns the only strong ref here)
    {
        let api_for_echo = api.clone();
        api.access(|api_mut| {
            let on_message: Arc<OnMessageHandler> =
                Arc::new(move |sender, sender_handle, message| {
                    tracing::info!(
                        "on_message: sender={} sender_handle={} message={}",
                        sender,
                        sender_handle,
                        message
                    );
                    if echo_mode {
                        // Check if this is a PlainTextMessage: has "text" field and no non-null "app"/"type"
                        if let Some(text) = message.get("text").and_then(|v| v.as_str()) {
                            let is_plain = message.get("app").is_none_or(|v| v.is_null())
                                && message.get("type").is_none_or(|v| v.is_null());
                            if is_plain {
                                let echo_text = format!("Echo: {}", text);
                                let echo_msg = serde_json::json!({ "text": echo_text });
                                tracing::info!("[echo] Echoing back to {}: {}", sender, echo_msg);
                                let ok = api_for_echo.send_message_to_id(&sender, echo_msg, None);
                                if !matches!(ok, Ok(true)) {
                                    tracing::warn!(
                                        "[echo] Failed to send echo to {}: {:?}",
                                        sender,
                                        ok
                                    );
                                }
                            }
                        }
                    }
                });
            api_mut.set_on_message(Some(on_message));

            let on_connect: Arc<OnConnectHandler> = Arc::new(move |sender, sender_handle| {
                tracing::info!(
                    "on_connect: sender={} sender_handle={}",
                    sender,
                    sender_handle
                );
            });
            api_mut.set_on_connect(Some(on_connect));

            // Optional: install OnListening handler to manage a sentinel file
            if let Some(path) = sentinel_file.clone() {
                let p = path.clone();
                let on_listening: Arc<OnListeningHandler> = Arc::new(
                    move |listening: bool, _nat_type: bingle_core::engine::NatType| {
                        if listening {
                            match fs::OpenOptions::new()
                                .create(true)
                                .write(true)
                                .truncate(true)
                                .open(&p)
                            {
                                Ok(mut f) => {
                                    let _ = writeln!(f, "listening");
                                    tracing::info!("Created sentinel file: {}", p);
                                }
                                Err(e) => {
                                    tracing::warn!("Failed to create sentinel file '{}': {}", p, e);
                                }
                            }
                        } else {
                            match fs::remove_file(&p) {
                                Ok(()) => tracing::info!("Removed sentinel file: {}", p),
                                Err(e) => {
                                    tracing::warn!("Failed to remove sentinel file '{}': {}", p, e)
                                }
                            }
                        }
                    },
                );
                api_mut.set_on_listening(Some(on_listening));
            }
        });
    }

    // Start API
    loop {
        let mut start_res = Ok(());
        api.access(|api_mut| {
            start_res = api_mut.start(&opts);
        });

        match start_res {
            Ok(_) => break,
            Err(e) => {
                if let BingleError::Algo(ae) = &e
                    && ae.kind == AlgoErrorKind::HostUnreachable
                {
                    tracing::error!("Algorand node unreachable: {}. Retrying in 60s...", ae);
                    std::thread::sleep(Duration::from_secs(60));
                    continue;
                }
                warn!("Failed to start: {}", e);
                std::process::exit(1);
            }
        }
    }

    // Install Ctrl-C handler
    let (tx, rx) = channel::<()>();
    install_ctrlc_handler(tx);
    tracing::info!("Started. Press Ctrl-C or send SIGTERM to stop.");

    // Wait until Ctrl-C
    let _ = rx.recv();

    tracing::info!("Received shutdown signal. Stopping...");

    // Determine shutdown action and execute if needed
    let app_id_env = std::env::var("APP_ID")
        .ok()
        .and_then(|s| s.parse::<u64>().ok());
    let action = resolve_shutdown_action(&opts, app_id_env);
    match action {
        ShutdownAction::Unregister {
            app_id,
            passphrase,
            algo_provider_config,
            asset_id,
        } => {
            let ops =
                AlgoOps::new_for_algorand(Some(passphrase), None, algo_provider_config.clone());
            let asset_id_for_ctor = asset_id
                .or_else(|| algo_provider_config.as_ref().and_then(|c| c.asset_id))
                .unwrap_or(0);
            let bingle = AlgoBingle::new(ops.clone(), app_id, asset_id_for_ctor);
            // Guard against the redeploy race: a replacement task may already have
            // registered a newer record under the same account, so only clear the
            // record this process wrote at startup.
            let proceed = match ops.address.clone() {
                Some(addr) => match bingle.get_static_endpoint(app_id, &addr) {
                    Ok(current) => {
                        let clear = AlgoBingle::should_clear_static_endpoint(
                            registered_static_record.as_deref(),
                            current.as_deref(),
                        );
                        if !clear {
                            tracing::info!(
                                "[cmd_run] Skipping static endpoint unregistration: on-chain record {:?} is not the record registered by this process {:?}",
                                current,
                                registered_static_record
                            );
                        }
                        clear
                    }
                    Err(e) => {
                        tracing::warn!(
                            "[cmd_run] Could not read current static endpoint ({}); unregistering anyway",
                            e
                        );
                        true
                    }
                },
                None => true,
            };
            if proceed {
                tracing::info!(
                    "[cmd_run] Unregistering static endpoint for app_id={}",
                    app_id
                );
                match bingle.register_endpoint(app_id, "") {
                    Ok(txid) => {
                        tracing::info!(
                            "[cmd_run] Unregistered static endpoint for app_id {} (tx: {})",
                            app_id,
                            txid
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            "[cmd_run] Failed to unregister static endpoint for app_id {}: {}",
                            app_id,
                            e
                        );
                    }
                }
            }
        }
        ShutdownAction::NoStaticIp => {
            tracing::debug!("[cmd_run] No static IP configured; skipping endpoint unregistration");
        }
        ShutdownAction::NoAppId => {
            tracing::debug!("[cmd_run] No app_id available; skipping endpoint unregistration");
        }
        ShutdownAction::NoPassphrase => {
            tracing::debug!("[cmd_run] No passphrase available; skipping endpoint unregistration");
        }
    }

    // Tell a relay we are leaving so it removes our DDB entry. Best-effort:
    // relies on the transport ACK, and a failure here must not block shutdown.
    match api.ddb_signoff() {
        Ok(()) => tracing::info!("[cmd_run] Sent DDB signoff"),
        Err(e) => tracing::warn!("[cmd_run] DDB signoff failed (continuing shutdown): {}", e),
    }

    // Stop API
    {
        api.access(|api_mut| {
            api_mut.stop();
        });
    }
    tracing::info!("Stopped.");
}

/// `chat` subcommand: parse arguments, bridge the BingleLocal `--state_file`, then run the first-run
/// registration flow (issue #59). A user can chat only from a registered account, reached either by
/// running `bingle_cli register` beforehand or by passing a funded `--passphrase` + `--handle` here;
/// on later runs the saved state file suffices with no credentials. The interactive session loop is
/// a later subtask of the chat epic (#56); this command takes the account to the point of being
/// registered and ready. The pure startup decision lives in `bingle_cli::chat_register` for testing.
fn cmd_chat(args: Vec<String>) {
    const USAGE: &str = "Usage: bingle_cli chat [--handle <handle>|<handle>] [--passphrase <text>] [--to <handle> | --to-id <id>] [--state_file <file>] [--node-file <file>] [--app-id <id>] [--asset-id <id>] [--stun-servers <list>] [--stun-servers-file <file>] [--no-retries] [--info|--debug]";

    // Subcommand help prints to stdout and exits 0, mirroring the other subcommands.
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        std::process::exit(0);
    }

    let chat_args = match parse_chat_args(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Error: {e}\n{USAGE}");
            std::process::exit(2);
        }
    };

    // Capture the credentials the user supplied on the *command line* (before the state-file bridge
    // may fill them in from the file). The registration decision distinguishes a CLI-supplied handle
    // (to detect a mismatch with an already-registered one) from one resolved out of the file.
    let cli_handle: Option<String> = Some(chat_args.opts.handle.clone()).filter(|h| !h.is_empty());
    let cli_passphrase: Option<String> = chat_args.opts.algo_passphrase.clone();

    // Bridge the state file: resolves handle/passphrase from the stored keypair and seeds contacts.
    let mut state = match ChatState::from_chat_args(&chat_args) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: {e}\n{USAGE}");
            std::process::exit(2);
        }
    };

    // Ensure the account is registered and funded (exits non-zero with a clear message otherwise).
    run_chat_startup(&mut state, cli_handle.as_deref(), cli_passphrase.as_deref());

    // Start the engine and run the interactive REPL (send / receive / switch) until exit.
    run_chat_session(
        state,
        chat_args.to.clone(),
        chat_args.to_id.clone(),
        !chat_args.no_retries,
    );
}

/// How often the background worker checks for a pending outbound message to (re)attempt. A message
/// is only eligible once its per-message backoff has elapsed, so this poll is just the scheduler
/// tick, not the retry interval (that is `bingle_local::api::send_retry::RETRY_BACKOFF`).
const RETRY_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How long the chat REPL waits for the node to reach the listening state before showing the prompt
/// (issue #91). On timeout it proceeds best-effort so the REPL never hangs — outbound messages are
/// still queued and retried by the background worker.
const LISTENING_WAIT_TIMEOUT: Duration = Duration::from_secs(45);

/// `MessageSender` backed by the live engine: sends by handle or id.
struct EngineSender {
    api: Arc<BingleApiImpl>,
}

impl bingle_cli::chat_send::MessageSender for EngineSender {
    fn send_text(
        &self,
        target: &bingle_cli::chat_send::SendTarget,
        message: &serde_json::Value,
    ) -> Result<bool, bingle_core::api::bingle_api::BingleError> {
        use bingle_cli::chat_send::SendTarget;
        // Return the typed error unchanged so the send-failure cause survives to the classifier
        // (issue #99).
        match target {
            SendTarget::Handle(handle) => {
                self.api
                    .send_message_to_handle(handle, message.clone(), None)
            }
            SendTarget::Id(id) => self.api.send_message_to_id(id, message.clone(), None),
        }
    }
}

/// Print `prompt` with no trailing newline and flush, so the cursor sits after it.
fn reprint_prompt(prompt: &str) {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
}

/// Start the P2P engine for a ready (registered, funded) account and run the interactive REPL:
/// receive/print/persist incoming messages, and prompt for outgoing ones. Plain input sends to the
/// current recipient (persisted + retried via the pending-message model); `/prefix` switches the
/// recipient (resolved to its canonical handle via `handle_lookup_partial`); `!exit` or Ctrl-D exits
/// cleanly (Ctrl-C likewise, via the signal handler). Mirrors `cmd_run`'s start loop.
fn run_chat_session(
    state: ChatState,
    to: Option<String>,
    to_id: Option<String>,
    retries_enabled: bool,
) {
    let opts = state.opts.clone();
    // Shared with the engine callback and the retry worker: all mutate the one ChatState.
    let shared = Arc::new(std::sync::Mutex::new(state));
    let api = BingleApiImpl::new(&opts);
    // The prompt currently shown, so async prints (received messages, retry results) can redraw it.
    let prompt_line = Arc::new(std::sync::Mutex::new(CurrentRecipient::None.prompt()));
    // Readiness gate: flipped true by the `on_listening(true, …)` callback once the node is actually
    // listening on the network. The REPL waits on this before showing the prompt so the user cannot
    // send (and lose) a message before there is a return path (issue #91).
    let listening_gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));

    {
        let shared_for_msg = shared.clone();
        let prompt_for_msg = prompt_line.clone();
        let listening_gate_cb = listening_gate.clone();
        api.access(|api_mut| {
            let on_message: Arc<OnMessageHandler> =
                Arc::new(move |sender, sender_handle, message| {
                    let received = {
                        let mut guard = match shared_for_msg.lock() {
                            Ok(g) => g,
                            Err(e) => {
                                tracing::error!("chat: state lock poisoned; dropping message: {e}");
                                return;
                            }
                        };
                        bingle_cli::chat_receive::receive_message(
                            &mut guard,
                            &sender,
                            &sender_handle,
                            &message,
                        )
                    };
                    if let Some(received) = received {
                        // Print above the current prompt, then redraw it so a half-typed line stays
                        // readable.
                        println!("\n{}: {}", received.sender_handle, received.text);
                        let prompt = prompt_for_msg.lock().map(|g| g.clone()).unwrap_or_default();
                        reprint_prompt(&prompt);
                    }
                });
            api_mut.set_on_message(Some(on_message));

            // Connection/listening status callbacks log at debug, so they are silent at the default
            // WARN level and only surface under `--debug` (or `--info` does not show them). Installed
            // unconditionally — the level filter, not this wiring, decides whether they are seen.
            let on_connect: Arc<OnConnectHandler> = Arc::new(move |sender, sender_handle| {
                tracing::debug!("chat: connected sender={} handle={}", sender, sender_handle);
            });
            api_mut.set_on_connect(Some(on_connect));

            let on_listening: Arc<OnListeningHandler> = Arc::new(move |listening, _nat_type| {
                tracing::debug!("chat: listening={}", listening);
                // Signal the readiness gate once (and only once) the node is listening, so the REPL
                // can stop waiting and show the prompt (issue #91).
                if listening
                    && let Ok(mut ready) = listening_gate_cb.0.lock()
                    && !*ready
                {
                    *ready = true;
                    listening_gate_cb.1.notify_all();
                }
            });
            api_mut.set_on_listening(Some(on_listening));
        });
    }

    // Start the engine, retrying while the Algorand node is unreachable (mirrors cmd_run).
    loop {
        let mut start_res = Ok(());
        api.access(|api_mut| {
            start_res = api_mut.start(&opts);
        });
        match start_res {
            Ok(_) => break,
            Err(e) => {
                if let BingleError::Algo(ae) = &e
                    && ae.kind == AlgoErrorKind::HostUnreachable
                {
                    tracing::error!("Algorand node unreachable: {}. Retrying in 60s...", ae);
                    std::thread::sleep(Duration::from_secs(60));
                    continue;
                }
                warn!("chat: failed to start: {}", e);
                std::process::exit(1);
            }
        }
    }

    // Ctrl-C / SIGTERM: the main thread blocks on stdin, so the signal handler itself performs the
    // clean shutdown (stop + final save) and exits.
    {
        let api_sig = api.clone();
        let shared_sig = shared.clone();
        if let Err(e) = ctrlc::set_handler(move || {
            tracing::info!("chat: received signal; shutting down");
            api_sig.access(|api_mut| api_mut.stop());
            if let Ok(guard) = shared_sig.lock() {
                let _ = guard.save_state();
            }
            std::process::exit(0);
        }) {
            warn!("chat: failed to install signal handler: {}", e);
        }
    }

    // Background retry worker: keep re-attempting pending outbound messages (transient failures are
    // retried indefinitely with per-message backoff, mirroring the RN client). Not started under
    // --no-retries, where a failed send is reported once and never queued.
    if retries_enabled {
        let retry_api = api.clone();
        let retry_shared = shared.clone();
        let retry_prompt = prompt_line.clone();
        std::thread::spawn(move || {
            let sender = EngineSender { api: retry_api };
            let mut retry_after: std::collections::HashMap<i64, std::time::Instant> =
                std::collections::HashMap::new();
            loop {
                std::thread::sleep(RETRY_POLL_INTERVAL);
                let outcome = match retry_shared.lock() {
                    Ok(mut guard) => bingle_cli::chat_send::retry_pending(
                        &sender,
                        &mut guard,
                        &mut retry_after,
                        std::time::Instant::now(),
                    ),
                    Err(_) => continue,
                };
                let Some(outcome) = outcome else {
                    continue; // nothing eligible right now
                };
                match outcome.outcome {
                    SendOutcome::Delivered => println!("\n✓ delivered to {}", outcome.recipient),
                    SendOutcome::Failed(reason) => {
                        println!("\n! send to {} failed: {}", outcome.recipient, reason)
                    }
                    // Transient: still retrying, stays quiet (the first failure already printed).
                    SendOutcome::Retrying(_) => continue,
                }
                let prompt = retry_prompt.lock().map(|g| g.clone()).unwrap_or_default();
                reprint_prompt(&prompt);
            }
        });
    }

    tracing::info!(
        "chat: started as '{}'. Type a message, /handle to switch recipient, or !exit.",
        opts.handle
    );

    // Wait for the node to actually be listening before showing the prompt, so the first typed
    // message is not sent (and its reply lost) before there is a return path (issue #91). Incoming
    // messages and retry results still print during the wait — their callbacks/thread are already
    // installed. Bounded by a timeout so an unreachable network never wedges the REPL.
    {
        println!("Connecting to the network…");
        let _ = std::io::stdout().flush();
        let (lock, cvar) = &*listening_gate;
        let ready = match lock.lock() {
            Ok(guard) => cvar
                .wait_timeout_while(guard, LISTENING_WAIT_TIMEOUT, |ready| !*ready)
                .map(|(guard, _)| *guard)
                .unwrap_or(false),
            Err(_) => false,
        };
        if ready {
            println!("Connected — ready to chat.");
        } else {
            warn!(
                "chat: still connecting after {}s; proceeding best-effort.",
                LISTENING_WAIT_TIMEOUT.as_secs()
            );
            let hint = if retries_enabled {
                "messages you send will be queued and retried until the connection is ready"
            } else {
                "a message you send now may fail if the connection is not yet ready"
            };
            println!("Still connecting; {hint}.");
        }
        let _ = std::io::stdout().flush();
    }

    // Interactive loop on the main thread (engine + retry worker run on their own threads).
    let sender = EngineSender { api: api.clone() };
    let mut recipient = CurrentRecipient::from_args(to.as_deref(), to_id.as_deref());
    let stdin = std::io::stdin();
    loop {
        let prompt = recipient.prompt();
        if let Ok(mut g) = prompt_line.lock() {
            *g = prompt.clone();
        }
        reprint_prompt(&prompt);

        let mut line = String::new();
        match stdin.read_line(&mut line) {
            Ok(0) => {
                println!();
                break;
            } // Ctrl-D / EOF
            Ok(_) => {}
            Err(e) => {
                warn!("chat: input error: {}", e);
                break;
            }
        }

        match parse_input(&line) {
            ChatInput::Empty => continue,
            ChatInput::Exit => break,
            ChatInput::Switch { prefix } => {
                if prefix.is_empty() {
                    println!("usage: /<handle-prefix>");
                    continue;
                }
                match api.handle_lookup_partial(&prefix) {
                    Ok(Some((id, canonical))) => {
                        recipient = CurrentRecipient::Handle {
                            handle: canonical.clone(),
                            id: Some(id.clone()),
                        };
                        if let Ok(mut guard) = shared.lock()
                            && let Err(e) = guard.add_received_contact(&canonical, &id)
                        {
                            warn!("chat: could not record contact '{}': {}", canonical, e);
                        }
                    }
                    Ok(None) => println!("no handle matching '{prefix}'"),
                    Err(e) => println!("lookup failed: {e}"),
                }
            }
            ChatInput::Send { text } => match recipient.target() {
                None => println!("no recipient; use /<handle> to pick one, or start with --to"),
                Some(target) => {
                    let outcome = match shared.lock() {
                        Ok(mut guard) => bingle_cli::chat_send::send_once(
                            &sender,
                            &mut guard,
                            &target,
                            &text,
                            retries_enabled,
                        ),
                        Err(_) => {
                            warn!("chat: state lock poisoned");
                            continue;
                        }
                    };
                    // On success print nothing: the terminal already echoed the typed line, which is
                    // the transcript entry. Only surface failures.
                    match outcome {
                        SendOutcome::Delivered => {}
                        SendOutcome::Retrying(reason) => println!(
                            "! send to {} not delivered ({}); will keep retrying…",
                            target.label(),
                            reason
                        ),
                        SendOutcome::Failed(reason) => {
                            println!("! send to {} failed: {}", target.label(), reason)
                        }
                    }
                }
            },
        }
    }

    tracing::info!("chat: shutting down...");
    api.access(|api_mut| api_mut.stop());
    if let Ok(guard) = shared.lock()
        && let Err(e) = guard.save_state()
    {
        warn!("chat: final save failed: {}", e);
    }
    tracing::info!("chat: stopped.");
}

/// Drive the first-run registration state machine to a ready account, or exit non-zero with a clear
/// message. On success (the account is registered and adequately funded) it logs readiness and
/// returns; the interactive session loop lands in a later subtask. Never logs the passphrase.
fn run_chat_startup(state: &mut ChatState, cli_handle: Option<&str>, cli_passphrase: Option<&str>) {
    let have_passphrase = cli_passphrase.is_some();
    let mut status = resolve_status_or_exit(state);

    // The loop re-decides after each state-changing step (import, register). Each step strictly
    // advances the account (NoKeypair -> Funded/Active/Unfunded; Funded -> Active), so a small cap
    // is a safety net against an unexpected non-converging status rather than normal flow.
    for _ in 0..6 {
        match decide_startup(&status, cli_handle, have_passphrase) {
            StartupDecision::Proceed { handle } => {
                tracing::info!(
                    "chat: account '{}' is registered and funded ({} known contact(s)); ready to chat",
                    handle,
                    state.contacts.len()
                );
                return;
            }
            StartupDecision::NeedCredentials { gap } => {
                let msg = match gap {
                    CredentialGap::NoAccount => {
                        "no registered account: pass a funded --passphrase and --handle to register on first run, or run `bingle_cli register` first"
                    }
                    CredentialGap::FundedNeedsHandle => {
                        "account is funded but has no handle: pass --handle to register it"
                    }
                };
                eprintln!("Error: {msg}");
                std::process::exit(2);
            }
            StartupDecision::Fund { id, needed_algos } => {
                eprintln!(
                    "Error: account {id} is not sufficiently funded. Add {needed_algos:.6} ALGO to this address, then re-run."
                );
                std::process::exit(1);
            }
            StartupDecision::HandleMismatch { existing, supplied } => {
                eprintln!(
                    "Error: this account is already registered as '{existing}'; it cannot be re-registered as '{supplied}'. Omit --handle or pass '{existing}'."
                );
                std::process::exit(1);
            }
            StartupDecision::Register { handle } => {
                // Ensure a keypair exists, importing from the CLI passphrase on genuine first run,
                // then re-decide against the freshly resolved status (the account may turn out to be
                // already registered, funded, or still unfunded).
                if !state.has_keypair() {
                    let Some(passphrase) = cli_passphrase else {
                        // decide_startup only returns Register for NoKeypair when a passphrase is
                        // present, so this is unreachable in practice; guard defensively.
                        eprintln!("Error: --passphrase is required to register a new account");
                        std::process::exit(2);
                    };
                    if let Err(e) = state.import_keypair(passphrase) {
                        eprintln!("Error: could not import account from passphrase: {e}");
                        std::process::exit(2);
                    }
                    status = resolve_status_or_exit(state);
                    continue;
                }
                // Keypair present and funded but unregistered: register now, then re-resolve.
                match state.register(&handle) {
                    Ok(()) => {
                        tracing::info!("chat: registered handle '{handle}'");
                        status = resolve_status_or_exit(state);
                    }
                    Err(RegisterError::HandleTaken(owner)) => {
                        eprintln!(
                            "Error: handle '{handle}' is already in use by {owner}; choose another handle."
                        );
                        std::process::exit(1);
                    }
                    Err(RegisterError::Other(msg)) => {
                        eprintln!("Error: registration failed: {msg}");
                        std::process::exit(1);
                    }
                }
            }
        }
    }

    eprintln!("Error: could not reach a ready account state; please check funding and try again.");
    std::process::exit(1);
}

/// Resolve the account status or exit non-zero with the error (e.g. blockchain unreachable).
fn resolve_status_or_exit(state: &ChatState) -> chat_register::AccountStatus {
    match state.resolve_account_status() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

use bingle_cli::chat::parse_chat_args;
use bingle_cli::chat_register::{self, CredentialGap, StartupDecision, decide_startup};
use bingle_cli::chat_repl::{ChatInput, CurrentRecipient, parse_input};
use bingle_cli::chat_send::SendOutcome;
use bingle_cli::chat_state::ChatState;
use bingle_cli::chat_state::RegisterError;
use bingle_core::api::network_endpoint::NetworkEndpoint;
use serde_json::json;
use std::net::SocketAddr;
use std::time::Duration;
use std::time::Instant;

fn cmd_checkrelays(mut args: Vec<String>) {
    // Support subcommand help
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "Usage: bingle_cli checkrelays --passphrase <text> [--node-file <file>] [--app-id <id>] [--asset-id <id>] [--interval-ms <n>] [--once] [--stun-servers <list>] [--stun-servers-file <file>]\n\
              Notes: by default this command repeats indefinitely, sleeping --interval-ms between runs. Use --once to run a single iteration."
        );
        std::process::exit(0);
    }

    // Extract optional --interval-ms <n>
    let mut interval_ms: u64 = 5000;
    let mut once: bool = false;
    let mut i = 0usize;
    while i < args.len() {
        if args[i] == "--interval-ms" {
            if i + 1 >= args.len() {
                warn!("--interval-ms requires a value");
                std::process::exit(2);
            }
            match args[i + 1].parse::<u64>() {
                Ok(v) => interval_ms = v,
                Err(e) => {
                    warn!("invalid --interval-ms: {}", e);
                    std::process::exit(2);
                }
            }
            args.remove(i); // flag
            args.remove(i); // value
            continue;
        } else if args[i] == "--once" {
            once = true;
            args.remove(i);
            continue;
        }
        i += 1;
    }

    // Manually parse only the options needed for checkrelays (no handle required)
    let mut passphrase: Option<String> = None;
    let mut node_file: Option<String> = None;
    let mut cli_app_id: Option<u64> = None;
    let mut cli_asset_id: Option<u64> = None;
    let mut stun_servers: Option<Vec<SocketAddr>> = None;
    let dangerous_debug = false;

    // Remaining args after --interval-ms extraction
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--passphrase" => {
                let p = it.next().unwrap_or_else(|| {
                    warn!("--passphrase requires a value");
                    std::process::exit(2);
                });
                passphrase = Some(p);
            }
            "--node-file" => {
                let nf = it.next().unwrap_or_else(|| {
                    warn!("--node-file requires a <file> value");
                    std::process::exit(2);
                });
                node_file = Some(nf);
            }
            "--app-id" => {
                let v = it.next().unwrap_or_else(|| {
                    warn!("--app-id requires a value");
                    std::process::exit(2);
                });
                match v.parse::<u64>() {
                    Ok(id) => cli_app_id = Some(id),
                    Err(e) => {
                        warn!("Invalid --app-id '{}': {}", v, e);
                        std::process::exit(2);
                    }
                }
            }
            "--asset-id" => {
                let v = it.next().unwrap_or_else(|| {
                    warn!("--asset-id requires a value");
                    std::process::exit(2);
                });
                match v.parse::<u64>() {
                    Ok(id) => cli_asset_id = Some(id),
                    Err(e) => {
                        warn!("Invalid --asset-id '{}': {}", v, e);
                        std::process::exit(2);
                    }
                }
            }
            "--stun-servers" => {
                let v = it.next().unwrap_or_else(|| {
                    warn!("--stun-servers requires a value");
                    std::process::exit(2);
                });
                match bingle_core::util::config_utils::parse_stun_list(&v) {
                    Ok(list) => stun_servers = Some(list),
                    Err(e) => {
                        warn!("{}", e);
                        std::process::exit(2);
                    }
                }
            }
            "--stun-servers-file" => {
                let v = it.next().unwrap_or_else(|| {
                    warn!("--stun-servers-file requires a <file> value");
                    std::process::exit(2);
                });
                match bingle_core::util::config_utils::parse_stun_file(&v) {
                    Ok(list) => stun_servers = Some(list),
                    Err(e) => {
                        warn!("{}", e);
                        std::process::exit(2);
                    }
                }
            }
            s if s.starts_with('-') => {
                warn!("Unknown option: {}", s);
                std::process::exit(2);
            }
            other => {
                // Positional arguments are not expected for checkrelays
                warn!("Unexpected positional argument: {}", other);
                std::process::exit(2);
            }
        }
    }

    // Require passphrase so we can derive our id and access blockchain if needed
    let pass = match passphrase {
        Some(p) if !p.is_empty() => p,
        _ => {
            warn!("--passphrase is required");
            std::process::exit(2);
        }
    };

    // Load node file config if provided
    let (algo_network, algo_provider_config, node_app_id, node_asset_id) =
        if let Some(nf) = node_file.clone() {
            match parse_node_file_with_ids(&nf) {
                Ok((net, cfg, a, b)) => (net, Some(cfg), a, b),
                Err(e) => {
                    warn!("{}", e);
                    std::process::exit(2);
                }
            }
        } else {
            (None, None, None, None)
        };

    // Resolve app/asset ids (prefer values from node file or CLI; else environment)
    let (app_id, asset_id) =
        match resolve_app_asset_ids(node_app_id, node_asset_id, cli_app_id, cli_asset_id) {
            Ok((a, b)) => (a, b),
            Err(e) => {
                warn!("{}", e);
                std::process::exit(2);
            }
        };

    // Build AlgoOps for discovery using same config
    let ops = AlgoOps::new_for_algorand(Some(pass.clone()), None, algo_provider_config.clone());
    let my_address = match &ops.address {
        Some(a) => a.clone(),
        None => {
            warn!("Unable to derive address from passphrase");
            std::process::exit(2);
        }
    };

    // Construct minimal StartOptions without requiring user-provided handle
    let opts = bingle_core::api::bingle_api::StartOptions {
        handle: my_address.clone(), // synthetic handle: use our address to satisfy Engine
        algo_passphrase: Some(pass.clone()),
        static_ip: None,
        am_relay: false,
        stun_servers,
        algo_provider_config: algo_provider_config.clone(),
        algo_network,
        app_id: Some(app_id),
        asset_id: Some(asset_id),
        log_level: None,
        handle_cache_expiry: None,
        dangerous_debug,
        log_mode: LogMode::Plain,
        wait_response_timeout: None,
    };

    // Create API and start engine minimal
    let api = BingleApiImpl::new(&opts);
    loop {
        let mut start_res = Ok(());
        api.access(|api_mut| {
            start_res = api_mut.start(&opts);
        });

        match start_res {
            Ok(_) => break,
            Err(e) => {
                if let BingleError::Algo(ae) = &e
                    && ae.kind == AlgoErrorKind::HostUnreachable
                {
                    tracing::error!("Algorand node unreachable: {}. Retrying in 60s...", ae);
                    std::thread::sleep(Duration::from_secs(60));
                    continue;
                }
                warn!("Failed to start API: {}", e);
                std::process::exit(2);
            }
        }
    }

    // After starting, wait for Engine to reach Registered state to ensure discovery is ready
    let api_both: Arc<dyn bingle_core::api::bingle_api::BingleApiBoth> = api.clone();
    let wait_start = Instant::now();
    let wait_timeout = Duration::from_secs(5);
    while wait_start.elapsed() < wait_timeout {
        let st = api_both.get_state();
        if st == bingle_core::engine::EngineState::Registered {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Helper for a single check that returns Ok(duration_ms) or Err(()) using Ping/PingResponse
    let do_check = |relay_id: &str, addr: SocketAddr| -> Result<u128, ()> {
        let nsk = NetworkEndpoint::new_direct(addr);
        // Send a Ping with text and expect a PingResponse with ACK text
        let req = json!({ "app": "ping", "type": "ping", "text": "bingle-cli probe" });
        let start = Instant::now();
        match api.send_message_to_network_with_response(&nsk, &relay_id.to_string(), req, None) {
            Ok(resp) => {
                let app_ok = resp.get("app").and_then(|v| v.as_str()) == Some("ping");
                let type_ok = resp.get("type").and_then(|v| v.as_str()) == Some("response");
                let text_ok = resp
                    .get("text")
                    .and_then(|v| v.as_str())
                    .map(|s| s.starts_with("ACK:"))
                    .unwrap_or(false);
                if app_ok && type_ok && text_ok {
                    Ok(start.elapsed().as_millis())
                } else {
                    Err(())
                }
            }
            Err(_) => Err(()),
        }
    };

    // Repeat loop (unless --once)
    loop {
        let relays = api.list_all_relays(false);

        if relays.is_empty() {
            tracing::warn!("No relays discovered; nothing to check");
        } else {
            for r in &relays {
                let mut ok: u32 = 0;
                let mut fail: u32 = 0;
                let mut times: Vec<u128> = Vec::new();
                for _ in 0..5 {
                    match do_check(r.id(), r.address()) {
                        Ok(ms) => {
                            ok += 1;
                            times.push(ms);
                        }
                        Err(()) => {
                            fail += 1;
                        }
                    }
                }
                times.sort();
                let p50 = times
                    .get((times.len().saturating_sub(1)) / 2)
                    .copied()
                    .unwrap_or(0);
                let p95 = if !times.is_empty() {
                    let idx = ((times.len() as f64) * 0.95).ceil() as usize - 1;
                    *times.get(idx.min(times.len() - 1)).unwrap_or(&0)
                } else {
                    0
                };
                let avg = if !times.is_empty() {
                    times.iter().sum::<u128>() as f64 / times.len() as f64
                } else {
                    0.0
                };
                let rate = if ok + fail > 0 {
                    fail as f64 / (ok + fail) as f64
                } else {
                    1.0
                };
                println!(
                    "relay {} @ {} -> ok={} fail={} fail_rate={:.0}% avg={:.1}ms p50={}ms p95={}ms",
                    r.id(),
                    r.address(),
                    ok,
                    fail,
                    rate * 100.0,
                    avg,
                    p50,
                    p95
                );
            }
        }

        if once {
            break;
        }
        std::thread::sleep(Duration::from_millis(interval_ms));
    }

    // Stop API before exit
    api.access(|api_mut| api_mut.stop());
}

fn cmd_register(args: Vec<String>) {
    // Simple manual parsing to keep dependencies minimal
    let mut it = args.into_iter();
    let mut handle: Option<String> = None;
    let mut app_id: Option<u64> = None;
    let mut asset_id: Option<u64> = None;
    let mut price_units: Option<u64> = None;
    let mut node_file: Option<String> = None;
    let mut passphrase: Option<String> = None;

    if it.clone().any(|a| a == "--help" || a == "-h") {
        println!(
            "Usage: bingle_cli register --handle <handle> --passphrase <text> --app-id <id> --asset-id <id> --price-units <n> [--node-file <file>] [--stun-servers <list>] [--stun-servers-file <file>]"
        );
        std::process::exit(0);
    }

    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--handle" => {
                handle = Some(req_value(&mut it, "--handle"));
            }
            "--passphrase" => {
                passphrase = Some(req_value(&mut it, "--passphrase"));
            }
            "--app-id" => {
                app_id = Some(parse_u64(req_value(&mut it, "--app-id"), "--app-id"));
            }
            "--asset-id" => {
                asset_id = Some(parse_u64(req_value(&mut it, "--asset-id"), "--asset-id"));
            }
            "--price-units" => {
                price_units = Some(parse_u64(
                    req_value(&mut it, "--price-units"),
                    "--price-units",
                ));
            }
            "--node-file" => {
                node_file = Some(req_value(&mut it, "--node-file"));
            }
            // Accept common STUN options for consistency across commands; ignored for register
            "--stun-servers" => {
                let _ = req_value(&mut it, "--stun-servers");
            }
            "--stun-servers-file" => {
                let _ = req_value(&mut it, "--stun-servers-file");
            }
            s => {
                warn!("Unknown option for register: {}", s);
                std::process::exit(2);
            }
        }
    }

    let handle = match handle {
        Some(h) => h,
        None => {
            warn!("register requires --handle <handle>");
            std::process::exit(2);
        }
    };
    let passphrase = match passphrase {
        Some(p) => p,
        None => {
            warn!("register requires --passphrase <text>");
            std::process::exit(2);
        }
    };
    let price_units = match price_units {
        Some(v) => v,
        None => {
            warn!("register requires --price-units <n>");
            std::process::exit(2);
        }
    };

    // Load node file (may also contain app_id/asset_id) and build config
    let (cfg, node_app_id, node_asset_id): (AlgoChainConfig, Option<u64>, Option<u64>) =
        match node_file {
            Some(path) => match parse_node_file_with_ids(&path) {
                Ok((_net, cfg, nid_app, nid_asset)) => (cfg, nid_app, nid_asset),
                Err(e) => {
                    warn!("{}", e);
                    std::process::exit(2);
                }
            },
            None => (AlgoChainConfig::default(), None, None),
        };

    // Resolve IDs with precedence: node file > CLI > env; error if node+CLI both set
    let (app_id, asset_id) =
        match resolve_app_asset_ids(node_app_id, node_asset_id, app_id, asset_id) {
            Ok(v) => v,
            Err(e) => {
                warn!("{}", e);
                std::process::exit(2);
            }
        };

    // Build AlgoOps with provided passphrase; address is derived immediately in AlgoOps::new
    let ops = AlgoOps::new_for_algorand(Some(passphrase.clone()), None, Some(cfg));
    let address = match ops.address.as_ref() {
        Some(a) => a.clone(),
        None => {
            warn!(
                "Invalid passphrase: unable to derive address. Provide a valid Algorand mnemonic or supported secret format."
            );
            std::process::exit(2);
        }
    };

    // Ensure the account is funded
    let bal_algos = loop {
        match ops.account_balance() {
            Ok(Some(b)) => break b,
            Ok(None) => {
                warn!(
                    "Account {} not found or balance unavailable. Please ensure the account exists and is funded.",
                    address
                );
                std::process::exit(2);
            }
            Err(e) => {
                if let Some(ae) = e.downcast_ref::<AlgoError>()
                    && ae.kind == AlgoErrorKind::HostUnreachable
                {
                    tracing::error!("Algorand node unreachable: {}. Retrying in 60s...", ae);
                    std::thread::sleep(Duration::from_secs(60));
                    continue;
                }
                warn!("Failed to query account balance: {}", e);
                std::process::exit(2);
            }
        }
    };
    if bal_algos <= 0.0 {
        warn!(
            "Account {} has zero balance. Please fund it and retry.",
            address
        );
        std::process::exit(2);
    }
    tracing::info!(
        "Using funded account {} (balance {:.6} ALGO)",
        address,
        bal_algos
    );

    // Register the handle on-chain
    let bingle = AlgoBingle::new(ops.clone(), app_id, asset_id);
    loop {
        match bingle.register(app_id, asset_id, &handle, price_units) {
            Ok(txid) => {
                tracing::info!(
                    "Successfully registered handle '{}' for {} (tx: {})",
                    handle,
                    address,
                    txid
                );
                break;
            }
            Err(e) => {
                if let Some(ae) = e.downcast_ref::<AlgoError>()
                    && ae.kind == AlgoErrorKind::HostUnreachable
                {
                    tracing::error!("Algorand node unreachable: {}. Retrying in 60s...", ae);
                    std::thread::sleep(Duration::from_secs(60));
                    continue;
                }
                warn!("Failed to register handle: {}", e);
                std::process::exit(1);
            }
        }
    }
}

fn cmd_buybingle(args: Vec<String>) {
    // Usage help: an explicit --help prints to stdout and exits 0; missing args is an error to stderr.
    const USAGE: &str = "Usage: bingle_cli buybingle <price_algos> --passphrase <text> --app-id <id> --asset-id <id> [--node-file <file>] [--stun-servers <list>] [--stun-servers-file <file>]";
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        std::process::exit(0);
    }
    if args.is_empty() {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }

    let mut it = args.into_iter();
    // First positional: price in ALGOs (decimal), convert to microAlgos
    let price_str = it.next().expect("checked non-empty");
    let price_micro = match parse_algos_decimal_to_microalgos(&price_str) {
        Ok(v) => v,
        Err(e) => {
            warn!("Invalid <price_algos> '{}': {}", price_str, e);
            std::process::exit(2);
        }
    };

    let mut app_id: Option<u64> = None;
    let mut asset_id: Option<u64> = None;
    let mut node_file: Option<String> = None;
    let mut passphrase: Option<String> = None;

    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--app-id" => {
                app_id = Some(parse_u64(req_value(&mut it, "--app-id"), "--app-id"));
            }
            "--asset-id" => {
                asset_id = Some(parse_u64(req_value(&mut it, "--asset-id"), "--asset-id"));
            }
            "--node-file" => {
                node_file = Some(req_value(&mut it, "--node-file"));
            }
            "--passphrase" => {
                passphrase = Some(req_value(&mut it, "--passphrase"));
            }
            // Accept common STUN options for consistency across commands; ignored for buybingle
            "--stun-servers" => {
                let _ = req_value(&mut it, "--stun-servers");
            }
            "--stun-servers-file" => {
                let _ = req_value(&mut it, "--stun-servers-file");
            }
            other => {
                warn!("Unknown option for buybingle: {}", other);
                std::process::exit(2);
            }
        }
    }

    let passphrase = match passphrase {
        Some(p) => p,
        None => {
            warn!("buybingle requires --passphrase <text>");
            std::process::exit(2);
        }
    };

    // Load node file (may also contain app_id/asset_id)
    let (cfg, node_app_id, node_asset_id): (AlgoChainConfig, Option<u64>, Option<u64>) =
        match node_file {
            Some(path) => match parse_node_file_with_ids(&path) {
                Ok((_net, cfg, nid_app, nid_asset)) => (cfg, nid_app, nid_asset),
                Err(e) => {
                    warn!("{}", e);
                    std::process::exit(2);
                }
            },
            None => (AlgoChainConfig::default(), None, None),
        };

    // Resolve IDs with precedence
    let (app_id, asset_id) =
        match resolve_app_asset_ids(node_app_id, node_asset_id, app_id, asset_id) {
            Ok(v) => v,
            Err(e) => {
                warn!("{}", e);
                std::process::exit(2);
            }
        };

    // Ops
    let ops = AlgoOps::new_for_algorand(Some(passphrase.clone()), None, Some(cfg));
    let address = ops.address.as_ref().cloned().unwrap_or_else(|| {
        warn!("Invalid passphrase: unable to derive address.");
        std::process::exit(2);
    });
    let bal_algos = loop {
        match ops.account_balance() {
            Ok(Some(b)) => break b,
            Ok(None) => break 0.0,
            Err(e) => {
                if let Some(ae) = e.downcast_ref::<AlgoError>()
                    && ae.kind == AlgoErrorKind::HostUnreachable
                {
                    tracing::error!("Algorand node unreachable: {}. Retrying in 60s...", ae);
                    std::thread::sleep(Duration::from_secs(60));
                    continue;
                }
                break 0.0;
            }
        }
    };
    if bal_algos <= 0.0 {
        warn!(
            "Account {} has zero/unavailable balance. Please fund it and retry.",
            address
        );
        std::process::exit(2);
    }
    tracing::info!("Using account {} (balance {:.6} ALGO)", address, bal_algos);

    let bingle = AlgoBingle::new(ops.clone(), app_id, asset_id);
    loop {
        match bingle.buy_bingle(app_id, asset_id, price_micro) {
            Ok(txid) => {
                tracing::info!("buybingle submitted (tx: {})", txid);
                break;
            }
            Err(e) => {
                if let Some(ae) = e.downcast_ref::<AlgoError>()
                    && ae.kind == AlgoErrorKind::HostUnreachable
                {
                    tracing::error!("Algorand node unreachable: {}. Retrying in 60s...", ae);
                    std::thread::sleep(Duration::from_secs(60));
                    continue;
                }
                warn!("buybingle failed: {}", e);
                std::process::exit(1);
            }
        }
    }
}

fn cmd_sellbingle(args: Vec<String>) {
    // Usage help: an explicit --help prints to stdout and exits 0; missing args is an error to stderr.
    const USAGE: &str = "Usage: bingle_cli sellbingle <amount_units> <price_algos> --passphrase <text> --app-id <id> --asset-id <id> [--node-file <file>] [--stun-servers <list>] [--stun-servers-file <file>]";
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        std::process::exit(0);
    }
    if args.is_empty() {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }

    let mut it = args.into_iter();
    let amount_str = it.next().expect("checked non-empty");
    let amount_units = match amount_str.parse::<u64>() {
        Ok(v) => v,
        Err(e) => {
            warn!("Invalid <amount_units> '{}': {}", amount_str, e);
            std::process::exit(2);
        }
    };
    let price_str = match it.next() {
        Some(s) => s,
        None => {
            warn!("sellbingle requires <price_algos> positional argument");
            std::process::exit(2);
        }
    };
    let price_micro = match parse_algos_decimal_to_microalgos(&price_str) {
        Ok(v) => v,
        Err(e) => {
            warn!("Invalid <price_algos> '{}': {}", price_str, e);
            std::process::exit(2);
        }
    };

    let mut app_id: Option<u64> = None;
    let mut asset_id: Option<u64> = None;
    let mut node_file: Option<String> = None;
    let mut passphrase: Option<String> = None;

    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--app-id" => {
                app_id = Some(parse_u64(req_value(&mut it, "--app-id"), "--app-id"));
            }
            "--asset-id" => {
                asset_id = Some(parse_u64(req_value(&mut it, "--asset-id"), "--asset-id"));
            }
            "--node-file" => {
                node_file = Some(req_value(&mut it, "--node-file"));
            }
            "--passphrase" => {
                passphrase = Some(req_value(&mut it, "--passphrase"));
            }
            // Accept common STUN options for consistency across commands; ignored for sellbingle
            "--stun-servers" => {
                let _ = req_value(&mut it, "--stun-servers");
            }
            "--stun-servers-file" => {
                let _ = req_value(&mut it, "--stun-servers-file");
            }
            other => {
                warn!("Unknown option for sellbingle: {}", other);
                std::process::exit(2);
            }
        }
    }

    let passphrase = match passphrase {
        Some(p) => p,
        None => {
            warn!("sellbingle requires --passphrase <text>");
            std::process::exit(2);
        }
    };

    // Load node file (may also contain app_id/asset_id)
    let (cfg, node_app_id, node_asset_id): (AlgoChainConfig, Option<u64>, Option<u64>) =
        match node_file {
            Some(path) => match parse_node_file_with_ids(&path) {
                Ok((_net, cfg, nid_app, nid_asset)) => (cfg, nid_app, nid_asset),
                Err(e) => {
                    warn!("{}", e);
                    std::process::exit(2);
                }
            },
            None => (AlgoChainConfig::default(), None, None),
        };

    // Resolve IDs with precedence
    let (app_id, asset_id) =
        match resolve_app_asset_ids(node_app_id, node_asset_id, app_id, asset_id) {
            Ok(v) => v,
            Err(e) => {
                warn!("{}", e);
                std::process::exit(2);
            }
        };

    // Ops
    let ops = AlgoOps::new_for_algorand(Some(passphrase.clone()), None, Some(cfg));
    let address = ops.address.as_ref().cloned().unwrap_or_else(|| {
        warn!("Invalid passphrase: unable to derive address.");
        std::process::exit(2);
    });
    let bal_algos = loop {
        match ops.account_balance() {
            Ok(Some(b)) => break b,
            Ok(None) => break 0.0,
            Err(e) => {
                if let Some(ae) = e.downcast_ref::<AlgoError>()
                    && ae.kind == AlgoErrorKind::HostUnreachable
                {
                    tracing::error!("Algorand node unreachable: {}. Retrying in 60s...", ae);
                    std::thread::sleep(Duration::from_secs(60));
                    continue;
                }
                break 0.0;
            }
        }
    };
    if bal_algos <= 0.0 {
        warn!(
            "Account {} has zero/unavailable balance. Please fund it and retry.",
            address
        );
        std::process::exit(2);
    }
    tracing::info!("Using account {} (balance {:.6} ALGO)", address, bal_algos);

    let bingle = AlgoBingle::new(ops.clone(), app_id, asset_id);
    loop {
        match bingle.sell_bingle(app_id, asset_id, amount_units, price_micro) {
            Ok(txid) => {
                tracing::info!("sellbingle submitted (tx: {})", txid);
                break;
            }
            Err(e) => {
                if let Some(ae) = e.downcast_ref::<AlgoError>()
                    && ae.kind == AlgoErrorKind::HostUnreachable
                {
                    tracing::error!("Algorand node unreachable: {}. Retrying in 60s...", ae);
                    std::thread::sleep(Duration::from_secs(60));
                    continue;
                }
                warn!("sellbingle failed: {}", e);
                std::process::exit(1);
            }
        }
    }
}

fn req_value(it: &mut impl Iterator<Item = String>, name: &str) -> String {
    match it.next() {
        Some(v) => v,
        None => {
            warn!("{} requires a value", name);
            std::process::exit(2);
        }
    }
}

fn parse_u64(v: String, name: &str) -> u64 {
    match v.parse::<u64>() {
        Ok(n) => n,
        Err(e) => {
            warn!("Invalid value for {} '{}': {}", name, v, e);
            std::process::exit(2);
        }
    }
}

#[allow(dead_code)]
fn write_text_file(path: &str, content: &str) -> std::io::Result<()> {
    let mut f = fs::File::create(path)?;
    f.write_all(content.as_bytes())
}

/// Outcome of the pre-start check that the configured app is still supported (not superseded
/// by a newer app). Kept as a pure decision so it can be unit-tested without a live node.
#[derive(Debug, PartialEq, Eq)]
enum AppSupport {
    /// No app_id was configured; there is nothing to check.
    NoAppId,
    /// The app is current and clients may run against it.
    Supported,
    /// The app has been superseded by `successor`; the client must upgrade.
    Superseded { app_id: u64, successor: u64 },
}

/// Decide whether the configured app is still supported, given the app_id (if any) and the
/// successor pointer read from chain (`None` when the app is not superseded).
fn resolve_app_support(app_id: Option<u64>, successor_app: Option<u64>) -> AppSupport {
    match app_id {
        None => AppSupport::NoAppId,
        Some(app_id) => match successor_app {
            Some(successor) => AppSupport::Superseded { app_id, successor },
            None => AppSupport::Supported,
        },
    }
}

/// Describes the action to take on shutdown regarding static endpoint unregistration.
#[derive(Debug, PartialEq, Eq)]
// `Unregister` carries an `AlgoChainConfig` (~200 bytes since algo_ops 0.7.5 added the
// optional throttle/budget fields); this is a cold, single-use CLI action descriptor, so
// the size difference across variants is irrelevant. Box the field only if it ever gets hot.
#[allow(clippy::large_enum_variant)]
enum ShutdownAction {
    /// No static IP was configured; nothing to unregister.
    NoStaticIp,
    /// Static IP was configured but no app_id is available.
    NoAppId,
    /// Static IP and app_id are available but no passphrase.
    NoPassphrase,
    /// All parameters are available; unregister the endpoint.
    Unregister {
        app_id: u64,
        passphrase: String,
        algo_provider_config: Option<AlgoChainConfig>,
        asset_id: Option<u64>,
    },
}

/// Determine the shutdown action based on the current StartOptions.
/// The app_id_env parameter allows injecting the APP_ID environment variable for testability.
fn resolve_shutdown_action(
    opts: &bingle_core::api::bingle_api::StartOptions,
    app_id_env: Option<u64>,
) -> ShutdownAction {
    if opts.static_ip.is_none() {
        return ShutdownAction::NoStaticIp;
    }
    let app_id_opt = opts.app_id.or(app_id_env);
    match app_id_opt {
        None => ShutdownAction::NoAppId,
        Some(app_id) => match opts.algo_passphrase {
            Some(ref passphrase) => ShutdownAction::Unregister {
                app_id,
                passphrase: passphrase.clone(),
                algo_provider_config: opts.algo_provider_config.clone(),
                asset_id: opts.asset_id,
            },
            None => ShutdownAction::NoPassphrase,
        },
    }
}

fn install_ctrlc_handler(tx: Sender<()>) {
    if let Err(e) = ctrlc::set_handler(move || {
        tracing::info!(
            "Received shutdown signal (SIGINT/SIGTERM); sending message to main thread to exit"
        );
        let _ = tx.send(());
    }) {
        warn!("Failed to install signal handler: {}", e);
    }
}
