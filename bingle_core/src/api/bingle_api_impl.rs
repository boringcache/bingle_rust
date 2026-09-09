use data_encoding::BASE32_NOPAD;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::themes;
use serde_json::{Map as JsonMap, Value as JsonValue};
use tracing::warn;
use uuid::Uuid;

use crate::api::bingle_api::{
    BingleApi, BingleError, Handle, NetworkEndpoint, OnConnectHandler, OnMessageHandler,
    ProgressCallback, SendFailureKind, StartOptions, UserId,
};
use crate::api::pki::generate_pki_from_ops;
use crate::blockchain::algo_bingle::AccountsCache;
use crate::dtls::Dtls;
use crate::engine::{BingleAccess, Engine, EngineState};
use crate::protocol::ISSUER_SUFFIX;
use crate::relay::relay_finder::RelayFinderTrait;
use algo_ops::AlgoOps;
use algo_ops::error::{AlgoError, AlgoErrorKind};
use ed25519_dalek::SigningKey;

/// Upper bound on a single hot-path blockchain lookup — the outgoing `handle_lookup`
/// and the incoming `handle_lookup_by_id`. Both resolve a handle via one Indexer/algod
/// query and answer in well under a second when the node is reachable. algonaut 0.8
/// sets no HTTP timeout, so without this an unreachable-but-routable node could block
/// the caller on TCP connect (~75 s) — stalling the drain worker's send or the DTLS
/// auth check (bingle_rust #48). Only these two lookups are bounded; the funding /
/// registration / confirmation paths (which legitimately take longer) are untouched.
const HANDLE_LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Run a blocking lookup `f` on a detached worker thread, bounded by `timeout`, and
/// return its result — or `None` if it doesn't finish in time.
///
/// The worker is intentionally not joined: if the underlying call is genuinely stuck
/// (algonaut's un-timed HTTP client) the thread lingers until the OS/algonaut errors,
/// but the caller has already moved on (fail-fast). Used only for the hot-path handle
/// lookups so a hung node can't wedge message delivery or inbound auth.
///
/// `pub` + `#[doc(hidden)]`: not part of the public API, exposed only so its
/// fail-fast behaviour can be unit-tested from the tests tree.
#[doc(hidden)]
pub fn run_bounded<T, F>(timeout: Duration, label: &'static str, f: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    if std::thread::Builder::new()
        .name(format!("bingle-{label}"))
        .spawn(move || {
            let _ = tx.send(f());
        })
        .is_err()
    {
        tracing::error!(
            "[BingleApiImpl] failed to spawn bounded worker for {}",
            label
        );
        return None;
    }
    match rx.recv_timeout(timeout) {
        Ok(v) => Some(v),
        Err(_) => {
            tracing::warn!(
                "[BingleApiImpl] {} exceeded {:?}; treating as unreachable",
                label,
                timeout
            );
            None
        }
    }
}

// Simple bidirectional cache for handle <-> user_id with per-entry timestamps
// Not exposed publicly; guarded by the encompassing Mutex in BingleApiImpl
struct HandleCacheBi {
    handle_to_id: std::collections::HashMap<Handle, (UserId, Instant)>,
    id_to_handle: std::collections::HashMap<UserId, (Handle, Instant)>,
}

impl HandleCacheBi {
    fn new() -> Self {
        Self {
            handle_to_id: std::collections::HashMap::new(),
            id_to_handle: std::collections::HashMap::new(),
        }
    }

    fn insert(&mut self, handle: Handle, user_id: UserId, now: Instant) {
        // Remove any previous reverse mapping for this handle
        if let Some((old_uid, _)) = self.handle_to_id.remove(&handle)
            && let Some((h2, _)) = self.id_to_handle.get(&old_uid)
            && *h2 == handle
        {
            self.id_to_handle.remove(&old_uid);
        }
        // If this user_id was mapped to a different handle, remove that handle mapping
        if let Some((old_handle, _)) = self.id_to_handle.remove(&user_id)
            && let Some((h_uid, _)) = self.handle_to_id.get(&old_handle)
            && *h_uid == user_id
        {
            self.handle_to_id.remove(&old_handle);
        }
        self.handle_to_id
            .insert(handle.clone(), (user_id.clone(), now));
        self.id_to_handle.insert(user_id, (handle, now));
    }

    fn get_id_by_handle(&mut self, handle: &Handle, expiry: Duration) -> Option<UserId> {
        if let Some((uid, ts)) = self.handle_to_id.get(handle) {
            if ts.elapsed() < expiry {
                return Some(uid.clone());
            }
            // expired: remove both directions
            let uid = uid.clone();
            self.handle_to_id.remove(handle);
            if let Some((h, _)) = self.id_to_handle.get(&uid)
                && h == handle
            {
                self.id_to_handle.remove(&uid);
            }
        }
        None
    }

    fn get_handle_by_id(&mut self, user_id: &UserId, expiry: Duration) -> Option<Handle> {
        if let Some((handle, ts)) = self.id_to_handle.get(user_id) {
            if ts.elapsed() < expiry {
                return Some(handle.clone());
            }
            // expired: remove both directions
            let handle = handle.clone();
            self.id_to_handle.remove(user_id);
            if let Some((uid, _)) = self.handle_to_id.get(&handle)
                && uid == user_id
            {
                self.handle_to_id.remove(&handle);
            }
        }
        None
    }
}

/// Concrete implementation of the BingleApi trait.
///
/// Minimal functionality implemented per task requirements:
/// - start: instantiate a DTLS implementation (DtlsOpenSsl on non-iOS) but do not start the accept loop (no address yet).
/// - send_message_to_network: when given a direct socket address, call DTLS send with the JSON message bytes.
pub struct BingleApiImpl {
    // Interior-mutable so the lifecycle/setter methods can take &self (the API is shared via
    // Arc/Weak; see Stage 2). These mirror the Engine's interior-mutability idiom and are never
    // locked across I/O.
    on_message: Mutex<Option<Arc<OnMessageHandler>>>,
    on_connect: Mutex<Option<Arc<OnConnectHandler>>>,
    started_options: RwLock<StartOptions>,
    // Shared on_message handler accessible from Engine/DTLS callback without needing &self
    shared_on_message: Arc<Mutex<Option<Arc<OnMessageHandler>>>>,
    // Optional on_listening handler
    on_listening: Mutex<Option<Arc<crate::api::bingle_api::OnListeningHandler>>>,
    // Engine instance for endpoint identification and DTLS/mux lifecycle. Owned 1:1 by this API;
    // the Engine's background threads re-enter it via the shared Weak<dyn BingleApiBoth> handle.
    engine: Engine,

    // Per-API router to avoid global cross-talk
    router: Mutex<Option<Arc<crate::messages::router::Router>>>,
    // Weak reference to ourselves for passing to components
    this: crate::api::bingle_api::BingleApiBothType,
    handle_lookup_mock:
        Mutex<Option<Box<dyn Fn(&Handle) -> Result<Option<UserId>, String> + Send + Sync>>>,
    // Test seam for reverse lookup (id -> handle) without network
    id_to_handle_lookup_mock:
        Mutex<Option<Box<dyn Fn(&UserId) -> Result<Option<Handle>, String> + Send + Sync>>>,
    handle_cache: Mutex<HandleCacheBi>,
    accounts_cache: Arc<Mutex<AccountsCache>>,
    span: Mutex<tracing::Span>,
}

impl BingleApiImpl {
    /// Clone of the current tracing span (held under a short-lived lock).
    fn span(&self) -> tracing::Span {
        self.span.lock().expect("span lock").clone()
    }

