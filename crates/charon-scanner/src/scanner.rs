//! Health-factor scanner — 3-bucket classifier + per-bucket scheduler.
//!
//! Protocol adapters supply positions; the scanner classifies each into
//! one of three buckets based on its `health_factor`:
//!
//! * **Liquidatable** (HOT) — `hf < liquidatable_threshold` (1.0 by default).
//! * **NearLiquidation** (WARM) — `liquidatable ≤ hf < near_liq`.
//! * **Healthy** (COLD) — everything else.
//!
//! The [`ScanScheduler`] answers "do I re-scan this bucket on this block?"
//! from the configured `{hot,warm,cold}_scan_blocks` cadence, so the scanner
//! does not burn RPC on a COLD bucket every block.
//!
//! Storage is a single [`DashMap`] — lock-free, shard-partitioned. The map
//! supports `prune()` so borrowers that fully repay are removed and do
//! not linger as stale Liquidatable entries forever.

use std::collections::HashSet;

use alloy::primitives::{Address, U256};
use charon_core::Position;
use dashmap::DashMap;
use tracing::warn;

/// Which classification bucket a borrower's position currently falls into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PositionBucket {
    /// Safely over-collateralized; nothing to do (COLD).
    Healthy,
    /// Close to the liquidation boundary (WARM).
    NearLiquidation,
    /// Currently liquidatable (HOT).
    Liquidatable,
}

impl PositionBucket {
    fn label(self) -> &'static str {
        match self {
            PositionBucket::Healthy => "healthy",
            PositionBucket::NearLiquidation => "near_liquidation",
            PositionBucket::Liquidatable => "liquidatable",
        }
    }
}

#[derive(Debug, Clone)]
pub struct BucketedPosition {
    pub position: Position,
    pub bucket: PositionBucket,
}

#[derive(Debug, Clone, Default)]
pub struct BucketCounts {
    pub healthy: usize,
    pub near_liquidation: usize,
    pub liquidatable: usize,
}

impl BucketCounts {
    pub fn total(&self) -> usize {
        self.healthy + self.near_liquidation + self.liquidatable
    }
}

/// Per-bucket scan cadence driver.
///
/// `should_scan(bucket, block)` returns true when the given block number
/// falls on the bucket's cadence. HOT cadence is usually 1 (every block).
///
/// `phase_offset` shifts the modulo predicate so two bots with the
/// same cadence land on different blocks. With `cold = 100` and
/// `phase_offset = 0` two bots both scan on block 100/200/300/...,
/// burst-loading the public RPC. Setting `phase_offset` to a stable
/// per-bot value (e.g. derived from the signer address) staggers the
/// bursts across the cadence window — see `with_phase_offset` and the
/// CLI startup that derives a deterministic offset from
/// `keccak256(signer_address || chain_id)`.
#[derive(Debug, Clone, Copy)]
pub struct ScanScheduler {
    pub hot: u64,
    pub warm: u64,
    pub cold: u64,
    /// Constant offset added to `block` before the modulo. `0` reproduces
    /// pre-#366 behaviour and is the test-friendly default.
    pub phase_offset: u64,
}

impl ScanScheduler {
    pub fn new(hot: u64, warm: u64, cold: u64) -> Self {
        Self {
            hot: hot.max(1),
            warm: warm.max(1),
            cold: cold.max(1),
            phase_offset: 0,
        }
    }

    /// Return a copy of `self` with `phase_offset` overridden.
    /// Builder-style so existing call sites don't have to change shape.
    pub fn with_phase_offset(mut self, phase_offset: u64) -> Self {
        self.phase_offset = phase_offset;
        self
    }

    pub fn should_scan(&self, bucket: PositionBucket, block: u64) -> bool {
        let period = match bucket {
            PositionBucket::Liquidatable => self.hot,
            PositionBucket::NearLiquidation => self.warm,
            PositionBucket::Healthy => self.cold,
        };
        // `period` is normalised to `>= 1` in `new`, so the modulo is
        // safe. `wrapping_add` matches the previous semantics on a
        // hypothetical near-u64::MAX block height (BSC will not reach
        // that in any meaningful timeframe; the wrap is for purity).
        block.wrapping_add(self.phase_offset) % period == 0
    }
}

