use bingle_core::api::bingle_api::{
    BingleApi, Handle, NetworkEndpoint, ProgressCallback, StartOptions, UserId,
};
use bingle_core::ddb::AdvertRecord;
use bingle_core::engine::Engine;
use std::sync::Arc;

#[path = "../test_util.rs"]
pub mod test_util;

struct DummyApi {
    pub app_id: Option<u64>,
    pub config: Option<algo_ops::AlgoChainConfig>,
}

impl BingleApi for DummyApi {
    fn list_all_relays(
        &self,
        _include_self: bool,
    ) -> Vec<bingle_core::relay::relay_finder::RelayInfo> {
        Vec::new()
    }
    fn set_on_listening(
        &self,
        _handler: Option<std::sync::Arc<bingle_core::api::bingle_api::OnListeningHandler>>,
    ) {
    }
    fn get_handle(&self) -> Option<String> {
        None
    }
    fn get_user_id(&self) -> Option<String> {
        None
    }
    fn debug_print_options(&self) {}
    fn get_my_id(&self) -> Option<String> {
        None
    }
    fn get_app_id(&self) -> Option<u64> {
        self.app_id
    }
    fn get_algo_provider_config(&self) -> Option<algo_ops::AlgoChainConfig> {
        self.config.clone()
    }
    fn get_accounts_cache(
        &self,
    ) -> Option<Arc<std::sync::Mutex<bingle_core::blockchain::algo_bingle::AccountsCache>>> {
        None
    }
    fn clear_accounts_cache(&self) {}
    fn start(
        &self,
        _options: &StartOptions,
    ) -> Result<(), bingle_core::api::bingle_api::BingleError> {
        Ok(())
    }
    fn stop(&self) {}
    fn network_change(&self) {}
    fn handle_lookup(
        &self,
        _handle: &Handle,
    ) -> Result<Option<UserId>, bingle_core::api::bingle_api::BingleError> {
        Ok(None)
    }
    fn handle_lookup_partial(
        &self,
        _handle: &Handle,
    ) -> Result<Option<(UserId, Handle)>, bingle_core::api::bingle_api::BingleError> {
        Ok(None)
    }
    fn handle_lookup_by_id(&self, _user_id: &UserId) -> Option<Handle> {
        None
    }
    fn send_message_to_id(
        &self,
        _user_id: &UserId,
        _message: serde_json::Value,
        _progress: Option<Arc<ProgressCallback>>,
    ) -> Result<bool, bingle_core::api::bingle_api::BingleError> {
        Ok(false)
    }
    fn send_message_to_handle(
        &self,
        _handle: &Handle,
        _message: serde_json::Value,
        _progress: Option<Arc<ProgressCallback>>,
    ) -> Result<bool, bingle_core::api::bingle_api::BingleError> {
        Ok(false)
    }
    fn send_message_to_network(
        &self,
        _nsk: &NetworkEndpoint,
        _user_id: &UserId,
        _message: serde_json::Value,
        _progress: Option<Arc<ProgressCallback>>,
    ) -> Result<bool, bingle_core::api::bingle_api::BingleError> {
        Ok(false)
    }
    fn send_message_to_id_with_response(
        &self,
        _user_id: &UserId,
        _message: serde_json::Value,
        _progress: Option<Arc<ProgressCallback>>,
    ) -> Result<serde_json::Value, bingle_core::api::bingle_api::BingleError> {
        Err(bingle_core::api::bingle_api::BingleError::Other(
            "not implemented".into(),
        ))
    }
    fn send_message_to_handle_with_response(
        &self,
        _handle: &Handle,
        _message: serde_json::Value,
        _progress: Option<Arc<ProgressCallback>>,
    ) -> Result<serde_json::Value, bingle_core::api::bingle_api::BingleError> {
        Err(bingle_core::api::bingle_api::BingleError::Other(
            "not implemented".into(),
        ))
    }
    fn send_message_to_network_with_response(
        &self,
        _nsk: &NetworkEndpoint,
        _user_id: &UserId,
        _message: serde_json::Value,
        _progress: Option<Arc<ProgressCallback>>,
    ) -> Result<serde_json::Value, bingle_core::api::bingle_api::BingleError> {
        Err(bingle_core::api::bingle_api::BingleError::Other(
            "not implemented".into(),
        ))
    }
    fn set_on_message(
        &self,
        _handler: Option<Arc<bingle_core::api::bingle_api::OnMessageHandler>>,
    ) {
    }
    fn set_on_connect(
        &self,
        _handler: Option<Arc<bingle_core::api::bingle_api::OnConnectHandler>>,
    ) {
    }
}