    /// Read guard over the started options (read-heavy; written once at start()).
    fn opts(&self) -> std::sync::RwLockReadGuard<'_, StartOptions> {
        self.started_options.read().expect("options lock")
    }

    fn check_dangerous_debug(options: &StartOptions) {
        if options.dangerous_debug && !cfg!(debug_assertions) {
            panic!("dangerous_debug is only allowed in debug builds");
        }
    }

    /// Construct a messaging engine from the given start options.
    ///
    /// This builds a real engine wired to the network and Algorand integration, so it is the
    /// single supported entry point for creating a [`BingleApiImpl`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use bingle_core::api::bingle_api::StartOptions;
    /// use bingle_core::api::bingle_api_impl::BingleApiImpl;
    ///
    /// let api = BingleApiImpl::new(&StartOptions::new("alice".to_string()));
    /// ```
    pub fn new(options: &StartOptions) -> Arc<Self> {
        tracing::info!("[BingleApiImpl::new][enter]");
        Self::check_dangerous_debug(options);
        let initial_options = options.clone();
        Arc::<Self>::new_cyclic(|me| {
            let me_both = me.clone();
            let engine = Engine::new(&initial_options, me_both.clone());
            Self {
                on_message: Mutex::new(None),
                on_connect: Mutex::new(None),
                started_options: RwLock::new(initial_options),
                shared_on_message: Arc::new(Mutex::new(None)),
                on_listening: Mutex::new(None),
                engine,
                router: Mutex::new(None),
                this: me_both,
                handle_lookup_mock: Mutex::new(None),
                id_to_handle_lookup_mock: Mutex::new(None),
                handle_cache: Mutex::new(HandleCacheBi::new()),
                accounts_cache: Arc::new(Mutex::new(AccountsCache::default())),
                span: Mutex::new(tracing::Span::none()),
            }
        })
    }
}

impl BingleApiImpl {
    /// Apply a closure to the owned Engine instance (test-only). The Engine is fully `&self`, so a
    /// shared reference is sufficient; the name is retained for existing call sites.
    #[doc(hidden)]
    pub fn with_engine_mut<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Engine) -> R,
    {
        f(&self.engine)
    }

    /// Test-oriented constructor to inject a custom DTLS implementation.
    #[doc(hidden)]
    pub fn new_with_dtls(dtls: Box<dyn Dtls + Send + Sync>) -> Arc<Self> {
        Self::new_with_dtls_and_options(dtls, StartOptions::new("".into()))
    }

    /// Test-oriented constructor to inject custom DTLS and options.
    #[doc(hidden)]
    pub fn new_with_dtls_and_options(
        dtls: Box<dyn Dtls + Send + Sync>,
        options: StartOptions,
    ) -> Arc<Self> {
        tracing::info!(
            "[BingleApiImpl::new_with_dtls_and_options][enter] dtls_provided=true am_relay={}",
            options.am_relay
        );
        Self::check_dangerous_debug(&options);
        Arc::<Self>::new_cyclic(|me| {
            let me_both = me.clone();
            let engine = Engine::new_with_dtls(&options, me_both.clone(), dtls);
            Self {
                on_message: Mutex::new(None),
                on_connect: Mutex::new(None),
                started_options: RwLock::new(options),
                shared_on_message: Arc::new(Mutex::new(None)),
                on_listening: Mutex::new(None),
                engine,
                router: Mutex::new(None),
                this: me_both,
                handle_lookup_mock: Mutex::new(None),
                id_to_handle_lookup_mock: Mutex::new(None),
                handle_cache: Mutex::new(HandleCacheBi::new()),
                accounts_cache: Arc::new(Mutex::new(AccountsCache::default())),
                span: Mutex::new(tracing::Span::none()),
            }
        })
    }

    /// Test-only helper: override issuer directly for unit/integration tests.
    /// Not part of the stable API surface.
    #[doc(hidden)]
    pub fn set_issuer_for_tests(&self, issuer: String) {
        tracing::info!(
            "[BingleApiImpl::set_issuer_for_tests][enter] issuer_len={}",
            issuer.len()
        );
        self.engine.set_issuer(issuer);
        tracing::info!("[BingleApiImpl::set_issuer_for_tests][exit]");
    }

    /// Test helpers to access the Engine from integration tests (not part of stable API).
    #[doc(hidden)]
    pub fn engine_state_for_tests(&self) -> Option<EngineState> {
        tracing::trace!("[BingleApiImpl::engine_state_for_tests][enter]");
        let s = Some(self.engine.access(|e| e.state()));
        tracing::trace!(
            "[BingleApiImpl::engine_state_for_tests][exit] state={:?}",
            s
        );
        s
    }
    #[doc(hidden)]
    pub fn engine_nat_type_for_tests(&self) -> Option<crate::engine::NatType> {
        tracing::info!("[BingleApiImpl::engine_nat_type_for_tests][enter]");
        let t = Some(self.engine.access(|e| e.nat_type()));
        tracing::info!(
            "[BingleApiImpl::engine_nat_type_for_tests][exit] nat_type={:?}",
            t
        );
        t
    }
    #[doc(hidden)]
    pub fn engine_last_public_addr_for_tests(&self) -> Option<SocketAddr> {
        tracing::info!("[BingleApiImpl::engine_last_public_addr_for_tests][enter]");
        let a = self.engine.access(|e| e.last_public_addr());
        tracing::info!(
            "[BingleApiImpl::engine_last_public_addr_for_tests][exit] addr={:?}",
            a
        );
        a
    }
    #[doc(hidden)]
    pub fn engine_local_bind_addr_for_tests(&self) -> Option<SocketAddr> {
        tracing::info!("[BingleApiImpl::engine_local_bind_addr_for_tests][enter]");
        let a = self.engine.access(|e| e.local_bind_addr_for_tests());
        tracing::info!(
            "[BingleApiImpl::engine_local_bind_addr_for_tests][exit] addr={:?}",
            a
        );
        a
    }
    #[doc(hidden)]
    pub fn engine_mux_for_tests(&self) -> Option<Arc<crate::dtls::UdpNetworkMux>> {
        self.engine.access(|e| e.mux_for_tests())
    }
    #[doc(hidden)]
    pub fn engine_receive_message_for_tests(&self, from_ep: &NetworkEndpoint, data: &[u8]) {
        self.engine.receive_message_for_tests(from_ep, data);
    }
    #[doc(hidden)]
    pub fn engine_ddb_lookup_for_tests(&self, id: &str) -> Result<NetworkEndpoint, BingleError> {
        tracing::info!(
            "[BingleApiImpl::engine_ddb_lookup_for_tests][enter] id={}",
            id
        );
        let res = self.engine.access(|e| e.ddb_client().lookup(id));
        tracing::info!(
            "[BingleApiImpl::engine_ddb_lookup_for_tests][exit] res={:?}",
            res.as_ref().ok()
        );
        res
    }

    #[doc(hidden)]
    pub fn engine_set_ddb_client_for_tests(&self, ddb: Arc<dyn crate::ddb::DdbClient>) {
        tracing::info!("[BingleApiImpl::engine_set_ddb_client_for_tests][enter]");
        self.engine.set_ddb_client_for_tests(ddb);
    }

    #[doc(hidden)]
    pub fn engine_set_retry_delays_for_tests(&self, delays: Vec<Duration>) {
        self.engine.set_retry_delays_for_packet_transport(delays);
    }

    /// Build an AlgoBingle configured for public Indexer queries (handle lookups).
    /// Indexer queries are public, so this uses the node's own id when available, or a
    /// dummy address otherwise, purely to satisfy AlgoOps::new requirements.
    fn build_indexer_algo_bingle(
        &self,
    ) -> Result<crate::blockchain::algo_bingle::AlgoBingle, BingleError> {
        let app_id = self
            .get_app_id()
            .ok_or_else(|| BingleError::Other("app_id not configured".to_string()))?;
        let config = self
            .get_algo_provider_config()
            .ok_or_else(|| BingleError::Other("algo_provider_config not configured".to_string()))?;
        // Handle lookups only issue public Indexer queries, so no account is needed.
        let ops = AlgoOps::new_for_algorand(None, None, Some(config));
        Ok(crate::blockchain::algo_bingle::AlgoBingle::new_with_cache(
            ops,
            app_id,
            0,
            self.accounts_cache.clone(),
        ))
    }

    #[doc(hidden)]
    pub fn set_handle_lookup_mock_for_tests(
        &self,
        mock: Box<dyn Fn(&Handle) -> Result<Option<UserId>, String> + Send + Sync>,
    ) {
        let mut m = self.handle_lookup_mock.lock().unwrap();
        *m = Some(mock);
    }

    #[doc(hidden)]
    pub fn set_id_to_handle_lookup_mock_for_tests(
        &self,
        mock: Box<dyn Fn(&UserId) -> Result<Option<Handle>, String> + Send + Sync>,
    ) {
        let mut m = self.id_to_handle_lookup_mock.lock().unwrap();
        *m = Some(mock);
    }

    /// Test-only accessor to the owned Engine (for white-box integration tests).
    #[doc(hidden)]
    pub fn engine_for_tests(&self) -> &Engine {
        &self.engine
    }
    #[doc(hidden)]
    pub fn engine_force_stun_consistent_for_tests(&mut self, addr: SocketAddr) {
        tracing::info!(
            "[BingleApiImpl::engine_force_stun_consistent_for_tests][enter] addr={}",
            addr
        );
        self.engine.test_force_stun_consistent(addr);
        tracing::info!("[BingleApiImpl::engine_force_stun_consistent_for_tests][exit]");
    }

    /// Test-only accessor to the Engine's TURN handler (for white-box integration tests)
    #[doc(hidden)]
    pub fn engine_turn_client_handler_for_tests(
        &self,
    ) -> Arc<crate::turn::turn_client_handler_impl::TurnClientHandlerImpl> {
        self.engine.access(|e| e.turn_client_handler_for_tests())
    }
    #[doc(hidden)]
    pub fn engine_turn_relay_handler_for_tests(
        &self,
    ) -> Arc<crate::turn::turn_relay_handler_impl::TurnRelayHandlerImpl> {
        self.engine.access(|e| e.turn_relay_handler_for_tests())
    }

    /// Test-only: set the engine's last public address (for self-send guard tests).
    #[doc(hidden)]
    pub fn engine_set_public_addr_for_tests(&mut self, addr: Option<SocketAddr>) {
        self.engine.set_last_public_addr(addr);
    }

    /// Exposed for integration tests: whether a DTLS instance has been created.
    #[doc(hidden)]
    pub fn has_dtls(&self) -> bool {
        tracing::info!("[BingleApiImpl::has_dtls][enter]");
        // Engine now always has a DTLS instance initialized in new()
        tracing::info!("[BingleApiImpl::has_dtls][exit] return=true");
        true
    }

    fn ensure_dtls(&self) {
        // No longer needed as Engine always has a DTLS instance.
    }

    fn send_over_dtls(
        &self,
        nsk: &NetworkEndpoint,
        message: JsonValue,
    ) -> Result<bool, BingleError> {
        tracing::info!(
            "[BingleApiImpl::send_over_dtls][enter] nsk={:?}, message={:?}",
            nsk,
            message
        );
        // Guard: reject incomplete relay endpoints (missing channel); fully-configured
        // relay endpoints (with channel+address) are handled by the TURN layer in DTLS.
        if nsk.is_relay() && nsk.relay_channel().is_none() {
            warn!(
                "[BingleApiImpl::send_over_dtls] rejecting incomplete relay endpoint (no channel): {}",
                nsk
            );
            return Ok(false);
        }
        // Guard: do not send to ourselves
        if let Some(target_addr) = nsk.inet_socket_address() {
            let my_addr = self.engine.access(|e| e.last_public_addr());
            if my_addr == Some(target_addr) {
                warn!(
                    "[BingleApiImpl::send_over_dtls] rejecting send to self: {}",
                    target_addr
                );
                return Ok(false);
            }
        }
        let bytes =
            serde_json::to_vec(&message).expect("Failed to serialize message to JSON bytes");
        match self.engine.access(|e| e.send_to_peer(nsk, &bytes)) {
            Ok(_) => Ok(true),
            Err(err) => {
                warn!("[BingleApiImpl] Engine send_to_peer failed: {}", err);
                if err.contains("rejecting") {
                    Ok(false)
                } else {
                    // The endpoint resolved but the peer could not be reached over DTLS (connect
                    // timeout / no route — peer offline): a transient, retryable cause (issue #99).
                    Err(BingleError::Send {
                        kind: SendFailureKind::PeerUnreachable,
                        detail: err,
                    })
                }
            }
        }
    }
}