/// Derive a stable phase offset for [`ScanScheduler::with_phase_offset`]
/// from `seed_bytes` and a `period_max`. The offset is uniform on
/// `0..period_max` and stable across restarts, so two bots in the
/// same swarm with different signer addresses land on disjoint
/// modulo classes for any cadence dividing `period_max`.
///
/// `seed_bytes` is typically the bot signer address concatenated with
/// the chain id; the CLI computes it that way at startup.
pub fn derive_phase_offset(seed_bytes: &[u8], period_max: u64) -> u64 {
    if period_max <= 1 {
        return 0;
    }
    let digest = alloy::primitives::keccak256(seed_bytes);
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&digest.0[0..8]);
    u64::from_be_bytes(buf) % period_max
}

/// 3-bucket health-factor scanner.
pub struct HealthScanner {
    liquidatable_threshold: U256,
    near_liq_threshold: U256,
    positions: DashMap<Address, BucketedPosition>,
}

impl HealthScanner {
    /// Build a scanner from basis-point thresholds of 1e18 (10_000 = 1.0).
    pub fn new(liquidatable_bps: u32, near_liq_bps: u32) -> anyhow::Result<Self> {
        if liquidatable_bps > near_liq_bps {
            anyhow::bail!(
                "liquidatable_threshold_bps ({liquidatable_bps}) must be ≤ near_liq_threshold_bps ({near_liq_bps})"
            );
        }
        Ok(Self {
            liquidatable_threshold: bps_to_1e18(liquidatable_bps),
            near_liq_threshold: bps_to_1e18(near_liq_bps),
            positions: DashMap::new(),
        })
    }

    /// Warn on startup if the supplied positions all carry the known
    /// binary-HF sentinel values (0 / 2e18) emitted by adapters that have
    /// not yet implemented a real ratio — otherwise the NearLiquidation
    /// bucket is silently dead code.
    pub fn warn_if_binary_sentinel(&self, sample: &[Position]) {
        if sample.is_empty() {
            return;
        }
        let scale = U256::from(10u64).pow(U256::from(18u64));
        let binary = sample
            .iter()
            .all(|p| p.health_factor == U256::ZERO || p.health_factor == scale * U256::from(2u64));
        if binary {
            warn!(
                "every observed health_factor is 0 or 2e18 — adapter appears to emit a binary sentinel; \
                 NearLiquidation bucket will never populate until real HF is computed"
            );
        }
    }

    pub fn classify(&self, health_factor: U256) -> PositionBucket {
        if health_factor < self.liquidatable_threshold {
            PositionBucket::Liquidatable
        } else if health_factor < self.near_liq_threshold {
            PositionBucket::NearLiquidation
        } else {
            PositionBucket::Healthy
        }
    }

    /// Upsert a batch of freshly-fetched positions. Detects per-borrower
    /// bucket transitions and increments `charon_scanner_transitions_total`.
    pub fn upsert(&self, positions: impl IntoIterator<Item = Position>) {
        for p in positions {
            let new_bucket = self.classify(p.health_factor);
            let prev_bucket = self.positions.get(&p.borrower).map(|e| e.value().bucket);
            self.positions.insert(
                p.borrower,
                BucketedPosition {
                    position: p,
                    bucket: new_bucket,
                },
            );
            if let Some(prev) = prev_bucket {
                if prev != new_bucket {
                    metrics::counter!(
                        "charon_scanner_transitions_total",
                        "from" => prev.label(),
                        "to" => new_bucket.label()
                    )
                    .increment(1);
                }
            }
        }
        self.publish_gauges();
    }

    /// Remove a single borrower (e.g. after full repayment detected).
    pub fn remove(&self, borrower: &Address) {
        self.positions.remove(borrower);
        self.publish_gauges();
    }

    /// Drop every tracked borrower that is **not** in `current`. Called by
    /// the scan loop after `upsert(fresh_positions)` so positions whose debt
    /// has been repaid (and thus no longer appear in the adapter response)
    /// stop being reported as Liquidatable.
    pub fn prune(&self, current: &[Position]) {
        let keep: HashSet<Address> = current.iter().map(|p| p.borrower).collect();
        self.positions.retain(|addr, _| keep.contains(addr));
        self.publish_gauges();
    }

    pub fn bucket_counts(&self) -> BucketCounts {
        let mut counts = BucketCounts::default();
        for entry in self.positions.iter() {
            match entry.value().bucket {
                PositionBucket::Healthy => counts.healthy += 1,
                PositionBucket::NearLiquidation => counts.near_liquidation += 1,
                PositionBucket::Liquidatable => counts.liquidatable += 1,
            }
        }
        counts
    }

