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
#[derive(Debug, Clone, Copy)]
pub struct ScanScheduler {
    pub hot: u64,
    pub warm: u64,
    pub cold: u64,
}

impl ScanScheduler {
    pub fn new(hot: u64, warm: u64, cold: u64) -> Self {
        Self {
            hot: hot.max(1),
            warm: warm.max(1),
            cold: cold.max(1),
        }
    }
    pub fn should_scan(&self, bucket: PositionBucket, block: u64) -> bool {
        let period = match bucket {
            PositionBucket::Liquidatable => self.hot,
            PositionBucket::NearLiquidation => self.warm,
            PositionBucket::Healthy => self.cold,
        };
        block % period == 0
    }
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
}