impl Drop for BingleApiImpl {
    fn drop(&mut self) {
        // Ensure background threads and network mux are stopped to avoid use-after-free across tests
        <BingleApiImpl as BingleApi>::stop(self);
    }
}

const DEFAULT_WAIT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(90);

impl BingleApi for BingleApiImpl {
    fn debug_print_options(&self) {
        let span = self.span();
        let _guard = span.enter();
        info_theme!(
            themes::API,
            "[BingleApiImpl::debug_print_options] started_options={}",
            *self.opts()
        );
    }
    fn list_all_relays(&self, include_self: bool) -> Vec<crate::relay::relay_finder::RelayInfo> {
        let span = self.span();
        let _guard = span.enter();
        info_theme!(
            themes::API,
            "[BingleApiImpl::list_all_relays] include_self={}",
            include_self
        );
        // Delegate to Engine's relay_finder-backed implementation
        let res = self.engine.access(|e| e.list_all_relays(include_self));
        info_theme!(
            themes::API,
            "[BingleApiImpl::list_all_relays] return={:?}",
            res
        );
        res
    }
    fn get_my_id(&self) -> Option<String> {
        let span = self.span();
        let _guard = span.enter();
        // Prefer issuer from Engine (issuer = id + ISSUER_SUFFIX). Trim suffix to return pure id.
        match self
            .engine
            .access(|e| e.issuer().map(|iss| iss.to_string()))
        {
            Ok(iss) => Some(iss.trim_end_matches(ISSUER_SUFFIX).to_string()),
            Err(e) => {
                warn_theme!(themes::API, "[BingleApiImpl::get_my_id] {}", e);
                None
            }
        }
    }
    fn get_user_id(&self) -> Option<String> {
        self.get_my_id()
    }
    fn get_handle(&self) -> Option<String> {
        let h = self.opts().handle.clone();
        if h.is_empty() { None } else { Some(h) }
    }
    fn get_app_id(&self) -> Option<u64> {
        self.opts().app_id
    }
    fn get_algo_provider_config(&self) -> Option<algo_ops::AlgoChainConfig> {
        self.opts().algo_provider_config.clone()
    }
    fn get_accounts_cache(&self) -> Option<Arc<Mutex<AccountsCache>>> {
        Some(self.accounts_cache.clone())
    }

