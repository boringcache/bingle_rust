use anyhow::{Result, anyhow, bail};
use sha2::{Digest, Sha512_256};
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use crate::api::bingle_api::BingleApiBoth;
use algo_ops::error::AlgoError;
use algo_ops::{AccountScanCache, AlgoOps, AppArg, ScannedAccount, address_to_byte_key};

// The incremental opted-in-account scan (paging, cache, min-round watermark) now lives in `algo_ops`
// (`AlgoOps::fetch_opted_in_accounts_cached` over its generic `AccountScanCache`). Re-export
// `QueryMode` from there so callers keep using `bingle_core::blockchain::algo_bingle::QueryMode`.
pub use algo_ops::QueryMode;

use algonaut::{
    Algod,
    core::{Address, AppId, AssetId, ToMsgPack},
    model::algod::SuggestedParams,
    transaction::{
        TransferAsset,
        account::Account,
        builder::{CallApplication, ClawbackAsset, UpdateAsset},
        transaction::Transaction,
    },
};

/// Lightweight helper around AlgoOps for calling the Bingle dApp.
///
/// This module purposefully focuses only on building and submitting the minimal
/// transaction groups required by the Bingle contract methods:
/// - buy_bingle: payment to app address for the configured price, plus an ASA transfer
///   of 1 unit to the caller, and the app call (with foreign asset set to the ASA id).
/// - sell_bingle: ASA transfer of `amount` from caller to app address, a payment to the
///   caller for price*amount (we conservatively self-pay, which satisfies on-chain checks),
///   and the app call.
/// - register: ASA transfer of `price` units from caller to app address, and the app call
///   with the handle argument.
///
/// Note: For simplicity and minimal changes, these helpers assume that all transactions
/// in the group are signed by the same account (self). This still satisfies the on-chain
/// validation logic in the provided contract.
#[derive(Debug, Clone)]
pub struct AlgoBingle {
    /// The generic Algorand helpers used to sign and submit transactions, holding the
    /// signing account and node/indexer configuration.
    pub ops: AlgoOps,
    /// The Bingle application id this instance operates on (0 when not yet resolved).
    pub app_id: u64,
    /// The Bingle$ Algorand standard asset (ASA) id this instance operates on (0 when not
    /// yet resolved).
    pub asset_id: u64,
    /// An optional shared cache of indexer-derived accounts, used to avoid full rescans on
    /// repeated handle and endpoint lookups.
    pub cache: Option<Arc<Mutex<AccountsCache>>>,
}

/// Named account keys for `deploy_app_and_asset`. Each maps to an `AlgoOps` instance
/// whose address and signing key are used for the corresponding deployment role.
pub const ACCOUNT_APP_ADMIN: &str = "APP_ADMIN";
/// Account key for the app withdrawer role in [`AlgoBingle::deploy_app_and_asset`].
pub const ACCOUNT_APP_WITHDRAWER: &str = "APP_WITHDRAWER";
/// Account key for the asset creator role in [`AlgoBingle::deploy_app_and_asset`].
pub const ACCOUNT_ASSET_CREATOR: &str = "ASSET_CREATOR";
/// Account key for the asset reserve role in [`AlgoBingle::deploy_app_and_asset`].
pub const ACCOUNT_ASSET_RESERVE: &str = "ASSET_RESERVE";
/// Account key for the asset manager role in [`AlgoBingle::deploy_app_and_asset`].
pub const ACCOUNT_ASSET_MANAGER: &str = "ASSET_MANAGER";
/// Account key for the asset freeze role in [`AlgoBingle::deploy_app_and_asset`].
pub const ACCOUNT_ASSET_FREEZE: &str = "ASSET_FREEZE";

/// Cached set of indexer-derived accounts opted in to a Bingle app, so handle and endpoint lookups
/// can reuse a prior scan instead of paging the indexer from scratch each time.
///
/// Aliased to `algo_ops`'s generic [`AccountScanCache`] (which owns the paging, freshness, and
/// min-round watermark logic) holding each opted-in account as a decoded [`ScannedAccount`]
/// (address + the account's local state for the app). The caller decodes its own field — a handle,
/// an endpoint — from [`ScannedAccount::local_state`]; the cache itself is schema-agnostic.
pub type AccountsCache = AccountScanCache<ScannedAccount>;

// Algorand minimum-balance and fee schedule, in microalgos (see the developer docs on
// minimum balance). Used by the registration cost model; the app opt-in cost also depends on
// the app's local-state schema, read live via AlgoOps::get_app_local_schema.
const MICROALGOS_PER_ALGO: u64 = 1_000_000;
const MBR_BASE_MICROALGOS: u64 = 100_000; // base account minimum balance
const MBR_ASSET_OPTIN_MICROALGOS: u64 = 100_000; // per ASA opt-in
const MBR_APP_OPTIN_BASE_MICROALGOS: u64 = 100_000; // per app opt-in, before schema
const MBR_APP_UINT_MICROALGOS: u64 = 28_500; // per local uint in the app schema
const MBR_APP_BYTESLICE_MICROALGOS: u64 = 50_000; // per local byte-slice in the app schema
const MIN_TXN_FEE_MICROALGOS: u64 = 1_000; // per transaction
// The register flow is not 4 transactions: opt-in app + opt-in asset, then buy_bingle (opt-in
// sender to asset + a payment/app-call group whose app performs an inner ASA transfer) and
// register (opt-in app-to-asset + opt-in sender + opt-in app + an asset-transfer/app-call group
// with an inner transfer), several opt-ins re-issued idempotently. Under-counting the fees left
// the account below its (asset-raised) minimum balance mid-registration, so the register txn
// failed with "balance N below min N+ε" (issue #15). Budget for the full set.
const REGISTRATION_TXN_COUNT: u64 = 12;
// Extra safety margin (microalgos) on top of the modelled fees so fee/rounding variance can
// never leave the account below its minimum balance during registration (issue #15).
const REGISTRATION_SAFETY_MARGIN_MICROALGOS: u64 = 10_000;

impl AlgoBingle {
    /// Create an [`AlgoBingle`] bound to the given [`AlgoOps`],
    /// application id, and Bingle$ asset id, without a shared accounts cache.
    ///
    /// Pass `app_id` or `asset_id` as `0` when the resource is not yet known (for example when
    /// it will be resolved by [`AlgoBingle::deploy_app_and_asset`]). To reuse indexer scans
    /// across lookups, use [`AlgoBingle::new_with_cache`] instead.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use algo_ops::AlgoOps;
    /// use bingle_core::blockchain::algo_bingle::AlgoBingle;
    ///
    /// let ops = AlgoOps::new_for_algorand(None, None, None);
    /// let bingle = AlgoBingle::new(ops, 1234, 5678);
    /// let owner = bingle.handle_lookup("alice")?;
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn new(ops: AlgoOps, app_id: u64, asset_id: u64) -> Self {
        // Debug-print the AlgoOps configuration for visibility
        algo_log!(
            "[AlgoBingle::new] ops.config={:?} app_id={} asset_id={}",
            ops.config,
            app_id,
            asset_id
        );
        Self {
            ops,
            app_id,
            asset_id,
            cache: None,
        }
    }

    /// Create an [`AlgoBingle`] that shares the given [`AccountsCache`], so repeated handle and
    /// endpoint lookups can reuse indexer scans instead of paging from scratch each time.
    ///
    /// Otherwise identical to [`AlgoBingle::new`].
    pub fn new_with_cache(
        ops: AlgoOps,
        app_id: u64,
        asset_id: u64,
        cache: Arc<Mutex<AccountsCache>>,
    ) -> Self {
        algo_log!(
            "[AlgoBingle::new_with_cache] app_id={} asset_id={}",
            app_id,
            asset_id
        );
        Self {
            ops,
            app_id,
            asset_id,
            cache: Some(cache),
        }
    }

    /// Extract the BinglePrice value from already-decoded global state entries.
    /// Returns Some(price) if found and parsed as u64; otherwise None.
    pub fn extract_bingle_price(entries: &[(String, String)]) -> Option<u64> {
        entries
            .iter()
            .find(|(k, _)| k == "BinglePrice")
            .and_then(|(_, v)| v.parse::<u64>().ok())
    }

    /// Exact ALGO balance an account must hold to complete registration, derived from live
    /// cost rather than a padded flat constant (issue #15, A3b).
    ///
    /// Reads the Bingle$ price and the app's local-state schema on-chain, then sums the
    /// post-registration minimum balance, the price, and the transaction fees via
    /// [`AlgoBingle::registration_funding_algos`]. Errors propagate so the caller can fall
    /// back to a static target if the chain is unreachable.
    pub fn required_funding(&self) -> Result<f64> {
        let price = self.get_bingle_price(self.app_id)?;
        let (uints, byte_slices) = self.ops.get_app_local_schema(self.app_id)?;
        Ok(Self::registration_funding_algos(price, uints, byte_slices))
    }

    /// The **post-registration minimum balance** (Algorand MBR) a *registered* account must retain,
    /// read live from this account's app: base + app opt-in (incl. local schema) + asset opt-in.
    ///
    /// Unlike [`Self::required_funding`], this excludes the one-time Bingle$ price and registration
    /// transaction fees — those are spent *at* registration, so charging them again would report a
    /// freshly registered account as short by exactly what it just paid. Algorand enforces this MBR
    /// on-chain, so a successfully registered account is always at or above it. Use this to decide
    /// whether an already-registered account can operate; use [`Self::required_funding`] for the funding
    /// needed to reach registration. Errors propagate so the caller can fall back if the chain is
    /// unreachable.
    pub fn post_registration_mbr(&self) -> Result<f64> {
        let (uints, byte_slices) = self.ops.get_app_local_schema(self.app_id)?;
        Ok(Self::post_registration_mbr_algos(uints, byte_slices))
    }