    /// Update the `charon_scanner_borrowers_in_bucket{bucket}` gauges.
    fn publish_gauges(&self) {
        let counts = self.bucket_counts();
        metrics::gauge!("charon_scanner_borrowers_in_bucket", "bucket" => "healthy")
            .set(counts.healthy as f64);
        metrics::gauge!("charon_scanner_borrowers_in_bucket", "bucket" => "near_liquidation")
            .set(counts.near_liquidation as f64);
        metrics::gauge!("charon_scanner_borrowers_in_bucket", "bucket" => "liquidatable")
            .set(counts.liquidatable as f64);
    }

    pub fn liquidatable(&self) -> Vec<Position> {
        self.positions
            .iter()
            .filter(|e| e.value().bucket == PositionBucket::Liquidatable)
            .map(|e| e.value().position.clone())
            .collect()
    }

    pub fn near_liquidation(&self) -> Vec<Position> {
        self.positions
            .iter()
            .filter(|e| e.value().bucket == PositionBucket::NearLiquidation)
            .map(|e| e.value().position.clone())
            .collect()
    }

    /// Return the borrowers currently assigned to `bucket`. Used by the
    /// scan scheduler to fetch only the subset that is due this block.
    pub fn borrowers_in_bucket(&self, bucket: PositionBucket) -> Vec<Address> {
        self.positions
            .iter()
            .filter(|e| e.value().bucket == bucket)
            .map(|e| *e.key())
            .collect()
    }
}