    fn clear_accounts_cache(&self) {
        algo_log!("[BingleApiImpl] clearing accounts cache");
        let mut cache = self.accounts_cache.lock().unwrap();
        cache.clear();
    }
    fn handle_lookup_by_id(&self, user_id: &UserId) -> Option<Handle> {
        // Delegate to inherent reverse-lookup with caching/blockchain fallback
        BingleApiImpl::handle_lookup_by_id(self, user_id)
    }
    fn start(&self, options: &StartOptions) -> Result<(), BingleError> {
        let span = tracing::info_span!("BingleApi", handle = %options.handle);
        *self.span.lock().expect("span lock") = span.clone();
        let _guard = span.enter();

        Self::check_dangerous_debug(options);

        // Algorand node connectivity check (fail-fast per requirement)
        if let Some(config) = &options.algo_provider_config {
            let ops = AlgoOps::new_for_algorand(
                options.algo_passphrase.clone(),
                None,
                Some(config.clone()),
            );
            if let Err(e) = ops.account_balance() {
                if let Some(ae) = e.downcast_ref::<AlgoError>()
                    && ae.kind == AlgoErrorKind::HostUnreachable
                {
                    tracing::error!(
                        "[BingleApiImpl::start] Algorand node is unreachable: {}",
                        ae
                    );
                    return Err(BingleError::Algo(ae.clone()));
                }
                tracing::warn!(
                    "[BingleApiImpl::start] Algorand node check failed (but not unreachable): {}",
                    e
                );
            }
        }

        // allow_relay check for relays (soft check)
        if options.am_relay
            && let (Some(config), Some(app_id), Some(pass)) = (
                &options.algo_provider_config,
                options.app_id,
                &options.algo_passphrase,
            )
        {
            let ops = AlgoOps::new_for_algorand(Some(pass.clone()), None, Some(config.clone()));
            if let Some(addr) = ops.address.clone() {
                let bingle = crate::blockchain::algo_bingle::AlgoBingle::new(
                    ops,
                    app_id,
                    options.asset_id.unwrap_or(0),
                );
                match bingle.check_allow_relay(app_id, &addr) {
                    Ok(Some(true)) => {
                        // Allowed, continue
                    }
                    Ok(Some(false)) => {
                        tracing::error!(
                            "[BingleApiImpl::start] Account {} is not allowed to relay in dApp {}",
                            addr,
                            app_id
                        );
                        return Err(BingleError::Other(format!(
                            "Account {} is not allowed to relay",
                            addr
                        )));
                    }
                    Ok(None) => {
                        tracing::error!(
                            "[BingleApiImpl::start] Account {} is not opted-in to dApp {}",
                            addr,
                            app_id
                        );
                        return Err(BingleError::Other(format!(
                            "Account {} is not opted-in to dApp",
                            addr
                        )));
                    }
                    Err(e) => {
                        tracing::warn!(
                            "[BingleApiImpl::start] Could not verify allow_relay on-chain: {}",
                            e
                        );
                    }
                }
            }
        }

        self.engine.set_span(self.span());

        info_theme!(
            themes::API,
            "[BingleApiImpl::start][enter] options={}",
            options
        );
        // Persist options and create a DTLS instance (not starting acceptor yet), then initialize PKI.
        *self.started_options.write().expect("options lock") = options.clone();
        self.ensure_dtls();

        // Initialize AlgoOps from provided algoPassphrase if available.
        if let Some(pass) = options.algo_passphrase.clone() {
            // Build AlgoOps with passphrase; it will derive and populate the address.
            let ops =
                AlgoOps::new_for_algorand(Some(pass), None, options.algo_provider_config.clone());
            // Ensure we have an address; if not, force an early error consistent with private_key_bytes failure.
            let addr = match ops.address.clone() {
                Some(a) => a,
                None => {
                    let err = ops
                        .private_key_bytes()
                        .err()
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "unknown error".to_string());
                    return Err(BingleError::Other(format!(
                        "Failed to get private key bytes from passphrase: {}",
                        err
                    )));
                }
            };
            let issuer = format!("{}{}", addr, ISSUER_SUFFIX);
            self.engine.set_issuer(issuer.clone());

            // Generate certificates: CA = Ed25519 self-signed using Algorand key; server/client = RSA-2048 signed by CA (SHA-512).
            match generate_pki_from_ops(&ops) {
                Ok((ca_pem, server_cert_pem, server_key_pem, client_cert_pem, client_key_pem)) => {
                    {
                        self.engine.with_dtls(|dtls: &(dyn Dtls + Send + Sync)| {
                            dtls.set_ca_cert(Some(ca_pem));
                            dtls.set_server_signing_cert(Some(server_cert_pem));
                            dtls.set_server_signing_private_key(Some(server_key_pem));
                            dtls.set_client_cert(Some(client_cert_pem));
                            dtls.set_client_private_key(Some(client_key_pem));
                            // Install a peer certificate handler in all cases.
                            dtls.set_handle_peer_certificate(Some(
                                crate::protocol::cert_verify::peer_certificate_handler(),
                            ));
                            // Accept during handshake and validate at the application layer for API flows
                            dtls.set_app_layer_only_verification(false);
                        });
                    }
                }
                Err(e) => {
                    return Err(BingleError::Other(format!(
                        "PKI initialization failed: {}",
                        e
                    )));
                }
            }
        }

        // Engine will handle incoming DTLS messages; no API-level DTLS handler required

        // Start Engine using the provided StartOptions and propagate any errors
        // Create a per-API Router instance and bind delegating API handle, sender, and internal controls
        let router_arc: Arc<crate::messages::router::Router> = {
            let router = Arc::new(crate::messages::router::Router::new(self.this.clone()));
            // Sender closure routes via the delegating API handle
            let this_weak = self.this.clone();
            let sender_cb: Arc<
                dyn Fn(&NetworkEndpoint, &UserId, serde_json::Value) -> bool
                    + Send
                    + Sync
                    + 'static,
            > = Arc::new(move |nsk, uid, msg| {
                tracing::info!(
                    "[BingleApiImpl::start][sender_cb] nsk={} uid={} msg={}",
                    nsk,
                    uid,
                    msg
                );
                let progress_cb = Arc::new(|percent: u8, message: String| {
                    tracing::info!(
                        "[BingleApiImpl::start][router sender] Send progress: {}% - {}",
                        percent,
                        message
                    );
                });
                if let Some(api) = this_weak.upgrade() {
                    api.access(|a| {
                        a.send_message_to_network(nsk, uid, msg, Some(progress_cb))
                            .unwrap_or(false)
                    })
                } else {
                    tracing::error!("[BingleApiImpl::start][sender_cb] this_weak upgrade failed");
                    false
                }
            });
            router.set_sender(Some(sender_cb));
            router
        };
        *self.router.lock().expect("router lock") = Some(router_arc.clone());
        // If an on_message handler was set prior to start(), propagate it to the newly created router now
        let pending_on_message = self.on_message.lock().expect("on_message lock").clone();
        if let Some(h) = pending_on_message {
            router_arc.set_on_message(Some(h));
        }

        // Install Engine callback to send via Bingle protocol using the delegating API handle
        let this_weak_for_engine = self.this.clone();
        {
            self.engine.set_send_via_bingle(Some(Arc::new(move |nsk, uid, msg| {
                tracing::info!(
                    "[BingleApiImpl::start][engine set send] nsk={} uid={} msg={}",
                    nsk,
                    uid,
                    msg
                );
                let progress_cb = Arc::new(|percent: u8, message: String| {
                    tracing::info!(
                        "[BingleApiImpl::start][engine sender] Send progress: {}% - {}",
                        percent,
                        message
                    );
                });
                if let Some(api) = this_weak_for_engine.upgrade() {
                    api.access(|a| {
                        a.send_message_to_network(nsk, uid, msg, Some(progress_cb))
                            .unwrap_or(false)
                    })
                } else {
                    tracing::error!(
                        "[BingleApiImpl::start][engine sender] this_weak_for_engine upgrade failed"
                    );
                    false
                }
            })));
            // Provide the BingleApi handle to Engine for handlers and DDB client
            self.engine.set_bingle_api(self.this.clone());
            // Provide per-API router to the Engine for routing context
            self.engine.set_router(router_arc.clone());
            self.engine.start(options)?;
        }