    /// Pure model of the post-registration minimum balance in microalgos (base + app opt-in incl.
    /// schema + asset opt-in). Shared with [`Self::registration_funding_algos`], which adds the one-time
    /// price + fees + safety margin on top.
    pub fn post_registration_mbr_microalgos(local_uints: u64, local_byte_slices: u64) -> u64 {
        let app_optin_mbr = MBR_APP_OPTIN_BASE_MICROALGOS
            + MBR_APP_UINT_MICROALGOS * local_uints
            + MBR_APP_BYTESLICE_MICROALGOS * local_byte_slices;
        MBR_BASE_MICROALGOS + app_optin_mbr + MBR_ASSET_OPTIN_MICROALGOS
    }

    /// [`Self::post_registration_mbr_microalgos`] expressed in ALGOs.
    pub fn post_registration_mbr_algos(local_uints: u64, local_byte_slices: u64) -> f64 {
        Self::post_registration_mbr_microalgos(local_uints, local_byte_slices) as f64
            / MICROALGOS_PER_ALGO as f64
    }

    /// Pure cost model for registering a handle: the post-registration minimum balance
    /// ([`Self::post_registration_mbr_microalgos`]) + the Bingle$ price + the registration transaction
    /// fees + a safety margin. The MBR/fee figures are Algorand protocol constants; `price_microalgos`
    /// and the app schema `(local_uints, local_byte_slices)` are read on-chain by
    /// [`AlgoBingle::required_funding`].
    pub fn registration_funding_algos(
        price_microalgos: u64,
        local_uints: u64,
        local_byte_slices: u64,
    ) -> f64 {
        let minimum_balance =
            Self::post_registration_mbr_microalgos(local_uints, local_byte_slices);
        let fees = MIN_TXN_FEE_MICROALGOS * REGISTRATION_TXN_COUNT;
        let total_microalgos =
            minimum_balance + price_microalgos + fees + REGISTRATION_SAFETY_MARGIN_MICROALGOS;
        total_microalgos as f64 / MICROALGOS_PER_ALGO as f64
    }

    /// Fetch the current Bingle price from the application's global state under key "BinglePrice".
    /// The price is stored as a uint in global state (microAlgos per Bingle unit).
    /// Errors if the key is not set or the application cannot be queried.
    pub fn get_bingle_price(&self, app_id: u64) -> Result<u64> {
        if app_id == 0 {
            bail!("app_id must be > 0");
        }
        let client = self.ops.algod_client()?;
        let app_info = self
            .ops
            .algod_call(|| client.app(AppId(app_id)))
            .map_err(|e| anyhow!("application_information failed: {e}"))?;
        let v = serde_json::to_value(&app_info)
            .map_err(|e| anyhow!("failed to serialize application info: {e}"))?;
        // Navigate to params.global-state (or global_state depending on casing)
        let gs_vec: Vec<serde_json::Value> = v
            .get("params")
            .and_then(|p| p.get("global-state").or_else(|| p.get("global_state")))
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        let entries = Self::decode_state_entries(&gs_vec);
        match Self::extract_bingle_price(&entries) {
            Some(p) => Ok(p),
            None => Err(anyhow!(
                "BinglePrice not set in global state for app_id {app_id}"
            )),
        }
    }

    fn decode_state_entries(entries: &[serde_json::Value]) -> Vec<(String, String)> {
        use base64::Engine as _;
        let mut kvs: Vec<(String, String)> = Vec::new();
        for entry in entries {
            let key_b64 = match entry.get("key").and_then(|x| x.as_str()) {
                Some(s) => s,
                None => continue,
            };
            let key = match base64::engine::general_purpose::STANDARD.decode(key_b64) {
                Ok(bytes) => String::from_utf8(bytes).unwrap_or_else(|_| key_b64.to_string()),
                Err(_) => key_b64.to_string(),
            };
            let val_obj = match entry.get("value").and_then(|x| x.as_object()) {
                Some(o) => o,
                None => continue,
            };
            let vtype = val_obj.get("type").and_then(|x| x.as_u64()).unwrap_or(0);
            let val = if vtype == 1 {
                // bytes can be an array of numbers or a base64 string
                let bytes_opt = if let Some(arr) = val_obj.get("bytes").and_then(|x| x.as_array()) {
                    let mut buf: Vec<u8> = Vec::with_capacity(arr.len());
                    for n in arr {
                        if let Some(u) = n.as_u64() {
                            buf.push((u & 0xFF) as u8);
                        } else if let Some(i) = n.as_i64()
                            && i >= 0
                        {
                            buf.push(((i as u64) & 0xFF) as u8);
                        }
                    }
                    Some(buf)
                } else if let Some(b64) = val_obj.get("bytes").and_then(|x| x.as_str()) {
                    base64::engine::general_purpose::STANDARD.decode(b64).ok()
                } else {
                    None
                };
                if let Some(bytes) = bytes_opt {
                    match String::from_utf8(bytes.clone()) {
                        Ok(s) => s,
                        Err(_) => {
                            let mut hex = String::from("0x");
                            for byte in bytes {
                                hex.push_str(&format!("{:02x}", byte));
                            }
                            hex
                        }
                    }
                } else {
                    String::new()
                }
            } else {
                val_obj
                    .get("uint")
                    .and_then(|x| x.as_u64())
                    .map(|u| u.to_string())
                    .unwrap_or_else(|| "0".to_string())
            };
            kvs.push((key, val));
        }
        kvs
    }

    /// Grant or revoke permission for a specific address to register a static endpoint.
    ///
    /// Calls set_allow_static on-chain for the provided target address. The caller must be the
    /// app admin (enforced by the contract). The target address must be opted-in to the app.
    /// Returns the submitted transaction id on success.
    pub fn set_allow_static(
        &self,
        app_id: u64,
        target_address: &str,
        allow: bool,
    ) -> Result<String> {
        if app_id == 0 {
            bail!("app_id must be > 0");
        }
        // Use AlgoOps::call_app and pass target address as an ARC-4 address argument
        let pk = address_to_byte_key(target_address)
            .map_err(|e| anyhow!("invalid target address: {e}"))?;
        let (txid, _logs) = self.ops.call_app(
            app_id,
            None,
            Some("set_allow_static(address,uint64)void"),
            &[
                AppArg::Bytes(pk.to_vec()),
                AppArg::Uint(if allow { 1 } else { 0 }),
            ],
        )?;
        Ok(txid)
    }

    /// Grant or revoke permission for a specific address to relay.
    ///
    /// Calls set_allow_relay on-chain for the provided target address. The caller must be the
    /// app admin (enforced by the contract). The target address must be opted-in to the app.
    /// Returns the submitted transaction id on success.
    pub fn set_allow_relay(
        &self,
        app_id: u64,
        target_address: &str,
        allow: bool,
    ) -> Result<String> {
        if app_id == 0 {
            bail!("app_id must be > 0");
        }
        // Use AlgoOps::call_app and pass target address as an ARC-4 address argument
        let pk = address_to_byte_key(target_address)
            .map_err(|e| anyhow!("invalid target address: {e}"))?;
        let (txid, _logs) = self.ops.call_app(
            app_id,
            None,
            Some("set_allow_relay(address,uint64)void"),
            &[
                AppArg::Bytes(pk.to_vec()),
                AppArg::Uint(if allow { 1 } else { 0 }),
            ],
        )?;
        Ok(txid)
    }

    /// Withdraw Algo from the app account to the given address.
    ///
    /// Calls withdraw(address,uint64)void. Must be called by the app withdrawer.
    /// Returns the submitted transaction id on success.
    /// Withdraw Algo and/or Bingle$ from the app account to the given address.
    ///
    /// Calls withdraw(address,uint64,uint64,uint64)void. Must be called by the app withdrawer.
    /// Pass amount=0 to skip Algo withdrawal; pass asset_amount=0 to skip ASA withdrawal.
    /// Returns the submitted transaction id on success.
    pub fn withdraw(
        &self,
        app_id: u64,
        address: &str,
        amount: u64,
        asset_id: u64,
        asset_amount: u64,
    ) -> Result<String> {
        if app_id == 0 {
            bail!("app_id must be > 0");
        }
        let pk = address_to_byte_key(address).map_err(|e| anyhow!("invalid address: {e}"))?;
        let foreign_asset = if asset_amount > 0 && asset_id > 0 {
            Some(asset_id)
        } else {
            None
        };
        let (txid, _logs) = self.ops.call_app(
            app_id,
            foreign_asset,
            Some("withdraw(address,uint64,uint64,uint64)void"),
            &[
                AppArg::Bytes(pk.to_vec()),
                AppArg::Uint(amount),
                AppArg::Uint(asset_id),
                AppArg::Uint(asset_amount),
            ],
        )?;
        Ok(txid)
    }

    /// Set the predecessor app id so that migrate_local can validate the source app.
    ///
    /// Calls set_predecessor_app(uint64)void. Must be called by the app creator.
    /// Returns the submitted transaction id on success.
    pub fn set_predecessor_app(&self, app_id: u64, predecessor_app_id: u64) -> Result<String> {
        if app_id == 0 {
            bail!("app_id must be > 0");
        }
        if predecessor_app_id == 0 {
            bail!("predecessor_app_id must be > 0");
        }
        let (txid, _logs) = self.ops.call_app_with_foreign_app(
            app_id,
            predecessor_app_id,
            None,
            Some("set_predecessor_app(uint64)void"),
            &[AppArg::Uint(predecessor_app_id)],
        )?;
        Ok(txid)
    }

    /// Mark `app_id` as superseded by `successor_app_id`, forcing clients to upgrade.
    ///
    /// Calls set_successor_app(uint64)void on `app_id`. Must be called by the app creator,
    /// on the app being retired (an app can only write its own global state). Once set, the
    /// old app hard-rejects user transactions and clients report UPGRADE_REQUIRED.
    /// Returns the submitted transaction id on success.
    pub fn set_successor_app(&self, app_id: u64, successor_app_id: u64) -> Result<String> {
        if app_id == 0 {
            bail!("app_id must be > 0");
        }
        if successor_app_id == 0 {
            bail!("successor_app_id must be > 0");
        }
        let (txid, _logs) = self.ops.call_app_with_foreign_app(
            app_id,
            successor_app_id,
            None,
            Some("set_successor_app(uint64)void"),
            &[AppArg::Uint(successor_app_id)],
        )?;
        Ok(txid)
    }

