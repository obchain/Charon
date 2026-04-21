# CLAUDE.md

Global rules for any assistant (Claude Code or otherwise) working in
this repository. Humans contributing through a PR review get the same
rules by convention — but these are hard constraints for automated
tools.

---

## Commits

- NEVER add a `Co-Authored-By` line.
- Author + committer must be the human operator of the repo; never
  an assistant, bot, or pair-programming identity.
- Commit messages describe the technical change only. No references
  to AI tools, prompts, models, or this file.
- Follow Conventional Commits. Prefixes in use: `feat(<crate>)`,
  `fix(<crate>)`, `chore`, `docs`, `test`, `refactor`, `perf`.
  Subject ≤ 70 chars, imperative mood, lowercase.
- No emoji in subjects unless a PR template explicitly calls for it.

## Branches and PRs

- `main` is protected. **Never push directly to main.** Every change
  lands via a pull request.
- One branch per GitHub issue: `feat/<N>-<slug>`, `fix/<N>-<slug>`,
  `chore/<N>-<slug>`.
- PR title mirrors the squash-merge subject. PR body must include
  `Closes #<N>` (or `Refs #<N>` for partial progress) so the issue
  tracker stays in sync.
- Squash-merge on acceptance. One commit per PR on `main`.
- PR descriptions: technical only. Same rule as commit messages — no
  AI references.

## Issues

- Issue titles use a `[layer] subject` prefix, e.g.
  `[scanner] Chainlink PriceCache staleness check`.
- Every issue carries `type:*`, `layer:*`, `priority:*`, `status:*`
  labels and a milestone.
- Issue comments and closures: technical only, no AI references.

## Workflow

- Before any commit: run the gates locally.
  - Rust: `cargo fmt --all`, `cargo clippy --workspace --all-targets
    --all-features -- -D warnings`, `cargo test --workspace`.
  - Solidity (Foundry, inside `contracts/`):
    `forge build`, `forge fmt --check`, `forge test`.
- A pre-commit hook (local, not tracked) enforces the Rust gates on
  any assistant-driven commit. Do not bypass with `--no-verify`.

## Scope (v0.1)

- **Venus protocol on BNB Chain only.** Multi-chain / multi-protocol
  expansion is a config change later, not a rewrite.
- Secrets (`.env`, private keys, API tokens) **never** go into the
  repo. Use `${ENV_VAR}` substitution in `config/default.toml`.

## Safety invariants

- The bot hot wallet holds gas only. Profit is swept to the cold
  wallet inside every flash-loan callback. Do not introduce code that
  parks profit in the hot wallet.
- Every liquidation transaction passes an `eth_call` simulation gate
  before broadcast. Do not add a bypass.
- `CharonLiquidator.executeLiquidation` is `onlyOwner`. Do not weaken
  or remove the modifier.
- Flash-loan atomicity is the last line of defense, not the first.
  Off-chain gates (health factor, price freshness, profitability, gas
  ceiling, flash-loan liquidity) must all run before simulation.

## What not to track

- Build artifacts (`target/`, `contracts/out/`, `contracts/cache/`).
- Editor configs (`.idea/`, `.vscode/`).
- Local-only notes and dev collaboration files.
- Anything containing secrets.