        tracing::info!("[BingleApiImpl::start][exit] Ok(())");
        Ok(())
    }

    fn stop(&self) {
        tracing::info!(
            "[BingleApiImpl::stop][enter] {:?}:{:?}",
            self.engine.issuer(),
            self.engine.last_public_addr()
        );
        // Notify listeners that we are no longer listening
        let on_listening = self.on_listening.lock().expect("on_listening lock").clone();
        if let Some(cb) = on_listening {
            cb(false, crate::engine::NatType::Unknown);
        }
        // Stop Engine
        self.engine.stop();
        tracing::info!(
            "[BingleApiImpl::stop][exit] {:?}:{:?}",
            self.engine.issuer(),
            self.engine.last_public_addr()
        );
    }

    fn network_change(&self) {
        tracing::info!("[BingleApiImpl::network_change][enter]");
        // Placeholder: in a full implementation, we would rescan STUN/static IP and update listeners.
        tracing::info!("[BingleApiImpl::network_change][exit]");
    }

    fn handle_lookup(&self, handle: &Handle) -> Result<Option<UserId>, BingleError> {
        let span = self.span();
        let _guard = span.enter();
        info_theme!(
            themes::API,
            "[BingleApiImpl::handle_lookup][enter] handle={}",
            handle
        );

        let expiry_duration = self
            .opts()
            .handle_cache_expiry
            .unwrap_or(Duration::from_secs(600)); // Default 10 minutes

        // Check cache
        if let Ok(mut cache) = self.handle_cache.lock()
            && let Some(user_id) = cache.get_id_by_handle(handle, expiry_duration)
        {
            tracing::info!(
                "[BingleApiImpl::handle_lookup] cache hit for handle {}: {}",
                handle,
                user_id
            );
            return Ok(Some(user_id));
        }

        {
            let mock = self.handle_lookup_mock.lock().unwrap();
            if let Some(m) = mock.as_ref() {
                let res = m(handle).map_err(BingleError::from);
                // Update cache on success from mock too
                if let Ok(Some(ref user_id)) = res
                    && let Ok(mut cache) = self.handle_cache.lock()
                {
                    cache.insert(handle.clone(), user_id.clone(), Instant::now());
                    tracing::info!(
                        "[BingleApiImpl::handle_lookup] cache updated from mock for handle {}: {}",
                        handle,
                        user_id
                    );
                }
                return res;
            }
        }

        let ab = self.build_indexer_algo_bingle()?;
        // Bound the blockchain query so an unreachable node can't wedge the caller
        // (e.g. the drain worker's send). A timeout is surfaced as a Retryable
        // "unreachable" error so the send path keeps the message pending and retries,
        // rather than marking it permanently failed (bingle_rust #48).
        let handle_owned = handle.clone();
        let res = match run_bounded(HANDLE_LOOKUP_TIMEOUT, "handle_lookup", move || {
            ab.handle_lookup(&handle_owned)
                .map_err(BingleError::from_anyhow)
        }) {
            Some(r) => r,
            None => Err(BingleError::Retryable(format!(
                "handle lookup for '{handle}' timed out; node unreachable"
            ))),
        };

        // Update cache on success
        if let Ok(Some(ref user_id)) = res
            && let Ok(mut cache) = self.handle_cache.lock()
        {
            cache.insert(handle.clone(), user_id.clone(), Instant::now());
            info_theme!(
                themes::API,
                "[BingleApiImpl::handle_lookup] cache updated for handle {}: {}",
                handle,
                user_id
            );
        }

        info_theme!(
            themes::API,
            "[BingleApiImpl::handle_lookup][exit] return={:?}",
            res
        );
        res
    }

    fn handle_lookup_partial(
        &self,
        handle: &Handle,
    ) -> Result<Option<(UserId, Handle)>, BingleError> {
        let span = self.span();
        let _guard = span.enter();
        info_theme!(
            themes::API,
            "[BingleApiImpl::handle_lookup_partial][enter] handle={}",
            handle
        );

        // Partial lookups are prefix matches, so the exact-handle cache does not apply.
        let ab = self.build_indexer_algo_bingle()?;
        let res = ab
            .handle_lookup_partial(handle)
            .map_err(BingleError::from_anyhow);

        info_theme!(
            themes::API,
            "[BingleApiImpl::handle_lookup_partial][exit] return={:?}",
            res
        );
        res
    }

    fn send_message_to_id(
        &self,
        user_id: &UserId,
        message: JsonValue,
        progress: Option<Arc<ProgressCallback>>,
    ) -> Result<bool, BingleError> {
        info_theme!(
            themes::API,
            "[BingleApiImpl::send_message_to_id][enter] user_id={} msg={} progress={}",
            user_id,
            message,
            progress.is_some()
        );
        if let Some(cb) = progress.as_ref() {
            cb(5, "Starting lookup".to_string());
        }
        // Engine is ready
        if let Some(cb) = progress.as_ref() {
            cb(10, "Engine ready".to_string());
        }

        // 1) Try to resolve as a known root relay via RelayFinder::lookup_root_id first
        {
            if let Some(cb) = progress.as_ref() {
                cb(15, "Checking root relays".to_string());
            }
            let app_id_opt = self.get_app_id().or_else(|| {
                std::env::var("BINGLE_APP_ID")
                    .ok()
                    .and_then(|s| s.parse::<u64>().ok())
            });
            if let Some(app_id) = app_id_opt {
                let cfg = self.get_algo_provider_config();
                let discover = crate::relay::discovery::indexer_discover_closure(
                    app_id,
                    cfg,
                    Some(self.accounts_cache.clone()),
                );
                // Use self.this for RelayFinder
                let finder =
                    crate::relay::relay_finder::RelayFinder::new(self.this.clone(), discover);
                if let Some(nsk) = finder.lookup_root_id(user_id) {
                    if let Some(cb) = progress.as_ref() {
                        cb(30, format!("Resolved via root: {}", nsk));
                    }
                    let ok =
                        self.send_message_to_network(&nsk, user_id, message, progress.clone())?;
                    info_theme!(
                        themes::API,
                        "[BingleApiImpl::send_message_to_id][exit] return={}",
                        ok
                    );
                    return Ok(ok);
                } else {
                    if let Some(cb) = progress.as_ref() {
                        cb(20, "Root not known; falling back to DDB".to_string());
                    }
                }
            }
        }

        // 2) Fallback to DDB lookup as previously
        let ddb = self.engine.access(|e| e.ddb_client());
        if let Some(cb) = progress.as_ref() {
            cb(20, "Looking up recipient via DDB".to_string());
        }
        match ddb.lookup(user_id) {
            Ok(nsk) => {
                if let Some(cb) = progress.as_ref() {
                    cb(40, format!("DDB lookup ok: {}", nsk));
                }
                let ok = self.send_message_to_network(&nsk, user_id, message, progress.clone())?;
                info_theme!(
                    themes::API,
                    "[BingleApiImpl::send_message_to_id][exit] return={}",
                    ok
                );
                Ok(ok)
            }
            Err(err) => {
                warn!(
                    "[BingleApiImpl::send_message_to_id] DDB lookup failed: {}",
                    err
                );
                if let Some(cb) = progress.as_ref() {
                    cb(100, format!("DDB lookup failed: {}", err));
                }
                tracing::info!("[BingleApiImpl::send_message_to_id][exit] return=error");
                Err(err)
            }
        }
    }

    fn send_message_to_handle(
        &self,
        handle: &Handle,
        message: JsonValue,
        progress: Option<Arc<ProgressCallback>>,
    ) -> Result<bool, BingleError> {
        let span = self.span();
        let _guard = span.enter();
        info_theme!(
            themes::API,
            "[BingleApiImpl::send_message_to_handle][enter] handle={} msg={} progress={}",
            handle,
            message,
            progress.is_some()
        );
        let user_id_opt = match self.handle_lookup(handle) {
            Ok(uid) => uid,
            Err(e) => {
                warn!(
                    "[BingleApiImpl::send_message_to_handle] handle lookup failed for handle {}: {}",
                    handle, e
                );
                if let Some(cb) = progress.as_ref() {
                    cb(100, format!("Handle lookup failed: {}", e));
                }
                // The handle could not be resolved because the blockchain/node call errored — a
                // transient condition while the node is unreachable (issue #99). Preserve any
                // already-typed cause; otherwise classify as a handle-lookup failure.
                return Err(match e {
                    BingleError::Send { .. } => e,
                    other => BingleError::Send {
                        kind: SendFailureKind::HandleLookupFailed,
                        detail: other.to_string(),
                    },
                });
            }
        };

        if let Some(user_id) = user_id_opt {
            if let Some(cb) = progress.as_ref() {
                cb(10, format!("Handle {} resolved to {}", handle, user_id));
            }
            let ok = self.send_message_to_id(&user_id, message, progress)?;
            tracing::info!(
                "[BingleApiImpl::send_message_to_handle][exit] return={}",
                ok
            );
            Ok(ok)
        } else {
            warn!(
                "[BingleApiImpl::send_message_to_handle] handle not found: {}",
                handle
            );
            if let Some(cb) = progress.as_ref() {
                cb(100, format!("Handle not found: {}", handle));
            }
            tracing::info!("[BingleApiImpl::send_message_to_handle][exit] return=HandleNotFound");
            // The handle is not registered on chain: a permanent failure. Return a typed cause
            // rather than the ambiguous `Ok(false)` so the client can distinguish "no such handle"
            // from a transient delivery failure (issue #99).
            Err(BingleError::Send {
                kind: SendFailureKind::HandleNotFound,
                detail: format!("handle not found: {}", handle),
            })
        }
    }

    fn send_message_to_network(
        &self,
        network_source_key: &NetworkEndpoint,
        user_id: &UserId,
        message: JsonValue,
        progress: Option<Arc<ProgressCallback>>,
    ) -> Result<bool, BingleError> {
        let span = self.span();
        let _guard = span.enter();
        info_theme!(
            themes::API,
            "[BingleApiImpl::send_message_to_network][enter] nsk={} user_id={} msg={} progress={}",
            network_source_key,
            user_id,
            message,
            progress.is_some()
        );
        if let Some(cb) = progress.as_ref() {
            cb(10, "Preparing send".to_string());
        }
        // Validate user_id is an Algorand address (base32 without padding) that decodes to 36 bytes
        let user_id_valid = match BASE32_NOPAD.decode(user_id.as_bytes()) {
            Ok(bytes) if bytes.len() == 36 => true,
            Ok(bytes) => {
                warn!(
                    "[BingleApiImpl::send_message_to_network][ERROR] invalid user_id: base32 decoded length {} (expected 36)",
                    bytes.len()
                );
                false
            }
            Err(e) => {
                warn!(
                    "[BingleApiImpl::send_message_to_network][ERROR] invalid user_id: base32 decode failed: {}",
                    e
                );
                false
            }
        };

        let ok_res = if user_id_valid {
            // If this is a relay endpoint missing a channel, allocate one via RelayClient::call
            let mut effective_nsk = network_source_key.clone();
            if effective_nsk.relay_id().is_some() && effective_nsk.relay_channel().is_none() {
                // Detect self-relay: if the relay_id is our own id, bypass the relay Call
                // and send directly using the relay address (we ARE the relay for this node).
                let my_id = self.get_my_id();
                let is_self_relay = my_id.as_deref() == effective_nsk.relay_id();

                if is_self_relay {
                    tracing::info!(
                        "[BingleApiImpl::send_message_to_network] target's relay is self; bypassing relay Call and sending directly"
                    );
                    if let Some(relay_addr) = effective_nsk.relay_address() {
                        effective_nsk = NetworkEndpoint::new_direct(relay_addr);
                    } else if let Some(client_addr) = self
                        .engine
                        .access(|e| e.turn_relay_lookup_addr_by_id(user_id))
                    {
                        tracing::info!(
                            "[BingleApiImpl::send_message_to_network] self-relay: looked up client addr {} for user_id {}",
                            client_addr,
                            user_id
                        );
                        effective_nsk = NetworkEndpoint::new_direct(client_addr);
                    } else {
                        tracing::warn!(
                            "[BingleApiImpl::send_message_to_network] self-relay but no relay_address and TurnHandler lookup failed for user_id {}; cannot convert to direct endpoint",
                            user_id
                        );
                        if let Some(cb) = progress.as_ref() {
                            cb(100, "Self-relay with no relay address".to_string());
                        }
                        return Err(BingleError::Send {
                            kind: SendFailureKind::RelayAllocationFailed,
                            detail: format!(
                                "self-relay but no relay address for user_id {}",
                                user_id
                            ),
                        });
                    }
                } else {
                    tracing::info!(
                        "[BingleApiImpl::send_message_to_network] relay endpoint without channel detected; allocating via RelayClient"
                    );
                    if let Some(cb) = progress.as_ref() {
                        cb(15, "Allocating relay channel".to_string());
                    }

                    // Construct RelayClient with API handle
                    let ddb = self.engine.access(|e| e.ddb_client());
                    let relay_client =
                        crate::relay::relay_client::RelayClient::new(self.this.clone(), ddb);
                    match relay_client.call(&effective_nsk, user_id) {
                        Ok(updated) => {
                            effective_nsk = updated;

                            // Register TURN client mapping upon successful CallResponse
                            if let (Some(channel), Some(relay_addr)) =
                                (effective_nsk.relay_channel(), effective_nsk.relay_address())
                            {
                                let source_addr =
                                    effective_nsk.inet_socket_address().unwrap_or(relay_addr);
                                self.engine.access(|e| {
                                    e.turn_handle_call_response(
                                        source_addr,
                                        relay_addr,
                                        channel,
                                        effective_nsk.relay_id().expect(
                                            "relay_id() should be set by RelayClient::call()",
                                        ),
                                    )
                                });
                            }

                            if let Some(cb) = progress.as_ref() {
                                cb(30, "Relay channel allocated".to_string());
                            }
                        }
                        Err(err) => {
                            tracing::warn!(
                                "[BingleApiImpl::send_message_to_network] relay Call failed: {}",
                                err
                            );
                            if let Some(cb) = progress.as_ref() {
                                cb(100, format!("Relay allocation failed: {}", err));
                            }
                            return Err(BingleError::Send {
                                kind: SendFailureKind::RelayAllocationFailed,
                                detail: format!("relay channel allocation failed: {}", err),
                            });
                        }
                    }
                }
            }
            tracing::info!(
                "[BingleApiImpl::send_message_to_network] send_over_dtls {:?}, {}",
                effective_nsk,
                message
            );
            self.send_over_dtls(&effective_nsk, message)
        } else {
            // The recipient id is not a valid account address (base32 decode / length check failed
            // above): a permanent failure, surfaced as a typed cause rather than `Ok(false)`
            // (issue #99).
            Err(BingleError::Send {
                kind: SendFailureKind::InvalidRecipientId,
                detail: format!("invalid recipient id: {}", user_id),
            })
        };

        let ok = ok_res.as_ref().copied().unwrap_or(false);
        if let Some(cb) = progress.as_ref() {
            cb(100, if ok { "Sent" } else { "Failed to send" }.to_string());
        }
        tracing::info!(
            "[BingleApiImpl::send_message_to_network][exit] return={}",
            ok
        );
        ok_res
    }

    fn send_message_to_id_with_response(
        &self,
        user_id: &UserId,
        message: JsonValue,
        progress: Option<Arc<ProgressCallback>>,
    ) -> Result<JsonValue, BingleError> {
        tracing::info!(
            "[BingleApiImpl::send_message_to_id_with_response][enter] user_id={} msg={} progress={}",
            user_id,
            message,
            progress.is_some()
        );
        // 1) Use the Engine-bound DDB client to resolve the destination NetworkSourceKey
        let cli = self.engine.access(|e| e.ddb_client());
        tracing::debug!("[BingleApiImpl::send_message_to_id_with_response][enter] got ddb_client");
        let nsk = cli
            .lookup(user_id)
            .map_err(|e| BingleError::Other(format!("DDB lookup failed: {}", e)))?;
        // 2) Delegate to send_message_to_network_with_response for the actual send + wait
        tracing::debug!(
            "[BingleApiImpl::send_message_to_id_with_response] calling with nsk={}",
            nsk
        );
        let res = self.send_message_to_network_with_response(&nsk, user_id, message, progress);
        tracing::info!(
            "[BingleApiImpl::send_message_to_id_with_response][exit] result={:?}",
            res.as_ref().ok()
        );
        res
    }

    fn send_message_to_handle_with_response(
        &self,
        handle: &Handle,
        message: JsonValue,
        progress: Option<Arc<ProgressCallback>>,
    ) -> Result<JsonValue, BingleError> {
        tracing::info!(
            "[BingleApiImpl::send_message_to_handle_with_response][enter] handle={} msg={} progress={}",
            handle,
            message,
            progress.is_some()
        );
        let user_id = self
            .handle_lookup(handle)?
            .ok_or_else(|| BingleError::Other(format!("Handle not found: {}", handle)))?;
        if let Some(cb) = progress.as_ref() {
            cb(10, format!("Handle {} resolved to {}", handle, user_id));
        }
        let res = self.send_message_to_id_with_response(&user_id, message, progress);
        tracing::info!(
            "[BingleApiImpl::send_message_to_handle_with_response][exit] result={:?}",
            res.as_ref().ok()
        );
        res
    }

    fn send_message_to_network_with_response(
        &self,
        network_source_key: &NetworkEndpoint,
        user_id: &UserId,
        message: JsonValue,
        progress: Option<Arc<ProgressCallback>>,
    ) -> Result<JsonValue, BingleError> {
        let timeout = self
            .opts()
            .wait_response_timeout
            .unwrap_or(DEFAULT_WAIT_RESPONSE_TIMEOUT);
        self.send_message_to_network_with_response_timeout(
            network_source_key,
            user_id,
            message,
            progress,
            timeout,
        )
    }

    fn send_message_to_network_with_response_timeout(
        &self,
        network_source_key: &NetworkEndpoint,
        user_id: &UserId,
        message: JsonValue,
        progress: Option<Arc<ProgressCallback>>,
        timeout: Duration,
    ) -> Result<JsonValue, BingleError> {
        tracing::info!(
            "[BingleApiImpl::send_message_to_network_with_response][enter] nsk={} user_id={} msg={} progress={} timeout={:?}",
            network_source_key,
            user_id,
            message,
            progress.is_some(),
            timeout
        );
        #[allow(unused)]
        {}
        // Create a unique tag and register a pending waiter with the Engine
        let (tag, pending) = self.engine.access(|eng| {
            let tag = Uuid::new_v4();
            eng.register_pending(tag);
            (tag, eng.pending_responses_arc())
        });

        // Ensure outbound request has a correlation tag (responses must echo this as responseTag)
        let msg_with_tag = match message {
            JsonValue::Object(mut m) => {
                m.remove("responseTag");
                m.insert("tag".to_string(), JsonValue::String(tag.to_string()));
                JsonValue::Object(m)
            }
            other => {
                let mut m = JsonMap::new();
                m.insert("payload".to_string(), other);
                m.insert("tag".to_string(), JsonValue::String(tag.to_string()));
                JsonValue::Object(m)
            }
        };

        // Send the request synchronously before waiting to avoid races and ensure handshake starts
        if let Some(cb) = progress.as_ref() {
            cb(5, "Sending request".to_string());
        }
        let sent_ok = self.send_message_to_network(
            network_source_key,
            user_id,
            msg_with_tag.clone(),
            progress.clone(),
        )?;
        if let Some(cb) = progress.as_ref() {
            cb(
                20,
                if sent_ok {
                    "Request sent"
                } else {
                    "Failed to send request"
                }
                .to_string(),
            );
        }
        if !sent_ok {
            return Err(BingleError::Other("Failed to send request".to_string()));
        }

        // Now wait for a response tagged with our UUID using the Engine's pending map,
        // bounded by the caller-supplied timeout.
        if let Some(resp) = Engine::wait_for_response_static(pending, &tag, timeout) {
            if let Some(cb) = progress.as_ref() {
                cb(100, "Received response".to_string());
            }
            tracing::info!(
                "[BingleApiImpl::send_message_to_network_with_response][exit] Ok(response)"
            );
            #[allow(unused)]
            {}
            Ok(resp)
        } else {
            if let Some(cb) = progress.as_ref() {
                cb(100, "Timed out waiting for response".to_string());
            }
            let err = if sent_ok {
                "timeout waiting for response".to_string()
            } else {
                "send failed".to_string()
            };
            tracing::warn!(
                "[BingleApiImpl::send_message_to_network_with_response][exit] nsk={} user_id={} msg={} Err({})",
                network_source_key,
                user_id,
                msg_with_tag.clone(),
                err
            );
            #[allow(unused)]
            {}
            if sent_ok {
                // The request was sent but the peer/relay did not answer within the timeout: a
                // transient, retryable cause (issue #99).
                Err(BingleError::Send {
                    kind: SendFailureKind::NoResponse,
                    detail: err,
                })
            } else {
                Err(BingleError::Other(err))
            }
        }
    }

    fn set_on_message(&self, handler: Option<Arc<OnMessageHandler>>) {
        tracing::info!(
            "[BingleApiImpl::set_on_message][enter] handler_is_some={}",
            handler.is_some()
        );
        #[allow(unused)]
        {}

        // Store the handler and register it with the per-API router and global fallback
        *self.on_message.lock().expect("on_message lock") = handler.clone();
        if let Ok(mut g) = self.shared_on_message.lock() {
            *g = handler.clone();
        }
        let router = self.router.lock().expect("router lock").clone();
        if let Some(r) = router {
            r.set_on_message(handler.clone());
        }

        tracing::info!("[BingleApiImpl::set_on_message][exit]");
        #[allow(unused)]
        {}
    }

    fn set_on_connect(&self, handler: Option<Arc<OnConnectHandler>>) {
        tracing::info!(
            "[BingleApiImpl::set_on_connect][enter] handler_is_some={}",
            handler.is_some()
        );
        *self.on_connect.lock().expect("on_connect lock") = handler;
        tracing::info!("[BingleApiImpl::set_on_connect][exit]");
    }

    fn set_on_listening(&self, handler: Option<Arc<crate::api::bingle_api::OnListeningHandler>>) {
        tracing::info!(
            "[BingleApiImpl::set_on_listening][enter] handler_is_some={}",
            handler.is_some()
        );
        // Store locally
        *self.on_listening.lock().expect("on_listening lock") = handler.clone();
        // Propagate to Engine so internal notifications can reach the application
        self.engine.set_on_listening_handler(handler);
        tracing::info!("[BingleApiImpl::set_on_listening][exit]");
    }
}