    /// Set the app admin address.
    ///
    /// Calls set_app_admin(address)void. Must be called by the app creator.
    /// Returns the submitted transaction id on success.
    pub fn set_app_admin(&self, app_id: u64, admin: &str) -> Result<String> {
        if app_id == 0 {
            bail!("app_id must be > 0");
        }
        let pk = address_to_byte_key(admin).map_err(|e| anyhow!("invalid admin address: {e}"))?;
        let (txid, _logs) = self.ops.call_app(
            app_id,
            None,
            Some("set_app_admin(address)void"),
            &[AppArg::Bytes(pk.to_vec())],
        )?;
        Ok(txid)
    }

    /// Set the app withdrawer address.
    ///
    /// Calls set_app_withdrawer(address)void. Must be called by the app creator.
    /// Returns the submitted transaction id on success.
    pub fn set_app_withdrawer(&self, app_id: u64, withdrawer: &str) -> Result<String> {
        if app_id == 0 {
            bail!("app_id must be > 0");
        }
        let pk = address_to_byte_key(withdrawer)
            .map_err(|e| anyhow!("invalid withdrawer address: {e}"))?;
        let (txid, _logs) = self.ops.call_app(
            app_id,
            None,
            Some("set_app_withdrawer(address)void"),
            &[AppArg::Bytes(pk.to_vec())],
        )?;
        Ok(txid)
    }

    /// Copy global state from old_app into this contract.
    ///
    /// Calls migrate_global(uint64)void. Must be called by the app creator.
    /// Returns the submitted transaction id on success.
    pub fn migrate_global(&self, app_id: u64, old_app_id: u64) -> Result<String> {
        if app_id == 0 {
            bail!("app_id must be > 0");
        }
        if old_app_id == 0 {
            bail!("old_app_id must be > 0");
        }
        let (txid, _logs) = self.ops.call_app_with_foreign_app(
            app_id,
            old_app_id,
            None,
            Some("migrate_global(uint64)void"),
            &[AppArg::Uint(old_app_id)],
        )?;
        Ok(txid)
    }

    /// Transfer the app account balance (minus min_balance) and Bingle$ ASA holdings to new_app.
    ///
    /// Calls migrate_reserve(uint64,uint64)void. Must be called by the app creator.
    /// Pass asset_id=0 to skip ASA migration.
    /// Returns the submitted transaction id on success.
    pub fn migrate_reserve(&self, app_id: u64, new_app_id: u64, asset_id: u64) -> Result<String> {
        if app_id == 0 {
            bail!("app_id must be > 0");
        }
        if new_app_id == 0 {
            bail!("new_app_id must be > 0");
        }
        let foreign_asset = if asset_id > 0 { Some(asset_id) } else { None };
        let (txid, _logs) = self.ops.call_app_with_foreign_app(
            app_id,
            new_app_id,
            foreign_asset,
            Some("migrate_reserve(uint64,uint64)void"),
            &[AppArg::Uint(new_app_id), AppArg::Uint(asset_id)],
        )?;
        Ok(txid)
    }

    /// Copy the caller's local state from old_app into the new app.
    ///
    /// Calls migrate_local(uint64)void. User-callable (no elevated privilege required).
    /// old_app_id must match the predecessor app stored on-chain by the creator.
    /// Returns the submitted transaction id on success.
    pub fn migrate_local(&self, app_id: u64, old_app_id: u64) -> Result<String> {
        if app_id == 0 {
            bail!("app_id must be > 0");
        }
        if old_app_id == 0 {
            bail!("old_app_id must be > 0");
        }
        let (txid, _logs) = self.ops.call_app_with_foreign_app(
            app_id,
            old_app_id,
            None,
            Some("migrate_local(uint64)void"),
            &[AppArg::Uint(old_app_id)],
        )?;
        Ok(txid)
    }

    /// Read the accepted-ancestor lineage of `new_app_id` — the creator-blessed source apps
    /// that `migrate_local` will copy from — decoded from the packed `AncestorApps` global.
    /// Ordered nearest ancestor first; empty if the app has no configured lineage.
    pub fn ancestor_apps(&self, new_app_id: u64) -> Result<Vec<u64>> {
        if new_app_id == 0 {
            bail!("new_app_id must be > 0");
        }
        let raw = self
            .ops
            .app_global_bytes(new_app_id, "AncestorApps")?
            .unwrap_or_default();
        let (chunks, _rem) = raw.as_chunks::<8>();
        let mut ids = Vec::with_capacity(chunks.len());
        for b in chunks {
            ids.push(u64::from_be_bytes(*b));
        }
        Ok(ids)
    }

    /// Read the successor app pointer of `app_id` — the app that supersedes it, if any —
    /// decoded from the 8-byte big-endian `SuccessorApp` global. Returns `None` when the app
    /// is not superseded (global absent or empty).
    pub fn successor_app(&self, app_id: u64) -> Result<Option<u64>> {
        if app_id == 0 {
            bail!("app_id must be > 0");
        }
        let raw = self
            .ops
            .app_global_bytes(app_id, "SuccessorApp")?
            .unwrap_or_default();
        if raw.len() != 8 {
            return Ok(None);
        }
        let mut b = [0u8; 8];
        b.copy_from_slice(&raw);
        let id = u64::from_be_bytes(b);
        Ok(if id == 0 { None } else { Some(id) })
    }

    /// Find the nearest blessed ancestor of `new_app_id` on which `account` still holds local
    /// state (i.e. has data to migrate). Returns None if the account has no data on any
    /// ancestor (a fresh install).
    pub fn find_migratable_ancestor(&self, new_app_id: u64, account: &str) -> Result<Option<u64>> {
        for ancestor in self.ancestor_apps(new_app_id)? {
            if self
                .ops
                .local_state_for_account(ancestor, account)?
                .is_some()
            {
                return Ok(Some(ancestor));
            }
        }
        Ok(None)
    }

    /// Migrate this account's local state to `new_app_id` from a blessed ancestor, once.
    ///
    /// Returns the `migrate_local` txid if a migration was performed, or `Ok(None)` when there
    /// is nothing to do: either the account is already registered on the new app (it holds a
    /// `Handle` there) or it has no data on any ancestor (a fresh install). The migration is
    /// user-signed: the caller opts into the new app (idempotent) and then copies from the
    /// nearest ancestor that holds its data.
    pub fn ensure_local_migrated(&self, new_app_id: u64) -> Result<Option<String>> {
        if new_app_id == 0 {
            bail!("new_app_id must be > 0");
        }
        let me = self
            .ops
            .address
            .as_ref()
            .ok_or_else(|| anyhow!("No sender address"))?
            .to_string();

        // Idempotency: if we already hold a Handle on the new app, migration is done.
        if let Some(kvs) = self.ops.local_state_for_account(new_app_id, &me)?
            && kvs.iter().any(|(k, _)| k == "Handle")
        {
            return Ok(None);
        }

        let ancestor = match self.find_migratable_ancestor(new_app_id, &me)? {
            Some(a) => a,
            None => return Ok(None), // fresh install: nothing to migrate
        };

        // Opt into the new app (idempotent) so the contract can write our local state, then
        // migrate from the ancestor. Both transactions are signed by our own account.
        self.ops.opt_in_app(new_app_id)?;
        let txid = self.migrate_local(new_app_id, ancestor)?;
        Ok(Some(txid))
    }

    /// Check if a specific address is allowed to relay on-chain.
    ///
    /// This queries the local state of the account for the provided app_id.
    /// Returns Ok(Some(true)) if allowed, Ok(Some(false)) if not allowed, Ok(None) if not opted-in, or Err if other error.
    pub fn check_allow_relay(&self, app_id: u64, address: &str) -> Result<Option<bool>> {
        if app_id == 0 {
            bail!("app_id must be > 0");
        }
        let kvs = self.ops.local_state_for_account(app_id, address)?;
        Ok(kvs.map(|entries| entries.iter().any(|(k, v)| k == "allow_relay" && v == "1")))
    }

    /// Resolve an account `address` to the handle it has registered on-chain, by reading the
    /// `"Handle"` key of its local state for `app_id`. Returns `Ok(None)` when the account is not
    /// opted in or has no handle registered.
    ///
    /// The inverse of [`handle_lookup`](Self::handle_lookup) (handle → address); used to attribute a
    /// store-and-forward message to its sender's handle from the authenticated sender address (#215).
    pub fn handle_for_address(&self, app_id: u64, address: &str) -> Result<Option<String>> {
        if app_id == 0 {
            bail!("app_id must be > 0");
        }
        let kvs = self.ops.local_state_for_account(app_id, address)?;
        Ok(kvs.and_then(|entries| {
            entries
                .into_iter()
                .find(|(k, _)| k == "Handle")
                .map(|(_, v)| v)
        }))
    }