impl bingle_core::api::bingle_api::BingleApiInternal for DummyApi {
    fn get_relay_state(&self) -> String {
        "off".to_string()
    }
}

#[test]
#[cfg(not(target_os = "ios"))]
pub fn test_ddb_upsert_relay_requires_blockchain_info() {
    let api = DummyApi {
        app_id: None,
        config: None,
    };
    let engine = Engine::new(
        &StartOptions::new("".into()),
        crate::util::mock_bingle_api::to_weak(api),
    );

    // Create a valid signed record with am_relay = true
    let id = crate::util::test_util::ADDRESS_SPEND.to_string();
    let passphrase = crate::util::test_util::PASSPHRASE_SPEND;

    let seed = algonaut::crypto::mnemonic::to_key(passphrase).expect("valid mnemonic");
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&seed.try_into().expect("32 bytes"));

    let record = AdvertRecord::new(
        id.clone(),
        None,
        Some(true),
        None,
        None,
        "2025-01-01T00:00:00Z".into(),
        &signing_key,
    );

    engine.ddb_upsert_record(record);

    let found = engine.ddb_backend_lookup_for_tests(&id);
    assert!(
        found.is_none(),
        "Relay record should be rejected when blockchain info is missing"
    );
}

#[test]
#[cfg(not(target_os = "ios"))]
pub fn test_ddb_upsert_relay_fails_on_blockchain_error() {
    // Provide a config that points to a non-existent port
    let config = algo_ops::AlgoChainConfig {
        client_api_url: "http://localhost".to_string(),
        client_api_port: 1234, // Hopefully unused
        indexer_api_url: "http://localhost".to_string(),
        indexer_api_port: 1235,
        token: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()),
        token_key: Some("X-Algo-API-Token".to_string()),
        app_id: None,
        asset_id: None,
        ..Default::default()
    };

    let api = DummyApi {
        app_id: Some(123),
        config: Some(config),
    };
    let engine = Engine::new(
        &StartOptions::new("".into()),
        crate::util::mock_bingle_api::to_weak(api),
    );

    let id = crate::util::test_util::ADDRESS_SPEND.to_string();
    let passphrase = crate::util::test_util::PASSPHRASE_SPEND;

    let seed = algonaut::crypto::mnemonic::to_key(passphrase).expect("valid mnemonic");
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&seed.try_into().expect("32 bytes"));

    let record = AdvertRecord::new(
        id.clone(),
        None,
        Some(true),
        None,
        None,
        "2025-01-01T00:00:00Z".into(),
        &signing_key,
    );

    engine.ddb_upsert_record(record);

    let found = engine.ddb_backend_lookup_for_tests(&id);
    assert!(
        found.is_none(),
        "Relay record should be rejected when blockchain is unreachable (fail-safe)"
    );
}

#[test]
#[cfg(not(target_os = "ios"))]
pub fn test_ddb_upsert_non_relay_always_accepted() {
    let api = DummyApi {
        app_id: None,
        config: None,
    };
    let engine = Engine::new(
        &StartOptions::new("".into()),
        crate::util::mock_bingle_api::to_weak(api),
    );

    let id = crate::util::test_util::ADDRESS_SPEND.to_string();
    let passphrase = crate::util::test_util::PASSPHRASE_SPEND;

    let seed = algonaut::crypto::mnemonic::to_key(passphrase).expect("valid mnemonic");
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&seed.try_into().expect("32 bytes"));

    // am_relay = false
    let record = AdvertRecord::new(
        id.clone(),
        None,
        Some(false),
        None,
        None,
        "2025-01-01T00:00:00Z".into(),
        &signing_key,
    );

    engine.ddb_upsert_record(record);

    let found = engine.ddb_backend_lookup_for_tests(&id);
    assert!(
        found.is_some(),
        "Non-relay record should be accepted even without blockchain info"
    );
}