impl BingleApiImpl {
    /// Public entry from the networking layer for inbound messages.
    /// If message contains a responseTag, it is treated as a response and routed to waiter; otherwise it is dispatched to on_message.
    #[doc(hidden)]
    pub fn handle_incoming_network_message(
        &self,
        sender: UserId,
        sender_handle: Handle,
        message: JsonValue,
    ) {
        tracing::info!(
            "[BingleApiImpl::handle_incoming_network_message][enter] sender={} handle={} msg={}",
            sender,
            sender_handle,
            message
        );
        #[allow(unused)]
        {}
        // Opportunistically update the cache mapping for reverse lookups
        if let Ok(mut cache) = self.handle_cache.lock() {
            cache.insert(sender_handle.clone(), sender.clone(), Instant::now());
        }
        // Engine now fulfills tagged responses; just forward application messages.
        let on_message = self.on_message.lock().expect("on_message lock").clone();
        if let Some(cb) = on_message {
            cb(sender, sender_handle, message);
        }
        tracing::info!("[BingleApiImpl::handle_incoming_network_message][exit]");
        #[allow(unused)]
        {}
    }
}

impl BingleApiImpl {
    /// Lookup a handle by user id using the local cache. Returns None if not present or expired.
    #[doc(hidden)]
    pub fn handle_lookup_by_id(&self, user_id: &UserId) -> Option<Handle> {
        let expiry_duration = self
            .opts()
            .handle_cache_expiry
            .unwrap_or(Duration::from_secs(600));
        // 1) Check cache first
        if let Ok(mut cache) = self.handle_cache.lock()
            && let Some(h) = cache.get_handle_by_id(user_id, expiry_duration)
        {
            return Some(h);
        }

        // 2) Test seam: allow injection for unit/integration tests to avoid network
        if let Ok(m) = self.id_to_handle_lookup_mock.lock()
            && let Some(cb) = m.as_ref()
        {
            match cb(user_id) {
                Ok(Some(handle)) => {
                    if let Ok(mut cache) = self.handle_cache.lock() {
                        cache.insert(handle.clone(), user_id.clone(), Instant::now());
                    }
                    return Some(handle);
                }
                Ok(None) => { /* fall through to blockchain (or ultimately None) */ }
                Err(e) => {
                    tracing::warn!(
                        "[BingleApiImpl::handle_lookup_by_id] test seam error: {}",
                        e
                    );
                    // fall through
                }
            }
        }

        // 3) Fallback: query blockchain local storage via AlgoOps/AlgoBingle to extract 'Handle'
        let app_id = match self.get_app_id() {
            Some(a) => a,
            None => {
                tracing::warn!("[BingleApiImpl::handle_lookup_by_id] app_id not configured");
                return None;
            }
        };
        let config = match self.get_algo_provider_config() {
            Some(c) => c,
            None => {
                tracing::warn!(
                    "[BingleApiImpl::handle_lookup_by_id] algo_provider_config not configured"
                );
                return None;
            }
        };

        // Build an AlgoBingle over the provided config and resolve address -> handle through the
        // shared reader (asset id is irrelevant to a local-state read, so 0).
        let ops =
            AlgoOps::new_for_algorand(self.opts().algo_passphrase.clone(), None, Some(config));
        let bgl = crate::blockchain::algo_bingle::AlgoBingle::new(ops, app_id, 0);
        // Bound the blockchain query: this runs on the DTLS receive path (sender-auth
        // check), so an unreachable node must not stall inbound message processing. A
        // timeout resolves to None (same as unreachable) — the sender retries and the
        // handle cache covers repeat senders (bingle_rust #48).
        let uid = user_id.clone();
        let lookup = run_bounded(HANDLE_LOOKUP_TIMEOUT, "handle_lookup_by_id", move || {
            bgl.handle_for_address(app_id, &uid)
        });
        match lookup {
            Some(Ok(Some(handle))) => {
                // Update cache and return.
                if let Ok(mut cache) = self.handle_cache.lock() {
                    cache.insert(handle.clone(), user_id.clone(), Instant::now());
                }
                Some(handle)
            }
            Some(Ok(None)) => {
                tracing::info!(
                    "[BingleApiImpl::handle_lookup_by_id] no handle registered for {} (not opted in, \
                     or no Handle key)",
                    user_id
                );
                None
            }
            Some(Err(e)) => {
                tracing::warn!(
                    "[BingleApiImpl::handle_lookup_by_id] blockchain query failed: {}",
                    e
                );
                None
            }
            // Timed out (already warned in run_bounded): treat as unreachable.
            None => None,
        }
    }
}