    /// Call register_endpoint(string)void for the caller.
    /// Passing an empty string clears the local state key.
    /// Returns the submitted transaction id on success.
    // endpoint_advert_record_compact is now a compact AdvertRecord
    pub fn register_endpoint(
        &self,
        app_id: u64,
        endpoint_advert_record_compact: &str,
    ) -> Result<String> {
        if app_id == 0 {
            bail!("app_id must be > 0");
        }
        // Build ARC-4 arguments manually to ensure correct string encoding (2-byte length prefix)
        let client = self.ops.algod_client()?;
        let params = self.params(&client)?;
        let (account, sender) = self.sender_account()?;
        let mut app_args: Vec<Vec<u8>> = Vec::new();
        app_args.push(AlgoOps::arc4_selector("register_endpoint(string)void").to_vec());
        let ep_bytes = endpoint_advert_record_compact.as_bytes();
        if ep_bytes.len() > u16::MAX as usize {
            bail!("endpoint too long");
        }
        let mut arg = Vec::with_capacity(2 + ep_bytes.len());
        arg.extend_from_slice(&(ep_bytes.len() as u16).to_be_bytes());
        arg.extend_from_slice(ep_bytes);
        app_args.push(arg);
        let tx = CallApplication::new(sender, AppId(app_id))
            .app_arguments(app_args)
            .note(AlgoOps::unique_note())
            .build(&params)
            .map_err(|e| anyhow!("build app call: {e}"))?;
        algo_log!("register_endpoint tx: {:?}", tx);
        let signed = account
            .sign(tx)
            .map_err(|e| anyhow!("sign app call: {e}"))?
            .to_msg_pack()
            .map_err(|e| anyhow!("encode signed app call: {e}"))?;
        self.broadcast_group(&client, vec![signed])
    }

    /// Read the static endpoint record currently registered on-chain by `address`.
    /// The record may be split across the "static_endpoint" and "static_endpoint_x"
    /// local state keys; the parts are concatenated.
    /// Returns Ok(None) if the account is not opted in or no record is present.
    pub fn get_static_endpoint(&self, app_id: u64, address: &str) -> Result<Option<String>> {
        if app_id == 0 {
            bail!("app_id must be > 0");
        }
        let kvs = match self.ops.local_state_for_account(app_id, address)? {
            Some(entries) => entries,
            None => return Ok(None),
        };
        let ep = kvs
            .iter()
            .find(|(k, _)| k == "static_endpoint")
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        let ep_x = kvs
            .iter()
            .find(|(k, _)| k == "static_endpoint_x")
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        let full = format!("{}{}", ep, ep_x);
        if full.is_empty() {
            Ok(None)
        } else {
            Ok(Some(full))
        }
    }

    /// Decide whether shutdown should clear the on-chain static endpoint record.
    ///
    /// Guards against a redeploy race where a replacement task has already registered
    /// a newer record under the same account: only clear when the record this process
    /// registered at startup is still the one on-chain.
    ///
    /// `registered_record` is the compact record this process wrote at startup (None if
    /// registration never happened or failed); `current_record` is the record currently
    /// on-chain (None if absent).
    pub fn should_clear_static_endpoint(
        registered_record: Option<&str>,
        current_record: Option<&str>,
    ) -> bool {
        match (registered_record, current_record) {
            (Some(ours), Some(current)) => ours == current,
            _ => false,
        }
    }

    /// Parse a RelayIP string into a SocketAddr. Accepts forms like "host:port" or "ip:port".
    /// Returns None if parsing fails.
    pub fn parse_relay_ip(ip: &str) -> Option<std::net::SocketAddr> {
        ip.parse::<std::net::SocketAddr>().ok()
    }

    /// Use the Indexer API to list accounts that have a non-empty "static_endpoint" in local state for the given app_id.
    /// Returns Vec of (account_address, static_endpoint_value).
    pub fn list_static_endpoints_via_indexer(&self, app_id: u64) -> Result<Vec<(String, String)>> {
        algo_log!(
            "[AlgoBingle::list_static_endpoints_via_indexer] app_id={}",
            app_id
        );
        if tokio::runtime::Handle::try_current().is_ok() {
            algo_log!(
                "[AlgoBingle::list_static_endpoints_via_indexer] running in tokio runtime, spawning thread"
            );
            let thread_result = std::thread::scope(|s| {
                s.spawn(|| self.list_static_endpoints_via_indexer_sync(app_id))
                    .join()
                    .unwrap_or_else(|_| Err(anyhow!("indexer thread panicked")))
            });
            algo_log!(
                "[AlgoBingle::list_static_endpoints_via_indexer] thread result={:?}",
                thread_result
            );
            return thread_result;
        }

        algo_log!(
            "[AlgoBingle::list_static_endpoints_via_indexer] not running in tokio runtime, calling sync"
        );
        let sync_result = self.list_static_endpoints_via_indexer_sync(app_id);
        algo_log!(
            "[AlgoBingle::list_static_endpoints_via_indexer] sync result={:?}",
            sync_result
        );
        sync_result
    }

    /// Synchronous implementation behind [`AlgoBingle::list_static_endpoints_via_indexer`],
    /// listing `(address, static_endpoint)` pairs for accounts opted in to `app_id`.
    ///
    /// Prefer [`AlgoBingle::list_static_endpoints_via_indexer`], which dispatches to this on a
    /// dedicated thread when called from within a tokio runtime.
    ///
    /// # Errors
    ///
    /// Errors if the indexer query fails; a host-unreachable failure is treated as a transient
    /// condition and logged at info level before being returned.
    pub fn list_static_endpoints_via_indexer_sync(
        &self,
        app_id: u64,
    ) -> Result<Vec<(String, String)>> {
        // Debug: print the current ops.config for visibility in discovery
        algo_log!(
            "[AlgoBingle::list_static_endpoints_via_indexer_sync] ops.config={:?}",
            self.ops.config
        );
        let mut results: Vec<(String, String)> = Vec::new();
        let indexer_query_result = self.indexer_query_opted_in_accounts_sync(
            app_id,
            QueryMode::Refresh,
            Some(30),
            |acct| {
                tracing::debug!(
                    "[AlgoBingle::list_static_endpoints_via_indexer_sync] processing account: address: {:?} local_state: {:?}",
                    acct.address,
                    acct.local_state
                );
                // `local_state` is already the app's decoded key/values (algo_ops scopes the scan to
                // `app_id`), so the endpoint is a split byte-slice: `static_endpoint` plus overflow in
                // `static_endpoint_x`.
                let field = |key: &str| {
                    acct.local_state
                        .iter()
                        .find(|(k, _)| k == key)
                        .map(|(_, v)| v.as_str())
                        .unwrap_or("")
                };
                let full_val = format!("{}{}", field("static_endpoint"), field("static_endpoint_x"));
                if !full_val.is_empty() {
                    results.push((acct.address.clone(), full_val));
                }
                Ok(())
            },
        );

        if let Err(e) = indexer_query_result {
            // A host-unreachable failure (no connection / HTTP send error) is an expected transient
            // condition, so log at info; any other failure is a genuine error.
            if AlgoError::is_host_unreachable(&e) {
                tracing::info!(
                    "[AlgoBingle::list_static_endpoints_via_indexer_sync] indexer_query_opted_in_accounts_sync unreachable: {}",
                    e
                );
            } else {
                tracing::error!(
                    "[AlgoBingle::list_static_endpoints_via_indexer_sync] indexer_query_opted_in_accounts_sync failed: {}",
                    e
                );
            }
            Err(e)
        } else {
            algo_log!(
                "[AlgoBingle::list_static_endpoints_via_indexer_sync] results={:?}",
                results
            );
            Ok(results)
        }
    }

    /// Query the accounts opted in to `app_id` via the indexer, invoking `f` once per opted-in
    /// account (in cache order).
    ///
    /// A thin adapter over [`AlgoOps::fetch_opted_in_accounts_cached`]: the incremental paging, the
    /// account cache, and the min-round watermark all live in `algo_ops` (over its
    /// [`AccountScanCache`]). The only Bingle-specific part is the per-account decode `f` performs
    /// against the already-decoded [`ScannedAccount::local_state`] — the endpoint or handle field.
    ///
    /// When this instance carries a shared [`cache`](Self::cache) the scan is cached/incremental per
    /// `mode` and `cache_lifetime_secs`; without one, an ephemeral cache backs a single scan so the
    /// callback still sees every currently opted-in account.
    ///
    /// # Errors
    ///
    /// Errors if `app_id` is 0, if the indexer query fails, or if `f` returns an error.
    pub fn indexer_query_opted_in_accounts_sync<F>(
        &self,
        app_id: u64,
        mode: QueryMode,
        cache_lifetime_secs: Option<u64>,
        mut f: F,
    ) -> Result<()>
    where
        F: FnMut(&ScannedAccount) -> Result<()>,
    {
        algo_log!(
            "[AlgoBingle][indexer_query_opted_in_accounts_sync] app_id={} mode={:?} cache_lifetime={:?}",
            app_id,
            mode,
            cache_lifetime_secs
        );

        // Refresh the caller's shared cache when present, else a throwaway one for a single scan.
        let ephemeral;
        let cache: &Mutex<AccountsCache> = match &self.cache {
            Some(shared) => shared.as_ref(),
            None => {
                ephemeral = Mutex::new(AccountsCache::new());
                &ephemeral
            }
        };

        // Cache each opted-in account as its decoded `ScannedAccount`; the caller decodes its own
        // field (endpoint, handle) from `local_state`, so the cache stays schema-agnostic.
        self.ops.fetch_opted_in_accounts_cached(
            app_id,
            cache,
            mode,
            cache_lifetime_secs,
            |acct| Some(acct.clone()),
        )?;

        let cache = cache
            .lock()
            .map_err(|_| anyhow!("account scan cache mutex poisoned"))?;
        for (_addr, acct) in &cache.entries {
            f(acct)?;
        }
        Ok(())
    }

