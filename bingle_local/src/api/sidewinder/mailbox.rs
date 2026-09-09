//! A thin wrapper over the Sidewinder Mailbox (first-in first-out, FIFO) operations the
//! store-and-forward path uses (store-and-forward epic #200, foundation story #213).
//!
//! The `sidewinder_ops` client is a generic transaction client — it submits a signed
//! [`TransactionRequest`] (an operation type plus its arguments) and polls the result. The Mailbox
//! is a node-side *application* configured over the FIFO primitive: `post(recipient, message)`
//! appends to the recipient's queue (keyed by the recipient address in `arg[0]`, callable by any
//! enrolled sender) and `pop()` removes and returns the head of the *caller's own* queue (keyed by
//! the authenticated sender). This wrapper turns those two operations into plain method calls,
//! packing the arguments, submitting through the client, and polling the transaction to finality.
//!
//! The operation *type numbers* (`post_type` / `pop_type`) are assigned by the node's
//! `application.yaml`, not fixed by the client crate, so they live in [`MailboxConfig`] and default
//! to the tier-1 Mailbox binding ([`MAILBOX_POST_TYPE`] / [`MAILBOX_POP_TYPE`]). No post-on-fail or
//! read-on-reconnect wiring lives here — those are their own stories (#214 / #215); this story is
//! just the client, the connection config, and the two Mailbox operations.

use algo_ops::AlgoOps;
use bingle_core::api::bingle_api::BingleError;
use sidewinder_ops::{
    AppArg, PendingTransaction, SidewinderClient, SidewinderConfig, SidewinderError,
    SidewinderErrorKind, SidewinderOps, Stage, TransactionRequest,
};
use std::time::{Duration, Instant};

/// The transaction type the tier-1 Mailbox configuration binds `post` (`FIFO.append`) to. The node's
/// `application.yaml` is the source of truth; this is the default when a caller does not override it.
pub const MAILBOX_POST_TYPE: u32 = 1;
/// The transaction type the tier-1 Mailbox configuration binds `pop` (`FIFO.remove_head`) to.
pub const MAILBOX_POP_TYPE: u32 = 2;

/// Default time to wait for a submitted transaction to reach the `final` stage before giving up.
/// Anchoring to the parent chain dominates this. LocalNet finalises in a round or two; a network
/// whose anchor batching is slower (e.g. TestNet's `v0_0_3` profile, K=64 rounds ≈ minutes) should
/// raise it via [`MailboxConfig::finality_timeout`].
pub const DEFAULT_FINALITY_TIMEOUT: Duration = Duration::from_secs(120);
/// The long-poll window handed to each `watch` call while polling for finality.
const WATCH_WAIT_SECS: u64 = 5;
/// Pause between `watch` retries when the read node does not yet know a just-submitted transaction
/// (a not-found is propagation lag, not a failure), so we do not busy-loop.
const NOT_FOUND_BACKOFF: Duration = Duration::from_millis(500);

/// How to reach a recipient's Sidewinder Mailbox: the node connection plus the operation-type
/// numbers `post` and `pop` are bound to in the node's application configuration.
///
/// The endpoint and bearer token come from deployment configuration (the testnet node json or the
/// environment), never hardcoded; the token is the v0.0.2 fixed shared client token (Sidewinder
/// #164). There is deliberately no `Default`: a node URL and token are deployment-specific.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxConfig {
    /// Base URL of the Sidewinder node, for example `http://localhost:9101`.
    pub base_url: String,
    /// Bearer token sent on every authenticated node endpoint.
    pub token: String,
    /// Transaction type bound to the Mailbox `post` operation (`FIFO.append`).
    pub post_type: u32,
    /// Transaction type bound to the Mailbox `pop` operation (`FIFO.remove_head`).
    pub pop_type: u32,
    /// How long to wait for a submitted transaction to reach `final` before giving up. Defaults to
    /// [`DEFAULT_FINALITY_TIMEOUT`]; raise it for a network whose anchor batching is slower than
    /// LocalNet (e.g. TestNet).
    pub finality_timeout: Duration,
}

impl MailboxConfig {
    /// Build a config from a node URL and bearer token, using the default tier-1 Mailbox operation
    /// types ([`MAILBOX_POST_TYPE`] / [`MAILBOX_POP_TYPE`]) and [`DEFAULT_FINALITY_TIMEOUT`]. Set the
    /// fields on the returned value to override the types or the finality timeout.
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            token: token.into(),
            post_type: MAILBOX_POST_TYPE,
            pop_type: MAILBOX_POP_TYPE,
            finality_timeout: DEFAULT_FINALITY_TIMEOUT,
        }
    }

    /// Map a caller's optional node URL and token to a Mailbox config: `Some` only when *both* are
    /// supplied, `None` otherwise (store-and-forward stays unconfigured when either is missing).
    /// Shared by the JSI, webserver, and CLI call sites so the mapping lives — and is tested — in one
    /// place. Empty strings are treated as absent so a blank environment value does not half-configure
    /// the Mailbox.
    pub fn from_parts(base_url: Option<String>, token: Option<String>) -> Option<Self> {
        let base_url = base_url.filter(|s| !s.trim().is_empty())?;
        let token = token.filter(|s| !s.trim().is_empty())?;
        Some(Self::new(base_url, token))
    }
}