impl crate::api::bingle_api::BingleApiInternal for BingleApiImpl {
    fn get_relay_state(&self) -> String {
        self.engine.access(|e| e.relay_state_str())
    }
    fn mutex_handle_request(&self, from_id: String, req: crate::messages::types::MutexRequest) {
        self.engine
            .access(|e| e.mutex_handle_request(&from_id, &req));
    }
    fn mutex_handle_response(&self, from_id: String, resp: crate::messages::types::MutexResponse) {
        self.engine
            .access(|e| e.mutex_handle_response(&from_id, &resp));
    }
    fn mutex_handle_release(&self, from_id: String, rel: crate::messages::types::MutexRelease) {
        self.engine
            .access(|e| e.mutex_handle_release(&from_id, &rel));
    }
    fn set_state(&self, state: EngineState) {
        tracing::info!("[BingleApiImpl::set_state][enter] state={:?}", state);
        let _ = self.engine.access(|e| e.set_state_internal(state));
        tracing::info!("[BingleApiImpl::set_state][exit]");
    }
    fn get_state(&self) -> EngineState {
        self.engine.access(|e| e.state())
    }
    fn set_nat_type(&self, nat: crate::engine::NatType) {
        tracing::info!("[BingleApiImpl::set_nat_type][enter] nat_type={:?}", nat);
        self.engine.access(|e| e.set_nat_type(nat));
        tracing::info!("[BingleApiImpl::set_nat_type][exit]");
    }
    fn get_nat_type(&self) -> crate::engine::NatType {
        self.engine.access(|e| e.nat_type())
    }
    fn get_last_public_addr(&self) -> Option<SocketAddr> {
        self.engine.access(|e| e.last_public_addr())
    }
    fn ddb_register_ip(&self, endpoint: SocketAddr, am_relay: bool) -> Result<(), BingleError> {
        let cli = self.engine.access(|e| e.ddb_client());
        tracing::info!(
            "[BingleApiImpl::ddb_register_ip] registering IP: {:?}, am_relay={}",
            endpoint,
            am_relay
        );
        cli.register_ip(endpoint, am_relay)
    }
    fn ddb_register_relay(
        &self,
        relay_id: String,
        relay_sig: Option<String>,
    ) -> Result<(), BingleError> {
        let cli = self.engine.access(|e| e.ddb_client());
        tracing::info!(
            "[BingleApiImpl::ddb_register_relay] registering relay: id={}",
            relay_id
        );
        cli.register_relay(relay_id, relay_sig)
    }
    fn ddb_signoff(&self) -> Result<(), BingleError> {
        let cli = self.engine.access(|e| e.ddb_client());
        tracing::info!("[BingleApiImpl::ddb_signoff] signing off");
        cli.signoff()
    }
    fn update_turn_listener_relay(
        &self,
        relay_id: String,
        relay_addr: SocketAddr,
    ) -> Result<(), BingleError> {
        tracing::info!(
            "[BingleApiImpl::update_turn_listener_relay][enter] id={} addr={}",
            relay_id,
            relay_addr
        );
        // Backwards-compatible: delegate to client-side listen response handler
        <BingleApiImpl as crate::api::bingle_api::BingleApiInternal>::turn_client_handle_listen_response(self, relay_addr, relay_id);
        tracing::info!("[BingleApiImpl::update_turn_listener_relay][exit] Ok(())");
        Ok(())
    }
    fn start_relay_keep_alive(&self, relay_id: String, relay_addr: SocketAddr) {
        self.engine
            .access(|e| e.start_relay_keep_alive(relay_id, relay_addr));
    }
    fn stop_relay_keep_alive(&self) {
        self.engine.access(|e| e.stop_relay_keep_alive());
    }
    fn turn_client_handle_listen_response(&self, relay_addr: SocketAddr, relay_id: String) {
        tracing::info!(
            "[BingleApiImpl::turn_client_handle_listen_response][enter] id={} addr={}",
            relay_id,
            relay_addr
        );
        self.engine
            .access(|e| e.turn_client_handle_listen_response(relay_addr, &relay_id));
        tracing::info!("[BingleApiImpl::turn_client_handle_listen_response][exit]");
    }
    fn notify_listening(&self, listening: bool, nat_type: crate::engine::NatType) {
        tracing::info!(
            "[BingleApiImpl::notify_listening] listening={} nat_type={:?}",
            listening,
            nat_type
        );
        let on_listening = self.on_listening.lock().expect("on_listening lock").clone();
        if let Some(cb) = on_listening {
            cb(listening, nat_type);
        }
    }
    fn turn_handle_called(&self, source: SocketAddr, dest: SocketAddr, channel: u16) {
        // Forward to the engine's TURN handler client-side interface (non-test API)
        self.engine
            .access(|e| e.turn_client_handle_called(source, dest, channel));
    }
    fn turn_handle_call_response(
        &self,
        source: SocketAddr,
        dest: SocketAddr,
        channel: u16,
        relay_id: String,
    ) {
        self.engine
            .access(|e| e.turn_handle_call_response(source, dest, channel, &relay_id));
    }
    fn turn_lookup_addr_by_id(&self, id: String) -> Option<SocketAddr> {
        self.engine.access(|e| e.turn_relay_lookup_addr_by_id(&id))
    }
    fn turn_handle_call(
        &self,
        source_id: String,
        dest_id: String,
        source: SocketAddr,
        dest: SocketAddr,
    ) -> i32 {
        self.engine
            .access(|e| e.turn_relay_handle_call(&source_id, &dest_id, source, dest))
    }
    fn turn_handle_listen(&self, id: String, source: SocketAddr) -> bool {
        self.engine
            .access(|e| e.turn_relay_handle_listen(&id, &source))
    }
    fn set_relay_state(&self, state: crate::engine::RelayState) {
        tracing::info!("[BingleApiImpl::set_relay_state] state={:?}", state);
        self.engine
            .set_relay_state(state, "set_relay_state from API internal");
    }
    fn get_peer_ddb_target(&self) -> Option<usize> {
        self.engine.access(|e| e.peer_ddb_records())
    }
    fn ddb_upsert_record(&self, record: crate::ddb::AdvertRecord) {
        self.engine.access(|e| e.ddb_upsert_record(record))
    }
    fn ddb_delete_record(&self, id: &str) {
        self.engine.access(|e| e.ddb_delete_record(id))
    }
    fn relay_finder_remove_relay(&self, relay_id: &str) {
        self.engine
            .access(|e| e.relay_finder_remove_relay(relay_id))
    }
    fn relay_finder_clear_state_cache(&self) {
        self.engine.access(|e| e.relay_finder_clear_state_cache())
    }
    fn ddb_backend_size(&self) -> usize {
        self.engine.access(|e| e.ddb_backend_size())
    }
    fn initialize_relay(&self) {
        tracing::info!("[BingleApiImpl::initialize_relay]");
        self.engine.initialize_relay();
    }
    fn with_engine(&self, f: &mut dyn FnMut(&Engine)) {
        f(&self.engine);
    }
    fn test_take_listen_drop(&self) -> bool {
        self.engine.test_take_listen_drop()
    }
    fn is_relay(&self) -> bool {
        self.opts().am_relay
    }
    fn signal_signon_complete(&self) {
        tracing::info!("[BingleApiImpl::signal_signon_complete]");
        self.engine.access(|e| e.signal_signon_complete());
    }
    fn reset_signon_complete(&self) {
        tracing::info!("[BingleApiImpl::reset_signon_complete]");
        self.engine.access(|e| e.reset_signon_complete());
    }
    fn ripple_message(
        &self,
        message: serde_json::Value,
        originator_id: String,
        ddb_backend: &dyn crate::ddb::DdbBackend,
    ) {
        tracing::info!(
            "[BingleApiImpl::ripple_message] originator={}",
            originator_id
        );
        self.engine
            .access(|e| e.ripple_message(message, originator_id, ddb_backend));
    }

    fn get_signing_key(&self) -> Option<SigningKey> {
        self.engine.access(|e| e.get_signing_key())
    }
}