    /// Look up a Bingle handle on-chain using the indexer, returning the address of the oldest
    /// account that has this handle registered in its local state.
    ///
    /// Handles are matched by their normalised form (see [`AlgoBingle::normalize_handle`]), and
    /// ties on registration time are broken deterministically by address. Returns `Ok(None)` when
    /// no account holds the handle.
    ///
    /// # Errors
    ///
    /// Errors if no application id is available (neither on this instance nor in the
    /// [`AlgoOps`] config) or if the indexer query fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use algo_ops::AlgoOps;
    /// use bingle_core::blockchain::algo_bingle::AlgoBingle;
    ///
    /// let bingle = AlgoBingle::new(AlgoOps::new_for_algorand(None, None, None), 1234, 5678);
    /// match bingle.handle_lookup("alice")? {
    ///     Some(address) => println!("alice is owned by {address}"),
    ///     None => println!("alice is not registered"),
    /// }
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn handle_lookup(&self, handle: &str) -> Result<Option<String>> {
        let app_id = if self.app_id != 0 {
            self.app_id
        } else {
            self.ops
                .config
                .app_id
                .ok_or_else(|| anyhow!("app_id not configured in AlgoOps config"))?
        };
        if tokio::runtime::Handle::try_current().is_ok() {
            return std::thread::scope(|s| {
                s.spawn(|| self.handle_lookup_sync(app_id, handle))
                    .join()
                    .unwrap_or_else(|_| Err(anyhow!("indexer thread panicked")))
            });
        }
        self.handle_lookup_sync(app_id, handle)
    }

    fn handle_lookup_sync(&self, app_id: u64, handle: &str) -> Result<Option<String>> {
        let mut matches: Vec<(String, u64)> = Vec::new();
        self.indexer_query_opted_in_accounts_sync(app_id, QueryMode::Refresh, Some(30), |acct| {
            Self::extract_handle_match(acct, handle, &mut matches);
            Ok(())
        })?;
        algo_log!(
            "[handle_lookup_sync] handle={} matches={:?}",
            handle,
            matches
        );
        Ok(Self::pick_oldest_match(matches))
    }

    /// Partial (prefix) lookup of a Bingle handle on-chain using the Indexer.
    ///
    /// The `prefix` is normalised by the handle matching rules (lowercase, alphanumeric
    /// only) and matched against the start of each registered handle's normalised form,
    /// so `"abc"` matches a registered `"ab_cd"`. Returns the oldest matching account as a
    /// tuple of `(id, canonical_handle)`, where `canonical_handle` is the handle exactly as
    /// written in the account's local state on the blockchain. Returns `Ok(None)` if no
    /// handle starts with the prefix.
    pub fn handle_lookup_partial(&self, prefix: &str) -> Result<Option<(String, String)>> {
        let app_id = if self.app_id != 0 {
            self.app_id
        } else {
            self.ops
                .config
                .app_id
                .ok_or_else(|| anyhow!("app_id not configured in AlgoOps config"))?
        };
        if tokio::runtime::Handle::try_current().is_ok() {
            return std::thread::scope(|s| {
                s.spawn(|| self.handle_lookup_partial_sync(app_id, prefix))
                    .join()
                    .unwrap_or_else(|_| Err(anyhow!("indexer thread panicked")))
            });
        }
        self.handle_lookup_partial_sync(app_id, prefix)
    }

    fn handle_lookup_partial_sync(
        &self,
        app_id: u64,
        prefix: &str,
    ) -> Result<Option<(String, String)>> {
        let mut matches: Vec<(String, String, u64)> = Vec::new();
        self.indexer_query_opted_in_accounts_sync(app_id, QueryMode::Refresh, Some(30), |acct| {
            Self::handle_prefix_match(acct, prefix, &mut matches);
            Ok(())
        })?;
        algo_log!(
            "[handle_lookup_partial_sync] prefix={} matches={:?}",
            prefix,
            matches
        );
        Ok(Self::pick_oldest_prefix_match(matches))
    }

    /// Normalises a handle for comparison: lowercase and alphanumeric characters only.
    pub fn normalize_handle(handle: &str) -> String {
        handle
            .chars()
            .filter(|c| c.is_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect()
    }

    /// Find a handle match in an opted-in account's decoded local state, appending
    /// `(address, handle_time)` to `matches` on an exact (normalised) match.
    ///
    /// `acct.local_state` is already the account's decoded key/values for the scanned app (algo_ops
    /// scopes the scan to a single `app_id`), so this only selects the `Handle` / `HandleTime`
    /// fields — no per-app filtering or base64 decode here.
    pub fn extract_handle_match(
        acct: &ScannedAccount,
        handle: &str,
        matches: &mut Vec<(String, u64)>,
    ) {
        let normalised_handle = Self::normalize_handle(handle);
        algo_log!(
            "[extract_handle_match] address={} local_state={:?}",
            acct.address,
            acct.local_state
        );
        if let Some((_, h)) = acct.local_state.iter().find(|(k, _)| k == "Handle")
            && Self::normalize_handle(h) == normalised_handle
        {
            let time = acct
                .local_state
                .iter()
                .find(|(k, _)| k == "HandleTime")
                .and_then(|(_, v)| v.parse::<u64>().ok())
                .unwrap_or(0);
            matches.push((acct.address.clone(), time));
        }
    }

    /// Picks the oldest account (smallest HandleTime) from a list of matches.
    pub fn pick_oldest_match(mut matches: Vec<(String, u64)>) -> Option<String> {
        if matches.is_empty() {
            None
        } else {
            matches.sort_by(|a, b| {
                let res = a.1.cmp(&b.1);
                if res == std::cmp::Ordering::Equal {
                    a.0.cmp(&b.0)
                } else {
                    res
                }
            });
            Some(matches[0].0.clone())
        }
    }

    /// Find a handle prefix match in an opted-in account's decoded local state.
    ///
    /// The `prefix` is normalised and compared against the start of the account's normalised handle.
    /// On a match, appends `(address, canonical_handle, handle_time)` to `matches`, where
    /// `canonical_handle` is the handle as written in local state. An empty (post-normalisation)
    /// prefix never matches.
    ///
    /// `acct.local_state` is already the account's decoded key/values for the scanned app (algo_ops
    /// scopes the scan to a single `app_id`), so this only selects the `Handle` / `HandleTime`
    /// fields — no per-app filtering or base64 decode here.
    pub fn handle_prefix_match(
        acct: &ScannedAccount,
        prefix: &str,
        matches: &mut Vec<(String, String, u64)>,
    ) {
        let normalised_prefix = Self::normalize_handle(prefix);
        if normalised_prefix.is_empty() {
            return;
        }
        if let Some((_, h)) = acct.local_state.iter().find(|(k, _)| k == "Handle")
            && Self::normalize_handle(h).starts_with(&normalised_prefix)
        {
            let time = acct
                .local_state
                .iter()
                .find(|(k, _)| k == "HandleTime")
                .and_then(|(_, v)| v.parse::<u64>().ok())
                .unwrap_or(0);
            matches.push((acct.address.clone(), h.clone(), time));
        }
    }

    /// Picks the oldest account (smallest HandleTime) from a list of prefix matches,
    /// returning `(address, canonical_handle)`. Ties are broken by address for determinism.
    pub fn pick_oldest_prefix_match(
        mut matches: Vec<(String, String, u64)>,
    ) -> Option<(String, String)> {
        if matches.is_empty() {
            None
        } else {
            matches.sort_by(|a, b| {
                let res = a.2.cmp(&b.2);
                if res == std::cmp::Ordering::Equal {
                    a.0.cmp(&b.0)
                } else {
                    res
                }
            });
            Some((matches[0].0.clone(), matches[0].1.clone()))
        }
    }

    fn sender_account(&self) -> Result<(Account, Address)> {
        let sk = self.ops.private_key_bytes()?;
        let seed: [u8; 32] = sk
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("Secret key must be 32 bytes"))?;
        let account = Account::from_seed(seed);
        let addr_str = self
            .ops
            .address
            .as_ref()
            .ok_or_else(|| anyhow!("This operation needs an address"))?;
        let addr = Address::from_str(addr_str).map_err(|e| anyhow!("invalid address: {e}"))?;
        Ok((account, addr))
    }

    fn params(&self, client: &Algod) -> Result<SuggestedParams> {
        self.ops.algod_call(|| client.suggested_params())
    }

    fn app_address(&self, app_id: u64) -> Result<Address> {
        let addr_str = self.ops.contract_address(app_id)?;
        Address::from_str(&addr_str).map_err(|e| anyhow!("invalid app address: {e}"))
    }

    /// Submit a group of already-signed, msgpack-encoded transactions and wait for confirmation
    /// of the first transaction in the group.
    ///
    /// Returns the transaction id reported by the node for the submitted group.
    ///
    /// # Errors
    ///
    /// Errors if the node rejects the raw submission or if confirmation is not observed within
    /// the wait window.
    pub fn broadcast_group(&self, client: &Algod, signed_group: Vec<Vec<u8>>) -> Result<String> {
        // Concatenate msgpack-encoded signed transactions into one byte array as per Algod API.
        let mut bytes: Vec<u8> = Vec::new();
        for s in signed_group {
            bytes.extend_from_slice(&s);
        }

        let txid = self
            .ops
            .algod_call(|| client.send_raw(&bytes))
            .map_err(|e| anyhow!("send_raw failed: {e}"))?
            .tx_id;
        // Wait for confirmation of the first tx id in the group.
        self.ops.wait_for_confirmation(&txid, 10)?;
        Ok(txid)
    }

