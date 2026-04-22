#!/usr/bin/env bash
#
# Boot a local anvil fork of BNB Smart Chain mainnet so the full
# Charon liquidation path (scanner → profit → Aave V3 flashloan →
# Venus liquidate → PancakeSwap swap) can be demonstrated without
# real funds.
#
# Usage:
#   ./scripts/anvil_fork.sh                # fork latest, primary RPC
#   FORK_BLOCK=41000000 ./scripts/anvil_fork.sh
#   FORK_RPC=https://custom/bsc ./scripts/anvil_fork.sh
#   FORK_PORT=8546 ./scripts/anvil_fork.sh  # avoid a port collision
#
# Environment knobs:
#   FORK_RPC      — explicit upstream; skips probe/fallback when set
#   FORK_BLOCK    — pin fork at this block (default: latest upstream)
#   FORK_PORT     — host port for HTTP+WS (default: 8545)
#   FORK_CHAIN_ID — preserved chain id (default: 56, BSC mainnet)
#
# Upstream probing:
#   When FORK_RPC is unset, the script tests the primary (dRPC) with a
#   single eth_blockNumber call. A non-2xx response or timeout falls
#   back to PublicNode. Both are free, keyless, and support historical
#   state reads — see `docs/Charon_Architecture_Diagrams.html` for the
#   research notes.

set -euo pipefail

# ── Resolve dependencies ─────────────────────────────────────────────
if ! command -v anvil >/dev/null 2>&1; then
    echo "anvil not found in PATH. Install Foundry: https://book.getfoundry.sh/getting-started/installation" >&2
    exit 127
fi

if ! command -v curl >/dev/null 2>&1; then
    echo "curl is required for the upstream RPC probe." >&2
    exit 127
fi

# ── Defaults ─────────────────────────────────────────────────────────
readonly PRIMARY_RPC="${FORK_RPC_PRIMARY:-https://bsc.drpc.org}"
readonly FALLBACK_RPC="${FORK_RPC_FALLBACK:-https://bsc-rpc.publicnode.com}"
readonly PORT="${FORK_PORT:-8545}"
readonly CHAIN_ID="${FORK_CHAIN_ID:-56}"

probe_rpc() {
    # Return 0 iff the RPC answers eth_blockNumber with a non-empty
    # hex payload within a reasonable timeout. Tight timeout because a
    # slow primary is as bad as a dead one for an interactive demo.
    local url="$1"
    local body
    body=$(curl -sS --max-time 5 -X POST \
        -H 'content-type: application/json' \
        --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' \
        "$url" 2>/dev/null) || return 1

    case "$body" in
        *'"result":"0x'*) return 0 ;;
        *) return 1 ;;
    esac
}

resolve_rpc() {
    # Explicit override wins — operator knows best.
    if [[ -n "${FORK_RPC:-}" ]]; then
        echo "$FORK_RPC"
        return
    fi

    if probe_rpc "$PRIMARY_RPC"; then
        echo "$PRIMARY_RPC"
        return
    fi

    echo "primary RPC $PRIMARY_RPC unreachable; falling back to $FALLBACK_RPC" >&2
    if probe_rpc "$FALLBACK_RPC"; then
        echo "$FALLBACK_RPC"
        return
    fi

    echo "both primary ($PRIMARY_RPC) and fallback ($FALLBACK_RPC) RPCs failed the probe" >&2
    exit 1
}

readonly RPC="$(resolve_rpc)"

# ── Anvil launch ─────────────────────────────────────────────────────
ANVIL_ARGS=(
    --fork-url "$RPC"
    --chain-id "$CHAIN_ID"
    --port "$PORT"
    --host 0.0.0.0
    # 3s block time tracks BSC's production cadence closely enough that
    # block-duration histograms and gas-oracle refresh intervals read
    # sensibly during a demo.
    --block-time 3
)

if [[ -n "${FORK_BLOCK:-}" ]]; then
    ANVIL_ARGS+=(--fork-block-number "$FORK_BLOCK")
fi

echo "anvil: forking chain ${CHAIN_ID} from ${RPC}"
if [[ -n "${FORK_BLOCK:-}" ]]; then
    echo "anvil: pinning at block ${FORK_BLOCK}"
else
    echo "anvil: pinning at upstream head (latest)"
fi
echo "anvil: listening on http://127.0.0.1:${PORT} (HTTP + WS)"
echo "anvil: Ctrl-C to stop"
echo

exec anvil "${ANVIL_ARGS[@]}"
