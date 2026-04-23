#!/usr/bin/env bash
#
# Boot a local anvil fork of BNB Smart Chain mainnet so the full
# Charon liquidation path (scanner → profit → Aave V3 flashloan →
# Venus liquidate → PancakeSwap swap) can be demonstrated without
# real funds.
#
# Usage:
#   ./scripts/anvil_fork.sh                # fork at the pinned default block
#   FORK_BLOCK=41000000 ./scripts/anvil_fork.sh
#   FORK_BLOCK=latest  ./scripts/anvil_fork.sh   # unpinned (discouraged)
#   FORK_RPC=https://custom/bsc ./scripts/anvil_fork.sh
#   FORK_PORT=8546 ./scripts/anvil_fork.sh   # avoid a port collision
#
# Environment knobs:
#   FORK_RPC      — explicit upstream; skips the default probe when set
#   FORK_BLOCK    — fork at this block; default `DEFAULT_FORK_BLOCK`.
#                   Set to the literal string `latest` to follow upstream
#                   head — not recommended for CI or soak tests because
#                   state drift across runs breaks reproducibility (#242).
#   FORK_PORT     — host port for HTTP+WS (default: 8545)
#   FORK_CHAIN_ID — preserved chain id (default: 56, BSC mainnet)
#
# Upstream:
#   The default upstream is dRPC (free, keyless, archive — historical
#   eth_call works against any block). If dRPC is unreachable the
#   script exits non-zero rather than falling back to PublicNode;
#   PublicNode is not an archive node (~128 blocks of state), so a
#   fork built against it silently returns "missing trie node" on
#   every historical call and defeats the fork (#246). Override with
#   FORK_RPC=<your-archive-url> to use a different archive provider.

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
readonly PORT="${FORK_PORT:-8545}"
readonly CHAIN_ID="${FORK_CHAIN_ID:-56}"
# Default fork block. Captured 2026-04-23, past every Aave V3 reserve
# activation and every Venus Core Pool vToken deployment the demo
# uses. The fork-test suite on `feat/25-foundry-fork-tests` pins the
# same value so a soak demo and the Foundry regression suite describe
# identical on-chain state. Bump in a dedicated reviewed commit when
# refreshing against a newer baseline.
readonly DEFAULT_FORK_BLOCK="${DEFAULT_FORK_BLOCK:-94000000}"

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

    echo "error: primary RPC $PRIMARY_RPC failed the probe" >&2
    echo "       refusing to fall back to a non-archive public provider —" >&2
    echo "       forked historical eth_call would return 'missing trie node'." >&2
    echo "       pass FORK_RPC=<your-archive-url> to override." >&2
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

# Resolve the effective fork block. Unset ⇒ the pinned default (for
# reproducible runs); `latest` ⇒ follow upstream head; anything else ⇒
# pin at that block.
FORK_BLOCK_EFFECTIVE="${FORK_BLOCK:-$DEFAULT_FORK_BLOCK}"
if [[ "$FORK_BLOCK_EFFECTIVE" != "latest" ]]; then
    ANVIL_ARGS+=(--fork-block-number "$FORK_BLOCK_EFFECTIVE")
fi

echo "anvil: forking chain ${CHAIN_ID} from ${RPC}"
if [[ "$FORK_BLOCK_EFFECTIVE" == "latest" ]]; then
    echo "anvil: pinning at upstream head (latest) — unpinned, not reproducible"
else
    echo "anvil: pinning at block ${FORK_BLOCK_EFFECTIVE}"
fi
echo "anvil: listening on http://127.0.0.1:${PORT} (HTTP + WS)"
echo "anvil: Ctrl-C to stop"
echo

exec anvil "${ANVIL_ARGS[@]}"