    /// Compute and assign the Algorand atomic-transfer group id to every transaction in `txns`,
    /// binding them into a single atomic group.
    ///
    /// The group id is derived like the Python software development kit (SDK):
    /// `sha512_256("TG" || msgpack({ txlist: [txid, …] }))`, where each `txid` is
    /// `sha512_256("TX" || msgpack(tx))`.
    ///
    /// # Errors
    ///
    /// Errors if `txns` is empty, if it holds more than 15 transactions (the limit of the minimal
    /// built-in msgpack encoder), or if any transaction fails to msgpack-encode.
    pub fn assign_group_id(txns: &mut [Transaction]) -> Result<()> {
        // Try to use algonaut's internal grouping if available via feature gates; otherwise compute manually.
        // We attempt the canonical SDK approach from Python: gid = sha512_256("TG" || msgpack({txlist:[txid...]})).
        // algonaut exposes a helper under transaction::transaction::assign_group_id in recent versions.
        #[allow(unused_imports)]
        use algonaut::transaction::transaction as txmod;
        #[allow(unused_variables)]
        {
            // If algonaut provides assign_group_id, use it.
            #[allow(unused_must_use)]
            {
                // Some versions expose txmod::assign_group_id(&mut [Transaction]) -> Result<()>
                // We'll call it if available; otherwise the below manual fallback compiles.
            }
        }
        // Manual fallback: compute group id like Python SDK.
        // 1) Compute txids = sha512_256("TX" || msgpack(transaction_without_group)) for each txn
        let mut txids: Vec<[u8; 32]> = Vec::with_capacity(txns.len());
        for tx in txns.iter() {
            let raw = tx
                .to_msg_pack()
                .map_err(|e| anyhow!("failed to msgpack unsigned tx: {e}"))?;
            let mut tv = Vec::with_capacity(2 + raw.len());
            tv.extend_from_slice(b"TX");
            tv.extend_from_slice(&raw);
            let mut ht = Sha512_256::new();
            ht.update(&tv);
            let txid: [u8; 32] = ht.finalize().into();
            txids.push(txid);
        }
        // 2) Build msgpack of { "txlist": [txid1, txid2, ...] } manually (minimal encoder)
        // msgpack map of 1 element => 0x81, key is a str of len 6 => 0xA6 'txlist'
        // array header: len N => 0x90 + N (assuming N<16)
        let n = txids.len();
        if n == 0 {
            bail!("no transactions to group");
        }
        if n > 15 {
            bail!("too many txns in group (max 15 supported by minimal encoder)");
        }
        let mut group_mp: Vec<u8> = Vec::with_capacity(1 + 1 + 6 + 1 + n * 34);
        group_mp.push(0x81); // 1-key map
        group_mp.push(0xA6); // str len 6
        group_mp.extend_from_slice(b"txlist");
        group_mp.push(0x90 + (n as u8)); // fixarray of length n
        for txid in &txids {
            // bin32 with 32 bytes: prefix 0xC4 0x20
            group_mp.push(0xC4);
            group_mp.push(32);
            group_mp.extend_from_slice(txid);
        }
        // 3) gid = sha512_256("TG" || group_mp)
        let mut v = Vec::with_capacity(2 + group_mp.len());
        v.extend_from_slice(b"TG");
        v.extend_from_slice(&group_mp);
        let mut hg = Sha512_256::new();
        hg.update(&v);
        let gid: [u8; 32] = hg.finalize().into();

        // 4) Set group id on each transaction
        for tx in txns.iter_mut() {
            tx.group = Some(algonaut::crypto::HashDigest(gid));
        }
        Ok(())
    }

    /// Call buy_bingle. Requires:
    /// - app_id: the deployed application id
    /// - asset_id: the ASA id for Bingle$
    /// - price_microalgos: the current price configured on-chain (microAlgos)
    pub fn buy_bingle(&self, app_id: u64, asset_id: u64, price_microalgos: u64) -> Result<String> {
        if app_id == 0 {
            bail!("app_id must be > 0");
        }
        if asset_id == 0 {
            bail!("asset_id must be > 0");
        }
        // Ensure the caller (sender) is opted in to receive the asset from the app's inner tx
        let _ = self.opt_in_sender_to_asset(asset_id)?;
        let app_addr = self.app_address(app_id)?.to_string();
        algo_log!("[buy_bingle] app_addr: {app_addr}");

        // Atomic group: payment (sender -> app for the price) + `buy_bingle()void` app-call with the
        // ASA as a foreign asset. The app verifies the payment and, via an inner tx, moves 1 unit of
        // the ASA from the creator-held reserve to the sender. Signed together and broadcast as one
        // group.
        self.ops
            .transaction_group()
            .payment(&app_addr, price_microalgos)
            .call_app(app_id, Some("buy_bingle()void"), &[])
            .foreign_asset(asset_id)
            .sign_and_send()
    }

    /// Call sell_bingle. Requires:
    /// - app_id, asset_id as above
    /// - amount: number of units to sell
    /// - price_microalgos: price of one unit, payout = price * amount
    pub fn sell_bingle(
        &self,
        app_id: u64,
        asset_id: u64,
        amount: u64,
        price_microalgos: u64,
    ) -> Result<String> {
        if app_id == 0 {
            bail!("app_id must be > 0");
        }
        if asset_id == 0 {
            bail!("asset_id must be > 0");
        }
        if amount == 0 {
            bail!("amount must be > 0");
        }
        // Ensure the app account is opted-in to the ASA so it can receive transfers
        self.ops
            .opt_in_app_to_asset(app_id, asset_id, "opt_in_to_bingle(uint64)void")?;
        let app_addr = self.app_address(app_id)?.to_string();
        let self_addr = self.ops.address_str()?;

        let payout = price_microalgos
            .checked_mul(amount)
            .ok_or_else(|| anyhow!("payout overflow"))?;

        // Atomic group: asset transfer (sender -> app of `amount` units) + a self-payment of
        // `payout` (satisfies the on-chain payment check) + `sell_bingle(uint64)void` app-call
        // carrying `amount` as its ARC-4 uint64 argument, with the ASA as a foreign asset.
        self.ops
            .transaction_group()
            .asset_transfer(asset_id, amount, &app_addr)
            .payment(&self_addr, payout)
            .call_app(
                app_id,
                Some("sell_bingle(uint64)void"),
                &[AppArg::Uint(amount)],
            )
            .foreign_asset(asset_id)
            .sign_and_send()
    }

    /// Register `handle` on-chain for the signing account, paying `price_units` of Bingle$.
    ///
    /// Before submitting, this checks handle uniqueness via the indexer and, afterwards, verifies
    /// that the signing account is the deterministic winner for the handle. `price_units` is the
    /// fee in Bingle$ units (the on-chain configured price). Returns the submitted transaction id
    /// on success.
    ///
    /// # Errors
    ///
    /// Errors if `app_id` or `asset_id` is `0`, if `handle` is empty, if the handle is already
    /// registered to a different account, or if any indexer query or transaction submission fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use algo_ops::AlgoOps;
    /// use bingle_core::blockchain::algo_bingle::AlgoBingle;
    ///
    /// let bingle = AlgoBingle::new(AlgoOps::new_for_algorand(None, None, None), 1234, 5678);
    /// let txid = bingle.register(1234, 5678, "alice", 1)?;
    /// println!("registered in {txid}");
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn register(
        &self,
        app_id: u64,
        asset_id: u64,
        handle: &str,
        price_units: u64,
    ) -> Result<String> {
        if app_id == 0 {
            bail!("app_id must be > 0");
        }
        if asset_id == 0 {
            bail!("asset_id must be > 0");
        }
        if handle.is_empty() {
            bail!("handle must not be empty");
        }

        let sender_str = self
            .ops
            .address
            .as_ref()
            .ok_or_else(|| anyhow!("No sender address"))?
            .to_string();

        // 1. Pre-check: Handle uniqueness via indexer
        if let Some(existing_owner) = self.handle_lookup(handle)?
            && existing_owner != sender_str
        {
            bail!("Handle already in use by {}", existing_owner);
        }

        let tx_id = self.register_unchecked(app_id, asset_id, handle, price_units)?;

        // 4. Post-check: Validate that we are the deterministic winner for this handle
        self.verify_registration_winner(handle, &sender_str)?;

        Ok(tx_id)
    }

    /// Internal method to perform the registration on-chain without uniqueness pre-checks.
    /// Used by `register` after pre-checks, or by tests to simulate "hacked" registrations.
    pub fn register_unchecked(
        &self,
        app_id: u64,
        asset_id: u64,
        handle: &str,
        price_units: u64,
    ) -> Result<String> {
        // Ensure the app account is opted-in to the ASA so it can receive the registration fee
        self.ops
            .opt_in_app_to_asset(app_id, asset_id, "opt_in_to_bingle(uint64)void")?;

        // Ensure the caller (sender) is opted-in to the ASA so the axfer succeeds
        let _ = self.opt_in_sender_to_asset(asset_id)?;
        // Ensure the caller is opted-in to the application local state to avoid logic eval error
        self.ops.opt_in_app(app_id)?;
        let app_addr = self.app_address(app_id)?.to_string();

        // ARC-4 string encoding for the `handle` argument: 2-byte big-endian length prefix + UTF-8.
        let handle_bytes = handle.as_bytes();
        if handle_bytes.len() > u16::MAX as usize {
            bail!("handle too long");
        }
        let mut handle_arg = Vec::with_capacity(2 + handle_bytes.len());
        handle_arg.extend_from_slice(&(handle_bytes.len() as u16).to_be_bytes());
        handle_arg.extend_from_slice(handle_bytes);

        // Atomic group: ASA fee transfer (sender -> app, `price_units`) + `register(string)void`
        // app-call carrying the handle as its ARC-4 string argument, with the ASA as a foreign asset.
        self.ops
            .transaction_group()
            .asset_transfer(asset_id, price_units, &app_addr)
            .call_app(
                app_id,
                Some("register(string)void"),
                &[AppArg::Bytes(handle_arg)],
            )
            .foreign_asset(asset_id)
            .sign_and_send()
    }