/// Whether store-and-forward posting should run for a send that failed direct delivery: the send
/// gate is on ([`store_and_forward_send`](crate::api::bingle_local_api_impl::LocalApiConfig::store_and_forward_send))
/// *and* a Sidewinder node is configured. Pure, so the post-on-delivery-fail gate (#214) is
/// unit-testable without a node.
pub fn should_forward_send(send_gate: bool, sidewinder_configured: bool) -> bool {
    send_gate && sidewinder_configured
}

/// The recipients of a message (identified by its `timestamp`) that have not yet been posted to a
/// Mailbox, given the set of `(timestamp, handle)` pairs already forwarded. This is the
/// per-recipient idempotency filter for post-on-delivery-fail (#214): a recipient already posted is
/// skipped, so a retry or restart re-posts only recipients whose post has not yet succeeded. Pure,
/// so the once-per-recipient guarantee is unit-tested without a node.
pub fn pending_forward_recipients(
    timestamp: i64,
    recipient_handles: &[String],
    forwarded: &std::collections::HashSet<(i64, String)>,
) -> Vec<String> {
    recipient_handles
        .iter()
        .filter(|handle| !forwarded.contains(&(timestamp, (*handle).clone())))
        .cloned()
        .collect()
}

/// A client for one recipient-addressable Sidewinder Mailbox, bound to an enrolled parent-chain
/// account (the [`AlgoOps`] handle signs every transaction it submits).
pub struct Mailbox {
    client: SidewinderClient,
    post_type: u32,
    pop_type: u32,
    finality_timeout: Duration,
}

impl Mailbox {
    /// Build a Mailbox client from an enrolled [`AlgoOps`] handle and connection config.
    ///
    /// Fails cleanly (a surfaced [`BingleError`], never a panic) when the endpoint or token is
    /// missing, so a misconfigured deployment is reported rather than crashing.
    pub fn new(algo: AlgoOps, config: MailboxConfig) -> Result<Self, BingleError> {
        if config.base_url.trim().is_empty() {
            return Err(BingleError::Other(
                "sidewinder mailbox: node base URL is empty".to_string(),
            ));
        }
        if config.token.trim().is_empty() {
            return Err(BingleError::Other(
                "sidewinder mailbox: node bearer token is empty".to_string(),
            ));
        }
        let client = SidewinderClient::from_algo_ops(
            algo,
            SidewinderConfig::new(config.base_url, config.token),
        );
        Ok(Self {
            client,
            post_type: config.post_type,
            pop_type: config.pop_type,
            finality_timeout: config.finality_timeout,
        })
    }

    /// Post `message` to `recipient`'s Mailbox (`FIFO.append`), waiting for the transaction to
    /// finalise. `recipient` is the recipient's Algorand address string, packed as the queue key in
    /// `arg[0]`; the message bytes are `arg[1]`.
    pub fn post(&self, recipient: &str, message: &[u8]) -> Result<(), BingleError> {
        let params = self.client.params().map_err(|e| map_error("params", e))?;
        let request = build_post_request(self.post_type, recipient, message, &params);
        self.submit_and_finalize("post", request)?;
        // A successful `FIFO.append` returns an empty result; there is nothing to hand back.
        Ok(())
    }

    /// Pop the head message from the caller's own Mailbox (`FIFO.remove_head`), waiting for the
    /// transaction to finalise. Returns the message bytes, or `None` when the Mailbox is empty (the
    /// operation returns an empty result once the queue is drained).
    ///
    /// The returned bytes are the value exactly as it was posted; interpreting them (opening the
    /// sealed store-and-forward envelope) is the read-on-reconnect story (#215), not this wrapper.
    pub fn pop(&self) -> Result<Option<Vec<u8>>, BingleError> {
        let params = self.client.params().map_err(|e| map_error("params", e))?;
        let request = build_pop_request(self.pop_type, &params);
        let finalized = self.submit_and_finalize("pop", request)?;
        Ok(match finalized.result {
            Some(bytes) if !bytes.is_empty() => Some(bytes),
            _ => None,
        })
    }

