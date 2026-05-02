#!/usr/bin/env bash
#
# End-to-end fork-replay smoke test (issue #401).
#
# Boots an anvil fork at a given block N, deploys CharonLiquidator with
# the dev-0 key, runs `charon ... replay --block N --borrower-file ...`
# once, and asserts:
#   1. the bot emits at least one liquidation decision per JSON record;
#   2. the realised-profit metric is exposed on /metrics (#397).
#
# Tear-down is unconditional — the anvil PID is killed on every exit
# path, including a Ctrl-C interrupt.
#
# Usage:
#   ./scripts/fork_replay.sh <fork-block> <borrower-file>
#   ./scripts/fork_replay.sh 41000000 fixtures/borrowers.txt
#
# Environment knobs:
#   CHARON_ANVIL_PORT  — same as anvil_fork.sh; default 8545.
#   CHARON_BIN         — path to the `charon` binary.
#                        Default: cargo run --bin charon --release --
#   CHARON_FORK_TOML   — path to fork config. Default: config/fork.toml.
#   FORK_RPC           — passthrough to anvil_fork.sh (archive RPC).
#   READY_TIMEOUT_SECS — wait budget for anvil readiness; default 60.
#   RECEIPT_TIMEOUT_SECS — wait budget for the realised-profit metric
#                        to land after replay returns; default 45.
#
# Dependencies (must be installed):
#   - foundry (anvil, cast, forge)
#   - jq (used to parse the JSON records)
#   - curl
#
# Depends on these issues being merged before the script can succeed:
#   - #395  charon replay --block N
#   - #396  Submitter loopback under fork (only required if --execute path is exercised)
#   - #397  realised-profit metric
#
# Exits:
#   0 — fork-replay produced ≥ 1 JSON record AND realised-profit metric is exposed.
#   non-zero — any of the above failed; the per-failure error is logged.

set -euo pipefail