    /// Wait up to 15 seconds for indexer to catch up and verify that `expected_winner` is the owner of `handle`.
    pub fn verify_registration_winner(&self, handle: &str, expected_winner: &str) -> Result<()> {
        let mut winner_verified = false;
        for i in 0..15 {
            if i > 0 {
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            if let Some(winner) = self.handle_lookup(handle)? {
                if winner == expected_winner {
                    winner_verified = true;
                    break;
                } else {
                    bail!(
                        "Handle registration verification failed: handle '{}' was taken by {} (expected {})",
                        handle,
                        winner,
                        expected_winner
                    );
                }
            }
        }

        if !winner_verified {
            algo_log!(
                "Warning: handle registration verification timed out for handle '{}'. It may still be pending in indexer.",
                handle
            );
        }
        Ok(())
    }

    /// Deploy or reuse the BingleDapp smart contract and Bingle$ ASA, wiring them together.
    ///
    /// `app_path` is a directory containing `*.approval.teal` and `*.clear.teal` files; used
    /// when a new application must be created.
    ///
    /// Resolution order for each resource:
    /// - `new_app = true`          → always deploy a fresh application from TEAL
    /// - `app_id = Some(id)`       → use the provided existing app id
    /// - `self.app_id > 0`         → fall back to the app id baked into this AlgoBingle instance
    /// - otherwise                 → deploy a fresh application from TEAL
    ///
    /// Same logic applies to the asset (`new_asset`, `asset_id`, `self.asset_id`).
    ///
    /// When a new app is deployed alongside an existing asset, any ASA balance held by the
    /// *old* app account (i.e. `self.app_id`) is clawbacked to the new app account.
    ///
    /// Returns `(effective_app_id, effective_asset_id)`.
    /// Deploy or reuse the BingleDapp smart contract and Bingle$ ASA, wiring them together.
    ///
    /// `self.ops` must be the APP_CREATOR account. Additional named accounts are supplied via
    /// `accounts` (keys are the `ACCOUNT_*` constants defined in this module):
    ///
    /// - `APP_ADMIN`      — address set as `app_admin` global state; calls `set_bingle_price`.
    ///                      Defaults to APP_CREATOR if absent.
    /// - `APP_WITHDRAWER` — address set as `app_withdrawer` global state; opted into the ASA.
    ///                      Defaults to APP_CREATOR if absent.
    /// - `ASSET_CREATOR`  — account that creates the ASA (becomes ASA manager).
    ///                      Defaults to APP_CREATOR if absent.
    /// - `ASSET_RESERVE`  — address used as the ASA reserve field; opted into the ASA if it
    ///                      differs from the app account. Defaults to the app address if absent.
    /// - `ASSET_MANAGER`  — address set as the ASA manager field, allowing future reconfiguration.
    /// - `ASSET_FREEZE`   — address set as the ASA freeze field.
    pub fn deploy_app_and_asset(
        &self,
        app_path: &std::path::Path,
        new_app: bool,
        new_asset: bool,
        app_id: Option<u64>,
        asset_id: Option<u64>,
        asset_name: &str,
        total_units: u64,
        initial_hot_bingle: u64,
        accounts: &HashMap<String, AlgoOps>,
    ) -> Result<(u64, u64)> {
        // All six named roles are required; all seven addresses must be distinct.
        let required_roles = [
            ACCOUNT_APP_ADMIN,
            ACCOUNT_APP_WITHDRAWER,
            ACCOUNT_ASSET_CREATOR,
            ACCOUNT_ASSET_RESERVE,
            ACCOUNT_ASSET_MANAGER,
            ACCOUNT_ASSET_FREEZE,
        ];
        for role in required_roles {
            if !accounts.contains_key(role) {
                return Err(anyhow!(
                    "deploy_app_and_asset: missing required account '{}'",
                    role
                ));
            }
        }
        let creator_addr = self.ops.address_str()?;
        let mut seen_addrs = HashSet::new();
        seen_addrs.insert(creator_addr.clone());
        for role in required_roles {
            let addr = accounts[role].address_str()?;
            if !seen_addrs.insert(addr.clone()) {
                return Err(anyhow!(
                    "deploy_app_and_asset: account '{}' address {} is not unique across roles",
                    role,
                    addr
                ));
            }
        }

        // Resolve named accounts.
        let admin_ops = &accounts[ACCOUNT_APP_ADMIN];
        let admin_addr = admin_ops.address_str()?;
        let withdrawer_ops = &accounts[ACCOUNT_APP_WITHDRAWER];
        let withdrawer_addr = withdrawer_ops.address_str()?;
        let asset_creator_ops = &accounts[ACCOUNT_ASSET_CREATOR];

        // ── 1. Resolve effective app ──────────────────────────────────────────────
        let need_new_app = new_app || (app_id.is_none() && self.app_id == 0);
        let effective_app_id = if need_new_app {
            let (approval_src, clear_src, arc56_json) = Self::read_teal_from_dir(app_path)?;
            let approval = self.ops.compile_teal(&approval_src)?;
            let clear = self.ops.compile_teal(&clear_src)?;
            let admin_pk = address_to_byte_key(&admin_addr)?;
            let withdrawer_pk = address_to_byte_key(&withdrawer_addr)?;
            let id = self
                .ops
                .deploy_app(
                    &approval,
                    &clear,
                    None,
                    Some("create(address,address)void"),
                    &[
                        AppArg::Bytes(admin_pk.to_vec()),
                        AppArg::Bytes(withdrawer_pk.to_vec()),
                    ],
                    "opt_in_to_bingle(uint64)void",
                    &arc56_json,
                )?
                .ok_or_else(|| anyhow!("deploy_app returned no app_id"))?;
            // Default price of 1 so registration/buy flows work immediately.
            admin_ops.call_app(
                id,
                None,
                Some("set_bingle_price(uint64)void"),
                &[AppArg::Uint(1)],
            )?;
            tracing::info!("[deploy_app_and_asset] deployed new app_id={}", id);
            id
        } else {
            let id = app_id.unwrap_or(self.app_id);
            tracing::info!("[deploy_app_and_asset] using existing app_id={}", id);
            id
        };

        // ── 2. Resolve effective asset ────────────────────────────────────────────
        let app_addr = self.ops.contract_address(effective_app_id)?;
        let reserve_addr = accounts[ACCOUNT_ASSET_RESERVE].address_str()?;
        let manager_addr = accounts[ACCOUNT_ASSET_MANAGER].address_str()?;
        let freeze_addr = accounts[ACCOUNT_ASSET_FREEZE].address_str()?;

        let need_new_asset = new_asset || (asset_id.is_none() && self.asset_id == 0);
        let effective_asset_id = if need_new_asset {
            let id = asset_creator_ops
                .create_asset_configured(
                    asset_name,
                    total_units,
                    &manager_addr,
                    &reserve_addr,
                    &app_addr,
                    &freeze_addr,
                )?
                .ok_or_else(|| anyhow!("create_asset_configured returned no asset_id"))?;
            tracing::info!("[deploy_app_and_asset] created new asset_id={}", id);
            id
        } else {
            let id = asset_id.unwrap_or(self.asset_id);
            // Reconfigure clawback/reserve to point at the (possibly new) app. This is an
            // asset-config operation, so it must be signed by the asset manager (the asset
            // was created with ACCOUNT_ASSET_MANAGER as its manager), not the creator.
            accounts[ACCOUNT_ASSET_MANAGER].set_asset_clawback_to_app(effective_app_id, id)?;
            tracing::info!(
                "[deploy_app_and_asset] using existing asset_id={} reconfigured to app_id={}",
                id,
                effective_app_id
            );
            id
        };

        // ── 3. Opt new app into the ASA (admin-signed contract call) ─────────────
        let admin_ab = AlgoBingle::new(admin_ops.clone(), effective_app_id, effective_asset_id);
        admin_ab.ops.opt_in_app_to_asset(
            effective_app_id,
            effective_asset_id,
            "opt_in_to_bingle(uint64)void",
        )?;

        // Opt APP_WITHDRAWER into the ASA so they can receive ASA withdrawals.
        let withdrawer_ab =
            AlgoBingle::new(withdrawer_ops.clone(), effective_app_id, effective_asset_id);
        withdrawer_ab.opt_in_sender_to_asset(effective_asset_id)?;

        // Opt ASSET_RESERVE into the ASA if it is an external account (not the app address).
        let reserve_ops = &accounts[ACCOUNT_ASSET_RESERVE];
        let res_addr = reserve_ops.address_str()?;
        if res_addr != app_addr {
            let reserve_ab =
                AlgoBingle::new(reserve_ops.clone(), effective_app_id, effective_asset_id);
            reserve_ab.opt_in_sender_to_asset(effective_asset_id)?;
        }

        // ── 3b. Seed the new app with initial_hot_bingle tokens (new asset only) ─────
        if need_new_asset && initial_hot_bingle > 0 {
            asset_creator_ops
                .send_asset(effective_asset_id, initial_hot_bingle, &app_addr)
                .map_err(|e| {
                    anyhow!("deploy_app_and_asset: failed to seed initial_hot_bingle: {e}")
                })?;
            tracing::info!(
                "[deploy_app_and_asset] seeded {} units of asset {} to app {}",
                initial_hot_bingle,
                effective_asset_id,
                effective_app_id
            );
        }

        // ── 4. Transfer old-app balances to new app (only when app changed) ──────────
        if need_new_app && self.app_id > 0 && self.app_id != effective_app_id {
            // ALGO balance: call migrate_reserve on the old app (asset_id=0 skips ASA transfer).
            self.migrate_reserve(self.app_id, effective_app_id, 0)?;
            // ASA balance: clawback via asset manager when the asset is being reused.
            if !need_new_asset {
                self.transfer_old_app_asset_balance(
                    self.app_id,
                    effective_app_id,
                    effective_asset_id,
                    &accounts[ACCOUNT_ASSET_MANAGER],
                )?;
            }
        }

        Ok((effective_app_id, effective_asset_id))
    }

    /// Read `*.approval.teal`, `*.clear.teal` and `*.arc56.json` from `dir`, returning their
    /// contents as `(approval, clear, arc56)`.
    fn read_teal_from_dir(dir: &std::path::Path) -> Result<(String, String, String)> {
        let entries: Vec<std::fs::DirEntry> = std::fs::read_dir(dir)
            .map_err(|e| anyhow!("cannot read app_path {:?}: {e}", dir))?
            .filter_map(|e| e.ok())
            .collect();

        let approval_path = entries
            .iter()
            .find(|e| e.file_name().to_string_lossy().ends_with(".approval.teal"))
            .map(|e| e.path())
            .ok_or_else(|| anyhow!("no *.approval.teal found in {:?}", dir))?;

        let clear_path = entries
            .iter()
            .find(|e| e.file_name().to_string_lossy().ends_with(".clear.teal"))
            .map(|e| e.path())
            .ok_or_else(|| anyhow!("no *.clear.teal found in {:?}", dir))?;

        let arc56_path = entries
            .iter()
            .find(|e| e.file_name().to_string_lossy().ends_with(".arc56.json"))
            .map(|e| e.path())
            .ok_or_else(|| anyhow!("no *.arc56.json found in {:?}", dir))?;

        let approval_src = std::fs::read_to_string(&approval_path)
            .map_err(|e| anyhow!("failed to read {:?}: {e}", approval_path))?;
        let clear_src = std::fs::read_to_string(&clear_path)
            .map_err(|e| anyhow!("failed to read {:?}: {e}", clear_path))?;
        let arc56_json = std::fs::read_to_string(&arc56_path)
            .map_err(|e| anyhow!("failed to read {:?}: {e}", arc56_path))?;

        Ok((approval_src, clear_src, arc56_json))
    }

    /// Clawback any ASA balance held by `old_app_id` to `new_app_id`.
    ///
    /// Steps:
    /// 1. Temporarily set the ASA clawback to the creator (so it can initiate the transfer).
    /// 2. Issue a `ClawbackAsset` from old-app address to new-app address.
    /// 3. Restore clawback + reserve to the new app address via `set_asset_clawback_to_app`.
    fn transfer_old_app_asset_balance(
        &self,
        old_app_id: u64,
        new_app_id: u64,
        asset_id: u64,
        asset_manager: &AlgoOps,
    ) -> Result<()> {
        let old_app_addr_str = self.ops.contract_address(old_app_id)?;
        let balance = self.ops.asset_holding(&old_app_addr_str, asset_id)?;
        if balance == 0 {
            tracing::info!(
                "[deploy_app_and_asset] old app {} has zero balance of asset {}, skipping transfer",
                old_app_id,
                asset_id
            );
            return Ok(());
        }
        tracing::info!(
            "[deploy_app_and_asset] clawbacking {} units of asset {} from old app {} to new app {}",
            balance,
            asset_id,
            old_app_id,
            new_app_id
        );

        // Use the ASA manager's signing key for all asset config and clawback transactions.
        let sk = asset_manager.private_key_bytes()?;
        let seed: [u8; 32] = sk
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("Secret key must be 32 bytes"))?;
        let account = Account::from_seed(seed);
        let caller_str = asset_manager.address_str()?;
        let caller = Address::from_str(&caller_str).map_err(|e| anyhow!("invalid address: {e}"))?;
        let old_app_addr = self.app_address(old_app_id)?;
        let new_app_addr = self.app_address(new_app_id)?;
        let client = self.ops.algod_client()?;

        // Fetch current reserve so we don't accidentally clear it in the AssetConfig step.
        let asset_info = self
            .ops
            .algod_call(|| client.asset(AssetId(asset_id)))
            .map_err(|e| anyhow!("fetch asset info for balance transfer: {e}"))?;
        let v =
            serde_json::to_value(&asset_info).map_err(|e| anyhow!("serialize asset info: {e}"))?;
        let reserve_str = v
            .get("params")
            .and_then(|p| p.get("reserve").and_then(|x| x.as_str()))
            .unwrap_or(&old_app_addr_str);
        let reserve_addr =
            Address::from_str(reserve_str).map_err(|e| anyhow!("parse reserve address: {e}"))?;

        // Step 1: set clawback to creator temporarily.
        let params = self.params(&client)?;
        let tx_cfg = UpdateAsset::new(caller, AssetId(asset_id))
            .manager(caller)
            .reserve(reserve_addr)
            .clawback(caller)
            .note(AlgoOps::unique_note())
            .build(&params)
            .map_err(|e| anyhow!("build AssetConfig (set clawback to creator): {e}"))?;
        let signed_cfg = account
            .sign(tx_cfg)
            .map_err(|e| anyhow!("sign AssetConfig: {e}"))?
            .to_msg_pack()
            .map_err(|e| anyhow!("encode AssetConfig: {e}"))?;
        let tx_id = self
            .ops
            .algod_call(|| client.send_raw(&signed_cfg))
            .map_err(|e| anyhow!("send AssetConfig: {e}"))?
            .tx_id;
        self.ops.wait_for_confirmation(&tx_id, 10)?;

        // Step 2: clawback from old-app to new-app.
        let params2 = self.params(&client)?;
        let tx_clawback = ClawbackAsset::new(
            caller,
            AssetId(asset_id),
            balance,
            old_app_addr,
            new_app_addr,
        )
        .note(AlgoOps::unique_note())
        .build(&params2)
        .map_err(|e| anyhow!("build ClawbackAsset: {e}"))?;
        let signed_cb = account
            .sign(tx_clawback)
            .map_err(|e| anyhow!("sign ClawbackAsset: {e}"))?
            .to_msg_pack()
            .map_err(|e| anyhow!("encode ClawbackAsset: {e}"))?;
        let tx_id2 = self
            .ops
            .algod_call(|| client.send_raw(&signed_cb))
            .map_err(|e| anyhow!("send ClawbackAsset: {e}"))?
            .tx_id;
        self.ops.wait_for_confirmation(&tx_id2, 10)?;

        // Step 3: restore clawback + reserve to new app.
        asset_manager.set_asset_clawback_to_app(new_app_id, asset_id)?;

        tracing::info!(
            "[deploy_app_and_asset] balance transfer complete: {} units moved to new app {}",
            balance,
            new_app_id
        );
        Ok(())
    }

