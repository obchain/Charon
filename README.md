# Charon

A multi-chain flash-loan liquidation bot. v0.1 targets Venus Protocol on BNB Smart Chain.

Charon watches borrower health factors in real time, quotes a flash loan when a position goes underwater, simulates the full liquidation round-trip (`flashLoan` → `liquidate` → swap collateral → repay), and submits the transaction once the gross-minus-fees profit clears the operator's USD threshold.

## Build

```sh
cargo build --workspace
```

Full gate (must pass before every PR):

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
(cd contracts && forge build && forge test)
```

## Run modes

Three config profiles ship in [`config/`](config/). Pick one with `--config`.

### 1. Mainnet (production)

```sh
cp .env.example .env      # fill in RPC + private keys
charon --config config/default.toml listen
```

Requires `BNB_WS_URL`, `BNB_HTTP_URL`, optionally `BSC_PRIVATE_RPC_URL` and `BOT_SIGNER_KEY`.

### 2. BSC testnet (Chapel, chainId 97)

Live Venus deployment, zero capital risk. Aave V3 is not on Chapel, so the bot runs read-only: scanner and metrics populate, the opportunity path short-circuits. Useful for metrics dashboards.

```sh
charon --config config/testnet.toml listen
```

Requires `BNB_TESTNET_WS_URL`, `BNB_TESTNET_HTTP_URL` (defaults in `.env.example` point at PublicNode).

### 3. Local anvil fork (full end-to-end, no capital)

Fork BSC mainnet locally. Real Venus state, real Aave V3, real PancakeSwap. Liquidate real positions against a private chain.

Terminal A — boot the fork:

```sh
./scripts/anvil_fork.sh              # forks latest, primary RPC is dRPC
FORK_BLOCK=41000000 ./scripts/anvil_fork.sh   # pin a specific block
```

Terminal B — run Charon against it:

```sh
charon --config config/fork.toml listen
```

The script probes `https://bsc.drpc.org` first (free, keyless, archive). If the primary is unreachable it falls back to `https://bsc-rpc.publicnode.com`. Override with `FORK_RPC=<url>` when you have your own node.

## Metrics

Every profile ships with Prometheus exporter enabled. Scrape `http://<host>:9091/metrics`.

Key series (full list: [`crates/charon-metrics/src/lib.rs`](crates/charon-metrics/src/lib.rs)):

| Metric | Type | Labels |
| --- | --- | --- |
| `charon_scanner_blocks_total` | counter | chain |
| `charon_scanner_positions` | gauge | chain, bucket |
| `charon_pipeline_block_duration_seconds` | histogram | chain |
| `charon_executor_simulations_total` | counter | chain, result |
| `charon_executor_opportunities_queued_total` | counter | chain |
| `charon_executor_opportunities_dropped_total` | counter | chain, stage |
| `charon_executor_profit_usd_cents` | histogram | chain |
| `charon_executor_queue_depth` | gauge | — |

The exporter binds `:9091` (not `:9090`) so it doesn't collide with a co-located Prometheus server.

### Grafana dashboard

A ready-to-import dashboard lives at [`deploy/grafana/charon.json`](deploy/grafana/charon.json). Three steps to load it into Grafana or Grafana Cloud:

1. Add a Prometheus data source that scrapes `http://<charon-host>:9091/metrics` (every ~10 s is fine).
2. In Grafana, **Dashboards → New → Import → Upload JSON file** and pick the file above.
3. On the import screen, select the Prometheus data source you created and click **Import**.

Dashboard UID is `charon-v0` and tags are `charon`, `liquidation`, `defi` — re-importing over an existing copy replaces it rather than duplicating. Variables (`Chain`, `Instance`) auto-populate from label values once metrics start flowing.

## Repository layout

```
crates/
  charon-core/       shared types, config loader, profit calc, queue
  charon-scanner/    chain listener, health scanner, price cache, mempool watcher
  charon-protocols/  Venus adapter (Comptroller + vToken bindings)
  charon-flashloan/  Aave V3 adapter + router
  charon-executor/   tx builder, simulator, gas oracle, nonce manager, submitter, batcher
  charon-metrics/    Prometheus exporter and metric-name constants
  charon-cli/        `charon` binary wiring everything together
contracts/           CharonLiquidator.sol + Foundry suite
config/              TOML profiles (default, testnet, fork)
scripts/             operator helpers (anvil_fork.sh, ...)
```

## License

MIT.
