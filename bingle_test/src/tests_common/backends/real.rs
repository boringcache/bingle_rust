// #![cfg(not(target_os = "ios"))]

use crate::tests_common::TestAlgo;
use algo_ops::{self, AlgoOps};

fn err<E: std::fmt::Display>(e: E) -> String {
    format!("{}", e)
}

pub struct RealBackend {
    inner: AlgoOps,
}

impl TestAlgo for RealBackend {
    fn new(passphrase: Option<String>, address: Option<String>, use_dummy_config: bool) -> Self {
        let cfg = if use_dummy_config {
            Some(algo_ops::AlgoChainConfig {
                client_api_url: "http://nowhere.not".to_string(),
                client_api_port: 666,
                indexer_api_url: "http://nowhere.not".to_string(),
                indexer_api_port: 666,
                token: Some("a".repeat(64)),
                token_key: Some("X-API-Key".to_string()),
                app_id: None,
                asset_id: None,
                ..Default::default()
            })
        } else {
            None
        };
        let inner = AlgoOps::new_for_algorand(passphrase, address, cfg);
        Self { inner }
    }

    fn create_address(&mut self, _save: bool, _always_new_address: bool) -> Result<String, String> {
        self.inner.create_address().map_err(err)
    }

    fn public_key_bytes(&self) -> Result<[u8; 32], String> {
        self.inner.public_key_bytes().map_err(err)
    }

    fn private_key_bytes(&self) -> Result<Vec<u8>, String> {
        self.inner.private_key_bytes().map_err(err)
    }

    fn contract_address(&self, app_id: u64) -> Result<String, String> {
        self.inner.contract_address(app_id).map_err(err)
    }

    fn sign(&self, text: &str) -> Result<String, String> {
        self.inner.sign(text).map_err(err)
    }

    fn verify(&self, text: &str, sig_b64: &str) -> Result<bool, String> {
        self.inner.verify(text, sig_b64).map_err(err)
    }

    fn global_state(
        &self,
        maybe_app_id: Option<u64>,
    ) -> Result<Option<Vec<(u64, Vec<(String, String)>)>>, String> {
        self.inner.global_state(maybe_app_id).map_err(err)
    }

    fn local_state_for_account(
        &self,
        app_id: u64,
        account_address: &str,
    ) -> Result<Option<Vec<(String, String)>>, String> {
        self.inner
            .local_state_for_account(app_id, account_address)
            .map_err(err)
    }

    fn send_algo(&self, to_address: &str, amount_algos: f64) -> Result<(), String> {
        self.inner.send_algo(to_address, amount_algos).map_err(err)
    }

    fn create_asset(&self, name: &str, units_in_issue: u64) -> Result<Option<u64>, String> {
        self.inner.create_asset(name, units_in_issue).map_err(err)
    }

    fn opt_in_to_asset(&self, asset_id: u64) -> Result<(), String> {
        self.inner.opt_in_to_asset(asset_id).map_err(err)
    }

    fn addr_from_pk(pk: &[u8; 32]) -> Result<String, String> {
        algo_ops::byte_key_to_address(pk).map_err(err)
    }

    fn pk_from_addr(addr: &str) -> Result<[u8; 32], String> {
        algo_ops::address_to_byte_key(addr).map_err(err)
    }
}
