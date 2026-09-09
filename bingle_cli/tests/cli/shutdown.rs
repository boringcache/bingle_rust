use algo_ops::AlgoChainConfig;
use bingle_core::api::bingle_api::StartOptions;
use std::net::SocketAddr;

/// Describes the action to take on shutdown regarding static endpoint unregistration.
/// Mirrors the ShutdownAction enum in bingle_cli.rs for testability.
#[derive(Debug, PartialEq, Eq)]
// Cold CLI action descriptor; see the note on the mirrored enum in main.rs.
#[allow(clippy::large_enum_variant)]
enum ShutdownAction {
    NoStaticIp,
    NoAppId,
    NoPassphrase,
    Unregister {
        app_id: u64,
        passphrase: String,
        algo_provider_config: Option<AlgoChainConfig>,
        asset_id: Option<u64>,
    },
}

/// Mirrors resolve_shutdown_action from bingle_cli.rs.
/// The app_id_env parameter allows injecting the APP_ID environment variable for testability.
fn resolve_shutdown_action(opts: &StartOptions, app_id_env: Option<u64>) -> ShutdownAction {
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

fn make_opts_with_static_ip(addr: &str) -> StartOptions {
    let mut opts = StartOptions::new("".into());
    opts.static_ip = Some(addr.parse::<SocketAddr>().expect("valid socket addr"));
    opts
}

#[test]
#[cfg(not(target_os = "ios"))]
pub fn no_static_ip_returns_no_static_ip() {
    let opts = StartOptions::new("".into());
    let action = resolve_shutdown_action(&opts, None);
    assert_eq!(action, ShutdownAction::NoStaticIp);
}

#[test]
#[cfg(not(target_os = "ios"))]
pub fn static_ip_without_app_id_or_env_returns_no_app_id() {
    let opts = make_opts_with_static_ip("1.2.3.4:5000");
    let action = resolve_shutdown_action(&opts, None);
    assert_eq!(action, ShutdownAction::NoAppId);
}

#[test]
#[cfg(not(target_os = "ios"))]
pub fn static_ip_with_app_id_but_no_passphrase_returns_no_passphrase() {
    let mut opts = make_opts_with_static_ip("1.2.3.4:5000");
    opts.app_id = Some(12345);
    let action = resolve_shutdown_action(&opts, None);
    assert_eq!(action, ShutdownAction::NoPassphrase);
}

#[test]
#[cfg(not(target_os = "ios"))]
pub fn static_ip_with_env_app_id_but_no_passphrase_returns_no_passphrase() {
    let opts = make_opts_with_static_ip("1.2.3.4:5000");
    let action = resolve_shutdown_action(&opts, Some(99999));
    assert_eq!(action, ShutdownAction::NoPassphrase);
}

#[test]
#[cfg(not(target_os = "ios"))]
pub fn static_ip_with_app_id_and_passphrase_returns_unregister() {
    let mut opts = make_opts_with_static_ip("44.223.62.108:12121");
    opts.app_id = Some(757297220);
    opts.algo_passphrase = Some("test passphrase".to_string());
    let action = resolve_shutdown_action(&opts, None);
    assert_eq!(
        action,
        ShutdownAction::Unregister {
            app_id: 757297220,
            passphrase: "test passphrase".to_string(),
            algo_provider_config: None,
            asset_id: None,
        }
    );
}

#[test]
#[cfg(not(target_os = "ios"))]
pub fn env_app_id_used_when_opts_app_id_is_none() {
    let mut opts = make_opts_with_static_ip("10.0.0.1:8080");
    opts.algo_passphrase = Some("my secret".to_string());
    // opts.app_id is None, but env provides it
    let action = resolve_shutdown_action(&opts, Some(42));
    assert_eq!(
        action,
        ShutdownAction::Unregister {
            app_id: 42,
            passphrase: "my secret".to_string(),
            algo_provider_config: None,
            asset_id: None,
        }
    );
}

#[test]
#[cfg(not(target_os = "ios"))]
pub fn opts_app_id_takes_precedence_over_env() {
    let mut opts = make_opts_with_static_ip("10.0.0.1:8080");
    opts.app_id = Some(100);
    opts.algo_passphrase = Some("pw".to_string());
    let action = resolve_shutdown_action(&opts, Some(999));
    assert_eq!(
        action,
        ShutdownAction::Unregister {
            app_id: 100,
            passphrase: "pw".to_string(),
            algo_provider_config: None,
            asset_id: None,
        }
    );
}

#[test]
#[cfg(not(target_os = "ios"))]
pub fn unregister_includes_provider_config_and_asset_id() {
    let mut opts = make_opts_with_static_ip("10.0.0.1:8080");
    opts.app_id = Some(100);
    opts.asset_id = Some(200);
    opts.algo_passphrase = Some("pw".to_string());
    let config = AlgoChainConfig {
        client_api_url: "https://example.com".to_string(),
        client_api_port: 443,
        indexer_api_url: "https://idx.example.com".to_string(),
        indexer_api_port: 443,
        token: Some("tok".to_string()),
        token_key: None,
        app_id: Some(100),
        asset_id: Some(200),
        ..Default::default()
    };
    opts.algo_provider_config = Some(config.clone());
    let action = resolve_shutdown_action(&opts, None);
    assert_eq!(
        action,
        ShutdownAction::Unregister {
            app_id: 100,
            passphrase: "pw".to_string(),
            algo_provider_config: Some(config),
            asset_id: Some(200),
        }
    );
}