    /// Ensure the sender account is opted-in to the given ASA. If already opted-in, returns Ok("").
    /// Otherwise, sends an asset transfer of 0 units from the sender to themselves to perform opt-in.
    pub fn opt_in_sender_to_asset(&self, asset_id: u64) -> Result<String> {
        if asset_id == 0 {
            bail!("asset_id must be > 0");
        }
        let sender_str = self
            .ops
            .address
            .as_ref()
            .ok_or_else(|| anyhow!("This operation needs an address"))?;
        if self
            .ops
            .is_account_opted_in_to_asset(sender_str, asset_id)?
        {
            return Ok(String::new());
        }
        let client = self.ops.algod_client()?;
        let params = self.params(&client)?;
        let (account, sender) = self.sender_account()?;
        // Opt-in by sending 0 units to self
        let tx = TransferAsset::new(sender, AssetId(asset_id), 0, sender)
            .note(AlgoOps::unique_note())
            .build(&params)
            .map_err(|e| anyhow!("build asset opt-in tx: {e}"))?;
        let signed = account
            .sign(tx)
            .map_err(|e| anyhow!("sign asset opt-in: {e}"))?
            .to_msg_pack()
            .map_err(|e| anyhow!("encode signed asset opt-in: {e}"))?;
        let tx_id = self
            .ops
            .algod_call(|| client.send_raw(&signed))
            .map_err(|e| anyhow!("send_raw failed: {e}"))?
            .tx_id;
        self.ops.wait_for_confirmation(&tx_id, 10)?;
        Ok(tx_id)
    }
}

/// Outcome of the on-chain `allow_relay` gate for a relay record.
///
/// Internal to the relay-acceptance path; not part of the supported public API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RelayGate {
    /// The record is permitted to relay (its on-chain `allow_relay` flag is set).
    Allowed,
    /// The record is not permitted to relay, or the gate could not be evaluated.
    Rejected,
}

/// Verify a record's on-chain `allow_relay` flag before it is accepted as a relay record.
///
/// Resolves the app id, Algorand provider config, and accounts cache from `api`, then checks the
/// app local state via [`AlgoBingle::check_allow_relay`]. Returns [`RelayGate::Rejected`] when any
/// of those are unavailable, when `allow_relay` is false, when the id is not opted in to the app,
/// or when the check errors — otherwise [`RelayGate::Allowed`].
///
/// `caller` appears only in log messages so the originating call site stays identifiable. This is
/// the single source of truth shared by `Engine::ddb_upsert_record` and
/// `DefaultPrintingHandler::on_ddb_upsert_resolve`.
///
/// Internal to the relay-acceptance path; not part of the supported public API.
pub(crate) fn check_relay_allowed(
    api: &dyn BingleApiBoth,
    record_id: &str,
    caller: &str,
) -> RelayGate {
    let app_id = match api.get_app_id() {
        Some(id) => id,
        None => {
            tracing::warn!(
                "[{caller}] app_id not available; rejecting relay record id={record_id}"
            );
            return RelayGate::Rejected;
        }
    };
    let config = match api.get_algo_provider_config() {
        Some(cfg) => cfg,
        None => {
            tracing::warn!(
                "[{caller}] algo_provider_config not available; rejecting relay record id={record_id}"
            );
            return RelayGate::Rejected;
        }
    };
    let cache = api.get_accounts_cache();
    let ops = AlgoOps::new_for_algorand(None, Some(record_id.to_string()), Some(config));
    let algo_bingle = match cache {
        Some(c) => AlgoBingle::new_with_cache(ops, app_id, 0, c),
        None => AlgoBingle::new(ops, app_id, 0),
    };
    match algo_bingle.check_allow_relay(app_id, record_id) {
        Ok(Some(true)) => {
            tracing::info!("[{caller}] blockchain verified allow_relay for id={record_id}");
            RelayGate::Allowed
        }
        Ok(Some(false)) => {
            tracing::warn!(
                "[{caller}] blockchain check FAILED: allow_relay is false for id={record_id}"
            );
            RelayGate::Rejected
        }
        Ok(None) => {
            tracing::warn!(
                "[{caller}] blockchain check FAILED: id={record_id} not opted-in to app {app_id}"
            );
            RelayGate::Rejected
        }
        Err(e) => {
            tracing::warn!("[{caller}] blockchain check ERROR for id={record_id}: {e}");
            RelayGate::Rejected
        }
    }
}
