#!/usr/bin/env sh
# Single-node Sidewinder network on LocalNet for bingle_core end-to-end testing.
#
# Generates (and funds) a node anchoring account and two client accounts — a **sender** and a
# **receiver** — sets up the one-node network (BLS key, allowlist enrolling the two clients as callers,
# node config against the LocalNet parent chain), starts the node with its client REST API, and prints
# the endpoint + address + mnemonic for the sender and receiver. Run two bingle_core instances against
# those to post and pop on the local network.
#
# Self-contained: it drives the **installed `sw-node`** binary and carries its own application config,
# so it needs no repository checkout or build — copy this one file anywhere and run it.
#
# Prereqs:
#   - sw-node on PATH:        cargo install --path sw-node   (from the repo), or `cargo install sw-node`
#   - algokit LocalNet up:    algokit localnet start
#   - curl                    (to probe the node's health endpoint)
#
# Usage:
#   e2e_sidewinder_localnet.sh [up]    # generate/reuse accounts, start the node, print access details
#   e2e_sidewinder_localnet.sh down    # stop the node and remove the work dir
set -eu

WORK=${WORK:-./tmp/bingle-e2e}
API_PORT=${API_PORT:-1080}
PEER_PORT=${PEER_PORT:-9101}
AUTH_TOKEN=${AUTH_TOKEN:-bingle-localnet-token}
FUND_ALGOS=${FUND_ALGOS:-100}
# the installed binary by default; override with a path (e.g. a repo build) if you prefer.
SW_NODE=${SW_NODE:-sw-node}
# LocalNet algod, used only for the pre-flight reachability check (funding goes via sw-node → kmd).
ALGOD_URL=${ALGOD_URL:-http://localhost:4001}
ALGOD_TOKEN=${ALGOD_TOKEN:-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa}
# optional: an application config file to use instead of the built-in Mailbox registry.
APP_CONFIG=${APP_CONFIG:-}

die() { echo "e2e_sidewinder_localnet: $1" >&2; exit 1; }

# write the built-in Mailbox application config (post / pop / drain over the FIFO primitive) — carried
# here so the harness needs no repo files.
write_app_config() {
  cat > "$1" <<'YAML'
transaction_types:
  - id: 1
    name: FIFO
    dispatch:
      standard: { class: FIFO, method: append, signature: "post(byte[],byte[])byte[]" }
    key_source: "arg:0"
    access_predicate: any-enrolled
    result: "byte[]"
    assurance: { f: 0 }
  - id: 2
    name: FIFO
    dispatch:
      standard: { class: FIFO, method: remove_head, signature: "pop()byte[]" }
    key_source: sender
    access_predicate: owner
    result: "byte[]"
    assurance: { f: 0 }
  - id: 3
    name: FIFO
    dispatch:
      standard: { class: FIFO, method: remove_all, signature: "drain()byte[]" }
    key_source: sender
    access_predicate: owner
    result: "byte[]"
    assurance: { f: 0 }
YAML
}

stop_node() { [ -f "$WORK/node.pid" ] && kill "$(cat "$WORK/node.pid")" 2>/dev/null || true; }

if [ "${1:-up}" = "down" ]; then
  stop_node
  rm -rf "$WORK"
  echo "e2e down: stopped the node and removed $WORK" >&2
  exit 0
fi

# --- pre-flight: prereqs present and LocalNet reachable ---
command -v "$SW_NODE" >/dev/null 2>&1 \
  || die "sw-node not found ('$SW_NODE') — install it: cargo install --path sw-node, or set SW_NODE=/path/to/sw-node"
command -v curl >/dev/null 2>&1 || die "curl not found — needed to probe the node's health endpoint"
[ -z "$APP_CONFIG" ] || [ -f "$APP_CONFIG" ] || die "application config not found: $APP_CONFIG"
curl -fsS -o /dev/null -H "X-Algo-API-Token: $ALGOD_TOKEN" "$ALGOD_URL/v2/status" 2>/dev/null \
  || die "LocalNet algod unreachable at $ALGOD_URL — start it: algokit localnet start"

stop_node                       # stop a node from a previous `up` (keeps the account files for reuse)
mkdir -p "$WORK/etc" "$WORK/data"

# parse a `key: value` line from sw-node's output.
field() { printf '%s\n' "$1" | sed -n "s/^$2: //p"; }

# --- generate (or reuse) and fund the three accounts on LocalNet ---
echo "generating + funding accounts on LocalNet ($FUND_ALGOS ALGO each)..." >&2
node_out=$("$SW_NODE" account --out "$WORK/etc/account.key" --fund-localnet --amount "$FUND_ALGOS") \
  || die "node account generate/fund failed (is LocalNet funded and kmd reachable?)"
sender_out=$("$SW_NODE" account --out "$WORK/sender.key" --fund-localnet --amount "$FUND_ALGOS") \
  || die "sender account generate/fund failed"
receiver_out=$("$SW_NODE" account --out "$WORK/receiver.key" --fund-localnet --amount "$FUND_ALGOS") \
  || die "receiver account generate/fund failed"

NODE_ADDR=$(field "$node_out" address)
SENDER_ADDR=$(field "$sender_out" address);   SENDER_MN=$(field "$sender_out" mnemonic)
RECEIVER_ADDR=$(field "$receiver_out" address); RECEIVER_MN=$(field "$receiver_out" mnemonic)

# --- network setup: BLS consensus key, allowlist, parent-chain, node config ---
"$SW_NODE" keygen --out "$WORK/etc/consensus.key" --account "$NODE_ADDR" > "$WORK/keygen.out" 2>&1 \
  || { cat "$WORK/keygen.out" >&2; die "keygen failed (a stale consensus.key? run 'down' first)"; }
BLS=$(sed -n 's/^[[:space:]]*bls_public_key:[[:space:]]*//p' "$WORK/keygen.out")
POP=$(sed -n 's/^[[:space:]]*proof_of_possession:[[:space:]]*//p' "$WORK/keygen.out")

if [ -n "$APP_CONFIG" ]; then cp "$APP_CONFIG" "$WORK/etc/application.yaml"; else write_app_config "$WORK/etc/application.yaml"; fi
cat > "$WORK/etc/parent-chain.json" <<EOF
{ "network": "localnet", "client_api_url": "http://localhost", "client_api_port": 4001, "token": "$ALGOD_TOKEN" }
EOF
cat > "$WORK/etc/allowlist.yaml" <<EOF
nodes:
  - account: "$NODE_ADDR"
    bls_public_key: "$BLS"
    proof_of_possession: "$POP"
callers:
  - "$SENDER_ADDR"
  - "$RECEIVER_ADDR"
EOF
cat > "$WORK/etc/node.yaml" <<EOF
index: 0
data_dir: $(cd "$WORK/data" && pwd)
application_config: application.yaml
allowlist: allowlist.yaml
parent_chain: parent-chain.json
consensus_key: consensus.key
account_key: account.key
listen: "0.0.0.0:$PEER_PORT"
api:
  listen: "0.0.0.0:$API_PORT"
  auth_token: "$AUTH_TOKEN"
EOF

# --- start the node (background local process; survives this script, stopped by `down`) ---
nohup "$SW_NODE" run --node-file "$WORK/etc/node.yaml" > "$WORK/node.log" 2>&1 &
echo $! > "$WORK/node.pid"

# wait for the client API to answer /health.
ready=""
i=0
while [ "$i" -lt 30 ]; do
  if curl -fsS -o /dev/null "http://127.0.0.1:$API_PORT/health" 2>/dev/null; then ready=1; break; fi
  sleep 1; i=$((i + 1))
done
[ -n "$ready" ] || { tail -20 "$WORK/node.log" >&2; stop_node; die "node did not become healthy — see $WORK/node.log"; }

cat <<EOF

=== bingle_core LocalNet endpoints ===
API endpoint : http://127.0.0.1:$API_PORT
Bearer token : $AUTH_TOKEN
Application  : Mailbox — post(recipient, msg) / pop() / drain()

[sender]
  address  : $SENDER_ADDR
  mnemonic : $SENDER_MN

[receiver]
  address  : $RECEIVER_ADDR
  mnemonic : $RECEIVER_MN

node pid $(cat "$WORK/node.pid") — logs: $WORK/node.log
stop with: $0 down
EOF