    /// Submit `request`, then poll it to the `final` stage, returning the finalised transaction.
    /// A transaction that reaches `failed`, or does not finalise within the configured finality
    /// timeout, is a surfaced error.
    fn submit_and_finalize(
        &self,
        operation: &str,
        request: TransactionRequest,
    ) -> Result<PendingTransaction, BingleError> {
        let txid = self
            .client
            .submit_transaction(&request)
            .map_err(|e| map_error(operation, e))?;
        // Log the transaction id at submit so it can be tracked on the node while it finalises.
        tracing::info!("[mailbox {operation}] submitted tx {txid}");
        self.poll_to_final(operation, &txid)
    }

    /// Poll `txid` until it reaches `final` (or `failed`), long-polling each request. A read node may
    /// not know a just-submitted transaction yet, so a not-found is tolerated as propagation lag
    /// until the deadline; any other error, a `failed` stage, or missing finality is an error.
    fn poll_to_final(
        &self,
        operation: &str,
        txid: &str,
    ) -> Result<PendingTransaction, BingleError> {
        let deadline = Instant::now() + self.finality_timeout;
        // The most recent `stage` seen, so a timeout reports how far the transaction actually got
        // (e.g. stuck at `Pending` vs. reaching `Verified` but never anchored).
        let mut last_stage: Option<String> = None;
        loop {
            match self.client.watch(txid, false, WATCH_WAIT_SECS) {
                Ok(pending) => {
                    last_stage = Some(format!("{:?}", pending.stage));
                    tracing::debug!("[mailbox {operation}] {txid} stage={:?}", pending.stage);
                    if pending.stage == Stage::Final {
                        if let Some(error) = pending.error {
                            return Err(BingleError::Other(format!(
                                "sidewinder {operation} transaction {txid} finalised with error: {error:?}"
                            )));
                        }
                        return Ok(pending);
                    }
                    if pending.stage == Stage::Failed {
                        tracing::warn!("[mailbox {operation}] {txid} FAILED: {:?}", pending.error);
                        return Err(BingleError::Other(format!(
                            "sidewinder {operation} transaction {txid} failed: {:?}",
                            pending.error
                        )));
                    }
                }
                Err(e) => {
                    if !is_not_found(&e) {
                        return Err(map_error(operation, e));
                    }
                    std::thread::sleep(NOT_FOUND_BACKOFF);
                }
            }
            if Instant::now() >= deadline {
                return Err(BingleError::Retryable(format!(
                    "sidewinder {operation} transaction {txid} did not finalise within {:?} \
                     (last stage: {})",
                    self.finality_timeout,
                    last_stage.as_deref().unwrap_or("unknown")
                )));
            }
        }
    }
}

/// Build the `post(recipient, message)` transaction: `arg[0]` is the recipient address string bytes
/// (the queue key), `arg[1]` is the message. A unique note keeps two otherwise-identical posts from
/// colliding on the content-address transaction identifier.
#[doc(hidden)]
pub fn build_post_request(
    post_type: u32,
    recipient: &str,
    message: &[u8],
    params: &sidewinder_ops::SuggestedParams,
) -> TransactionRequest {
    TransactionRequest {
        txn_type: post_type,
        args: vec![
            AppArg::Bytes(recipient.as_bytes().to_vec()),
            AppArg::Bytes(message.to_vec()),
        ],
        max_fee: params.min_fee,
        first_valid: params.last_round,
        last_valid: params.last_round + params.max_validity_window,
        instance: params.instance_id.clone(),
        note: Some(AlgoOps::unique_note()),
        group: None,
    }
}

/// Build the `pop()` transaction: no arguments (the queue key is the authenticated sender). A unique
/// note keeps repeated pops distinct on the content address.
#[doc(hidden)]
pub fn build_pop_request(
    pop_type: u32,
    params: &sidewinder_ops::SuggestedParams,
) -> TransactionRequest {
    TransactionRequest {
        txn_type: pop_type,
        args: vec![],
        max_fee: params.min_fee,
        first_valid: params.last_round,
        last_valid: params.last_round + params.max_validity_window,
        instance: params.instance_id.clone(),
        note: Some(AlgoOps::unique_note()),
        group: None,
    }
}

/// Whether an error from the client is a Sidewinder not-found — the propagation-lag case tolerated
/// while polling for finality.
fn is_not_found(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<SidewinderError>()
        .is_some_and(|se| se.kind == SidewinderErrorKind::NotFound)
}

/// Map a client error to a [`BingleError`], classifying unreachable/transient causes as retryable so
/// the store-and-forward path can distinguish "try again" from a persistent failure.
fn map_error(operation: &str, error: anyhow::Error) -> BingleError {
    if let Some(se) = error.downcast_ref::<SidewinderError>() {
        if matches!(
            se.kind,
            SidewinderErrorKind::HostUnreachable | SidewinderErrorKind::TransientFailure
        ) {
            return BingleError::Retryable(format!("sidewinder {operation}: {se}"));
        }
    }
    BingleError::Other(format!("sidewinder {operation} failed: {error}"))
}
