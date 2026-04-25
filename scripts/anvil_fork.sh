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
#   CHARON_ANVIL_PORT=8546 ./scripts/anvil_fork.sh   # avoid a port collision
#
# Environment knobs:
#   FORK_RPC                — explicit upstream; skips the default probe when set
#   FORK_BLOCK              — fork at this block; default `DEFAULT_FORK_BLOCK`.
#                             Set to the literal string `latest` to follow upstream
#                             head — not recommended for CI or soak tests because
#                             state drift across runs breaks reproducibility (#242).
#   CHARON_ANVIL_PORT       — host port for HTTP+WS (default: 8545). This is the
#                             same variable `config/fork.toml` reads via
#                             `${CHARON_ANVIL_PORT:-8545}`, so script and config
#                             agree on the port without the operator editing TOML (#247).
#   FORK_CHAIN_ID           — preserved chain id (default: 56, BSC mainnet)
#   FORK_MINE_INTERVAL_SECS — seconds between background anvil_mine
#                             calls (default: 30). Set to 0 to disable the keep-
#                             alive loop entirely (see stale-Chainlink note below).
#
# Foundry version pin (#259):
#   Foundry CLI output is reformatted across releases (nightly channel
#   changed the `forge --version` template in late 2024). Rather than
#   parse a moving target, we compare the raw first-line version
#   stamp against `CHARON_REQUIRED_FOUNDRY_VERSION` and warn — not
#   hard-fail — if they don't match. The goal is that a fresh clone
#   six months from now either sees the known-good stamp or gets a
#   loud remediation hint instead of silently running against a
#   version whose anvil behavior has drifted.
#
#   Override knobs:
#     CHARON_REQUIRED_FOUNDRY_VERSION     — expected substring in `anvil --version`
#                                           output (default pinned below).
#     CHARON_SKIP_FOUNDRY_VERSION_CHECK=1 — bypass the check entirely. Intended
#                                           for CI images that pin Foundry out
#                                           of band; local devs should run
#                                           `foundryup -v <version>` instead.
#
# Upstream:
#   The default upstream is dRPC (free, keyless, archive — historical
#   eth_call works against any block). If dRPC is unreachable the
#   script exits non-zero rather than falling back to PublicNode;
#   PublicNode is not an archive node (~128 blocks of state), so a
#   fork built against it silently returns "missing trie node" on
#   every historical call and defeats the fork (#246). Override with
#   FORK_RPC=<your-archive-url> to use a different archive provider.
#
# Stale-Chainlink keep-alive (#244):
#   Chainlink aggregators on the forked chain stop updating the instant
#   the fork is pinned — upstream keeps writing new rounds, but the
#   fork's state is frozen at the pin block. Charon's PriceCache
#   rejects any feed older than `DEFAULT_MAX_AGE`, so within ~10 minutes
#   of fork-time every feed looks stale, the scanner's health-factor
#   math can't price collateral, and the Grafana demo degrades to a
#   flat graph with zero liquidatable positions.
#
#   Mitigation: this script runs a background loop that issues
#   `cast rpc anvil_mine 1` every `FORK_MINE_INTERVAL_SECS` against the
#   local RPC. Each call advances the fork's wall clock by one block's
#   worth of time, which moves `block.timestamp` forward and — because
#   Chainlink freshness is measured against `block.timestamp` — keeps
#   the feeds inside the cache's freshness window. --block-time 3 keeps
#   organic blocks flowing for the listener; this extra nudge exists
#   purely to outrun the freshness gate during idle stretches.
#
#   Alternative if `cast` is unavailable: set
#   `CHARON_PRICE_MAX_AGE_SECS=86400` before starting charon and set
#   `FORK_MINE_INTERVAL_SECS=0` here to disable the loop.
#
# Process lifecycle (#240):
#   anvil is launched in the background with a tracked PID so a
#   `trap cleanup EXIT INT TERM` handler can tear both the node and the
#   mine loop down together when the script exits or the operator
#   hits Ctrl-C. `wait "$ANVIL_PID"` keeps the script in the foreground
#   so Ctrl-C still propagates; the prior `exec anvil` tail left no
#   room to background a mining loop alongside the node.

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

# ── Foundry version pin (#259) ───────────────────────────────────────
# Warn loudly if the installed anvil doesn't match the known-good
# stamp. We don't hard-fail on mismatch because (a) Foundry release
# cadence is weekly and the version string format has drifted, and
# (b) CI images pin their own versions. Hard-fail only when the
# operator can't produce a version string at all — that's a broken
# install, not a drift.
readonly REQUIRED_FOUNDRY_VERSION="${CHARON_REQUIRED_FOUNDRY_VERSION:-stable}"
if [[ "${CHARON_SKIP_FOUNDRY_VERSION_CHECK:-0}" != "1" ]]; then
    # `anvil --version` prints e.g. `anvil 0.3.0-stable (...)` or a
    # `nightly (...)` line depending on channel. Grab the first line.
    if ! anvil_version_line=$(anvil --version 2>/dev/null | head -n1); then
        echo "anvil: failed to read 'anvil --version' output — install appears broken." >&2
        echo "       reinstall Foundry: curl -L https://foundry.paradigm.xyz | bash && foundryup" >&2
        exit 127
    fi
    if [[ -z "$anvil_version_line" ]]; then
        echo "anvil: 'anvil --version' produced empty output — install appears broken." >&2
        exit 127
    fi
    if [[ "$anvil_version_line" != *"$REQUIRED_FOUNDRY_VERSION"* ]]; then
        echo "anvil: WARNING — installed version may not match the pin." >&2
        echo "       required substring: '$REQUIRED_FOUNDRY_VERSION'" >&2
        echo "       installed:          '$anvil_version_line'" >&2
        echo "       remediate:          foundryup -i $REQUIRED_FOUNDRY_VERSION" >&2
        echo "       or bypass:          CHARON_SKIP_FOUNDRY_VERSION_CHECK=1 ./scripts/anvil_fork.sh" >&2
        echo "       continuing in 2s — anvil semantics may have drifted." >&2
        sleep 2
    else
        echo "anvil: version ok ($anvil_version_line)"
    fi