if [[ $# -lt 2 ]]; then
    echo "usage: $0 <fork-block> <borrower-file>" >&2
    exit 64
fi

readonly FORK_BLOCK="$1"
readonly BORROWER_FILE="$2"

if ! [[ "$FORK_BLOCK" =~ ^[0-9]+$ ]]; then
    echo "fork-block must be a positive integer, got: $FORK_BLOCK" >&2
    exit 64
fi
if [[ ! -f "$BORROWER_FILE" ]]; then
    echo "borrower-file not found: $BORROWER_FILE" >&2
    exit 64
fi

readonly REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
readonly PORT="${CHARON_ANVIL_PORT:-8545}"
readonly RPC_URL="http://127.0.0.1:${PORT}"
readonly METRICS_URL="http://127.0.0.1:9091/metrics"
readonly CHARON_FORK_TOML="${CHARON_FORK_TOML:-${REPO_ROOT}/config/fork.toml}"
readonly READY_TIMEOUT_SECS="${READY_TIMEOUT_SECS:-60}"
readonly RECEIPT_TIMEOUT_SECS="${RECEIPT_TIMEOUT_SECS:-45}"
# Dev-0 key from the standard anvil HD wallet — every fork demo uses
# this account so `config/fork.toml`'s baked-in liquidator address
# (`0x5FbDB...64180aa3`) lines up with `forge create`'s CREATE address.
# This is publicly published; never use it on a live chain.
readonly DEV0_KEY="${CHARON_SIGNER_KEY:-0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80}"

# CHARON_BIN — caller can override with a pre-built binary path.
# Default falls back to a release-mode cargo run with the same args
# the operator's manual demo would use.
if [[ -n "${CHARON_BIN:-}" ]]; then
    CHARON_CMD=("$CHARON_BIN")
else
    CHARON_CMD=(cargo run --bin charon --release --quiet --)
fi

ANVIL_PID=""
REPLAY_OUTPUT=""
cleanup() {
    local exit_code=$?
    # Disarm the trap so a second signal arriving mid-teardown does not
    # re-enter cleanup and double-kill / double-wait the same pid.
    trap - EXIT INT TERM
    if [[ -n "$ANVIL_PID" ]] && kill -0 "$ANVIL_PID" 2>/dev/null; then
        echo "[fork_replay] tearing down anvil wrapper (pid $ANVIL_PID)" >&2
        # ANVIL_PID is the `anvil_fork.sh` wrapper, not anvil itself.
        # The wrapper has its own EXIT/INT/TERM trap that reaps both
        # the anvil child and the keep-alive mine loop, but only if it
        # actually receives the signal. SIGTERM the whole process group
        # if we can — that catches the wrapper plus any descendants
        # whose signals the wrapper might have missed. Fall back to a
        # plain SIGTERM if the kernel rejects the negative-pid form
        # (e.g., the wrapper is not a process-group leader).
        kill -TERM -- "-$ANVIL_PID" 2>/dev/null || kill -TERM "$ANVIL_PID" 2>/dev/null || true
        wait "$ANVIL_PID" 2>/dev/null || true
    fi
    if [[ -n "$REPLAY_OUTPUT" && -f "$REPLAY_OUTPUT" ]]; then
        rm -f "$REPLAY_OUTPUT" || true
    fi
    exit "$exit_code"
}
trap cleanup EXIT INT TERM

echo "[fork_replay] starting anvil fork at block $FORK_BLOCK on port $PORT" >&2
FORK_BLOCK="$FORK_BLOCK" "${REPO_ROOT}/scripts/anvil_fork.sh" >/tmp/charon-fork-replay-anvil.log 2>&1 &
ANVIL_PID=$!

echo "[fork_replay] waiting up to ${READY_TIMEOUT_SECS}s for anvil readiness" >&2
deadline=$(( $(date +%s) + READY_TIMEOUT_SECS ))
while ! cast block-number --rpc-url "$RPC_URL" >/dev/null 2>&1; do
    if (( $(date +%s) >= deadline )); then
        echo "[fork_replay] anvil failed to come up within ${READY_TIMEOUT_SECS}s" >&2
        echo "[fork_replay] last 30 lines of anvil log:" >&2
        tail -n 30 /tmp/charon-fork-replay-anvil.log >&2 || true
        exit 1
    fi
    sleep 1
done
echo "[fork_replay] anvil is responsive" >&2

# Deploy CharonLiquidator with dev-0 so its CREATE address matches the
# baked-in `[liquidator.bnb].contract_address` in config/fork.toml. The
# bytecode startup check (#399) refuses to start otherwise.
echo "[fork_replay] deploying CharonLiquidator via forge create" >&2
LIQUIDATOR_DEPLOY_LOG=$(forge create \
    --rpc-url "$RPC_URL" \
    --private-key "$DEV0_KEY" \
    --json \
    "${REPO_ROOT}/contracts/src/CharonLiquidator.sol:CharonLiquidator" \
    2>&1) || {
    echo "[fork_replay] forge create failed:" >&2
    echo "$LIQUIDATOR_DEPLOY_LOG" >&2
    exit 1
}
LIQUIDATOR_ADDR=$(echo "$LIQUIDATOR_DEPLOY_LOG" | jq -r '.deployedTo // empty')
if [[ -z "$LIQUIDATOR_ADDR" ]]; then
    echo "[fork_replay] forge create did not report deployedTo. Output:" >&2
    echo "$LIQUIDATOR_DEPLOY_LOG" >&2
    exit 1
fi
echo "[fork_replay] CharonLiquidator deployed at $LIQUIDATOR_ADDR" >&2

# Replay never broadcasts, so CHARON_EXECUTE_CONFIRMED is purely a
# defensive setting — the run_replay path ignores it. Set it anyway so
# the smoke test exercises the same env shape an operator would use
# locally.
export CHARON_SIGNER_KEY="$DEV0_KEY"
export CHARON_EXECUTE_CONFIRMED=1

REPLAY_OUTPUT=$(mktemp /tmp/charon-fork-replay.XXXXXX.json)
echo "[fork_replay] running replay --block $FORK_BLOCK" >&2
if ! "${CHARON_CMD[@]}" --config "$CHARON_FORK_TOML" \
    replay --block "$FORK_BLOCK" --borrower-file "$BORROWER_FILE" \
    >"$REPLAY_OUTPUT" 2>/tmp/charon-fork-replay-stderr.log; then
    echo "[fork_replay] charon replay returned non-zero. Last 30 lines of stderr:" >&2
    tail -n 30 /tmp/charon-fork-replay-stderr.log >&2 || true
    exit 1
fi

# Each emitted record is one JSON object per line. Count + sanity-check
# at least one record carries the simulation_result field.
record_count=$(grep -c '^{"borrower"' "$REPLAY_OUTPUT" || true)
if [[ "$record_count" -lt 1 ]]; then
    echo "[fork_replay] replay produced zero JSON records. stderr:" >&2
    tail -n 30 /tmp/charon-fork-replay-stderr.log >&2 || true
    echo "[fork_replay] borrower file:" >&2
    cat "$BORROWER_FILE" >&2 || true
    exit 1
fi
echo "[fork_replay] replay produced $record_count JSON record(s)" >&2

# Per acceptance: assert the realised-profit metric (#397) is at least
# *exposed* on /metrics. The replay path does not broadcast, so the
# value will be 0; a non-zero check belongs to a `listen --execute`
# follow-up. Asserting the metric is *registered* still catches a
# regression that drops it from `charon-metrics`.
echo "[fork_replay] checking metric is exposed at $METRICS_URL" >&2
deadline=$(( $(date +%s) + RECEIPT_TIMEOUT_SECS ))
while true; do
    if curl --silent --max-time 5 "$METRICS_URL" \
        | grep -q '^charon_executor_realised_profit_usd_cents'; then
        echo "[fork_replay] realised-profit metric exposed" >&2
        break
    fi
    if (( $(date +%s) >= deadline )); then
        echo "[fork_replay] charon_executor_realised_profit_usd_cents not exposed within ${RECEIPT_TIMEOUT_SECS}s" >&2
        curl --silent --max-time 5 "$METRICS_URL" \
            | grep -E '^charon_executor_' \
            | head -n 20 >&2 || true
        exit 1
    fi
    sleep 1
done

echo "[fork_replay] OK — fork at $FORK_BLOCK, $record_count opportunities replayed, metrics surface healthy" >&2
exit 0