/// Convert a basis-point value into a 1e18-fixed `U256`. 10_000 bps == 1.0e18.
/// Integer arithmetic only — no f64 at any point.
pub fn bps_to_1e18(bps: u32) -> U256 {
    // 1 bps = 1e14 in 1e18 scale.
    U256::from(bps) * U256::from(10u64).pow(U256::from(14u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;
    use charon_core::ProtocolId;

    fn one_e18() -> U256 {
        U256::from(10u64).pow(U256::from(18u64))
    }

    fn mk_position(borrower_byte: u8, hf: U256) -> Position {
        let mut bytes = [0u8; 20];
        bytes[19] = borrower_byte;
        Position {
            protocol: ProtocolId::Venus,
            chain_id: 56,
            borrower: Address::from(bytes),
            collateral_token: address!("0000000000000000000000000000000000000001"),
            debt_token: address!("0000000000000000000000000000000000000002"),
            collateral_amount: U256::ZERO,
            debt_amount: U256::ZERO,
            health_factor: hf,
            liquidation_bonus_bps: 1000,
        }
    }

    #[test]
    fn bps_to_1e18_exact_boundary() {
        // 10_500 bps must be exactly 1.05e18 — catches the old f64 ULP bug.
        assert_eq!(
            bps_to_1e18(10_500),
            U256::from(1_050_000_000_000_000_000u128)
        );
        assert_eq!(
            bps_to_1e18(10_000),
            U256::from(1_000_000_000_000_000_000u128)
        );
    }

    #[test]
    fn classify_partitions_positions_into_three_buckets() {
        let s = HealthScanner::new(10_000, 10_500).unwrap();
        let e18 = one_e18();

        assert_eq!(
            s.classify(e18 / U256::from(2u64)),
            PositionBucket::Liquidatable
        );
        assert_eq!(s.classify(e18), PositionBucket::NearLiquidation);
        let p104 = U256::from(1_040_000_000_000_000_000u128);
        assert_eq!(s.classify(p104), PositionBucket::NearLiquidation);
        let p105 = U256::from(1_050_000_000_000_000_000u128);
        assert_eq!(s.classify(p105), PositionBucket::Healthy);
        assert_eq!(s.classify(e18 * U256::from(2u64)), PositionBucket::Healthy);
    }

    #[test]
    fn upsert_updates_buckets_and_counts() {
        let s = HealthScanner::new(10_000, 10_500).unwrap();
        let e18 = one_e18();
        s.upsert([
            mk_position(1, U256::from(0u64)),
            mk_position(2, U256::from(1_020_000_000_000_000_000u128)),
            mk_position(3, e18 * U256::from(3u64)),
        ]);
        let c = s.bucket_counts();
        assert_eq!(c.liquidatable, 1);
        assert_eq!(c.near_liquidation, 1);
        assert_eq!(c.healthy, 1);
    }

    #[test]
    fn prune_drops_repaid_borrowers() {
        let s = HealthScanner::new(10_000, 10_500).unwrap();
        s.upsert([
            mk_position(1, U256::from(0u64)),
            mk_position(2, U256::from(0u64)),
        ]);
        assert_eq!(s.bucket_counts().liquidatable, 2);
        // Only borrower 2 still has a position after repayment.
        s.prune(&[mk_position(2, U256::from(0u64))]);
        assert_eq!(s.bucket_counts().liquidatable, 1);
    }

    #[test]
    fn remove_drops_single_borrower() {
        let s = HealthScanner::new(10_000, 10_500).unwrap();
        s.upsert([mk_position(1, U256::from(0u64))]);
        let mut bytes = [0u8; 20];
        bytes[19] = 1;
        s.remove(&Address::from(bytes));
        assert_eq!(s.bucket_counts().total(), 0);
    }

    #[test]
    fn scheduler_gates_per_bucket_cadence() {
        let sched = ScanScheduler::new(1, 10, 100);
        assert!(sched.should_scan(PositionBucket::Liquidatable, 17));
        assert!(sched.should_scan(PositionBucket::NearLiquidation, 20));
        assert!(!sched.should_scan(PositionBucket::NearLiquidation, 21));
        assert!(sched.should_scan(PositionBucket::Healthy, 100));
        assert!(!sched.should_scan(PositionBucket::Healthy, 101));
    }

    #[test]
    fn rejects_inverted_thresholds() {
        assert!(HealthScanner::new(10_500, 10_000).is_err());
    }

    /// `phase_offset = 0` reproduces the pre-#366 modulo-only behaviour.
    #[test]
    fn phase_offset_zero_matches_legacy_modulo() {
        let sched = ScanScheduler::new(1, 10, 100);
        for block in (0u64..1_000).step_by(13) {
            let legacy_hot = true; // period = 1 ⇒ always
            let legacy_warm = block % 10 == 0;
            let legacy_cold = block % 100 == 0;
            assert_eq!(
                sched.should_scan(PositionBucket::Liquidatable, block),
                legacy_hot
            );
            assert_eq!(
                sched.should_scan(PositionBucket::NearLiquidation, block),
                legacy_warm
            );
            assert_eq!(
                sched.should_scan(PositionBucket::Healthy, block),
                legacy_cold
            );
        }
    }

    /// Two bots with distinct phase offsets must never both fire a
    /// `cold = 100` scan on the same block — the entire point of the
    /// offset is to break swarm lockstep.
    #[test]
    fn distinct_phase_offsets_never_coincide_in_one_period() {
        let bot_a = ScanScheduler::new(1, 10, 100).with_phase_offset(0);
        let bot_b = ScanScheduler::new(1, 10, 100).with_phase_offset(37);
        let mut both_scan = 0;
        for block in 0u64..10_000 {
            if bot_a.should_scan(PositionBucket::Healthy, block)
                && bot_b.should_scan(PositionBucket::Healthy, block)
            {
                both_scan += 1;
            }
        }
        assert_eq!(
            both_scan, 0,
            "distinct phase offsets must never produce a shared cold-scan block"
        );
    }

    /// `period = 1` (hot bucket default) must scan every block
    /// regardless of phase offset — the offset is a stagger between
    /// bots, not a mute switch.
    #[test]
    fn phase_offset_does_not_silence_period_one() {
        for offset in [0, 1, 7, 99, u64::MAX / 2] {
            let sched = ScanScheduler::new(1, 10, 100).with_phase_offset(offset);
            for block in 0u64..50 {
                assert!(
                    sched.should_scan(PositionBucket::Liquidatable, block),
                    "period=1 must always scan; offset={offset} block={block}"
                );
            }
        }
    }

    /// Derived phase offsets are stable, deterministic, and constrained
    /// to `0..period_max`.
    #[test]
    fn derive_phase_offset_is_deterministic_and_in_range() {
        let seed_a = b"signer_a:chain_56";
        let seed_b = b"signer_b:chain_56";
        let a1 = derive_phase_offset(seed_a, 100);
        let a2 = derive_phase_offset(seed_a, 100);
        let b1 = derive_phase_offset(seed_b, 100);
        assert_eq!(a1, a2, "must be deterministic across calls");
        assert!(a1 < 100, "must fit in 0..period_max");
        assert!(b1 < 100, "must fit in 0..period_max");
        assert_ne!(
            a1, b1,
            "different seeds should produce different offsets in this case"
        );
        // period_max <= 1 collapses to 0 — no offset to apply.
        assert_eq!(derive_phase_offset(seed_a, 0), 0);
        assert_eq!(derive_phase_offset(seed_a, 1), 0);
    }
}