fi

# cast is only strictly required for the stale-Chainlink keep-alive.
# If it's missing and the loop is enabled, fail loudly — a silent
# fallback would reproduce exactly the Grafana-looks-dead failure mode
# the loop exists to prevent.
readonly MINE_INTERVAL_SECS="${FORK_MINE_INTERVAL_SECS:-30}"
if [[ "$MINE_INTERVAL_SECS" != "0" ]] && ! command -v cast >/dev/null 2>&1; then
    echo "cast (Foundry) not found in PATH — required for the Chainlink keep-alive loop." >&2
    echo "       install Foundry, or set FORK_MINE_INTERVAL_SECS=0 and run charon with" >&2
    echo "       CHARON_PRICE_MAX_AGE_SECS=86400 to bypass the freshness gate instead." >&2
    exit 127
fi

# ── Defaults ─────────────────────────────────────────────────────────
readonly PRIMARY_RPC="${FORK_RPC_PRIMARY:-https://bsc.drpc.org}"
# Port name matches `config/fork.toml`'s `${CHARON_ANVIL_PORT:-8545}`
# substitution so `CHARON_ANVIL_PORT=8546 ./anvil_fork.sh` and
# `CHARON_ANVIL_PORT=8546 charon --config config/fork.toml` agree
# without editing TOML (#247). `FORK_PORT` is honored as a legacy
# alias so existing operator muscle memory still works; prefer
# `CHARON_ANVIL_PORT` for new invocations.
readonly PORT="${CHARON_ANVIL_PORT:-${FORK_PORT:-8545}}"
readonly CHAIN_ID="${FORK_CHAIN_ID:-56}"
readonly LOCAL_RPC="http://127.0.0.1:${PORT}"
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
    # Throttle upstream call rate so free-tier dRPC / Onfinality / Ankr
    # endpoints don't 429 us during the heavy boot-time snapshot of all
    # vToken markets and Chainlink feeds. Override with FORK_CUPS=0 to
    # disable, or pass a higher value if upstream is keyed.
    --compute-units-per-second "${FORK_CUPS:-150}"
    --fork-retry-backoff "${FORK_RETRY_BACKOFF:-1500}"
    --retries "${FORK_RETRIES:-15}"
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
echo "anvil: listening on ${LOCAL_RPC} (HTTP + WS)"
echo "anvil: Ctrl-C to stop"
echo

# Track background PIDs so the cleanup trap can reap them on any exit
# path — normal termination, operator Ctrl-C, or a shell error under
# `set -e`. Initialize to empty so `kill` in cleanup can no-op safely
# if we never got as far as launching a given child.
ANVIL_PID=""
MINE_PID=""

cleanup() {
    # Disable the trap inside cleanup so a signal arriving mid-teardown
    # doesn't re-enter this function. Use `|| true` on kills so an
    # already-dead child doesn't trip `set -e` on the way out.
    trap - EXIT INT TERM
    if [[ -n "$MINE_PID" ]] && kill -0 "$MINE_PID" 2>/dev/null; then
        kill "$MINE_PID" 2>/dev/null || true
        wait "$MINE_PID" 2>/dev/null || true
    fi
    if [[ -n "$ANVIL_PID" ]] && kill -0 "$ANVIL_PID" 2>/dev/null; then
        kill "$ANVIL_PID" 2>/dev/null || true
        wait "$ANVIL_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

anvil "${ANVIL_ARGS[@]}" &
ANVIL_PID=$!

# ── Wait for anvil to accept RPC before kicking off the mine loop ────
# Without this probe the first `cast rpc anvil_mine` would race the
# node startup, log a connection-refused error, and burn a retry
# budget for no reason. Reuse the same eth_blockNumber check the
# upstream probe uses — anvil answers it the moment the HTTP server
# binds.
READINESS_TIMEOUT_SECS=30
readiness_deadline=$(( $(date +%s) + READINESS_TIMEOUT_SECS ))
while ! probe_rpc "$LOCAL_RPC"; do
    if ! kill -0 "$ANVIL_PID" 2>/dev/null; then
        echo "anvil: process exited before becoming ready — see output above" >&2
        exit 1
    fi
    if (( $(date +%s) >= readiness_deadline )); then
        echo "anvil: still not answering eth_blockNumber on ${LOCAL_RPC} after ${READINESS_TIMEOUT_SECS}s" >&2
        exit 1
    fi
    sleep 1
done

# ── Background keep-alive for Chainlink freshness (#244) ─────────────
if [[ "$MINE_INTERVAL_SECS" != "0" ]]; then
    echo "anvil: keep-alive enabled — mining 1 extra block every ${MINE_INTERVAL_SECS}s to keep Chainlink feeds fresh"
    (
        # Silence transient errors — if a single anvil_mine call fails
        # (e.g., during shutdown) we don't want to take down the whole
        # script. The trap on the parent handles real termination.
        while sleep "$MINE_INTERVAL_SECS"; do
            cast rpc anvil_mine 1 --rpc-url "$LOCAL_RPC" >/dev/null 2>&1 || true
        done
    ) &
    MINE_PID=$!
fi

# Foreground wait on anvil so Ctrl-C reaches the shell, the trap
# fires, and both children are reaped. `wait` returns the child's
# exit status; `|| true` prevents a non-zero anvil exit from
# short-circuiting the trap under `set -e`.
wait "$ANVIL_PID" || true
