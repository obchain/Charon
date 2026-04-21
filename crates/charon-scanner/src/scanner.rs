//! Health-factor scanner — 3-bucket classifier on top of normalized
//! [`Position`](charon_core::Position) records.
//!
//! Protocol adapters supply positions; the scanner classifies each into
//! one of three buckets based on its `health_factor`:
//!
//! * **Liquidatable** — `hf < liquidatable_threshold` (1.0 by default).
//!   Ready to be handed to the profit calculator and flash-loan router.
//! * **NearLiquidation** — `liquidatable_threshold ≤ hf < near_liq_threshold`.
//!   Pre-cached so we can fire instantly on the next adverse oracle update.
//! * **Healthy** — everything else. Tracked just enough to transition out
//!   quickly when the borrower's position deteriorates.
//!
//! Storage is a single `DashMap<Address, BucketedPosition>` — lock-free,
//! shard-partitioned, safe for the scanner task to mutate while other
//! tasks read for downstream stages.

use alloy::primitives::U256;
use charon_core::Position;
use dashmap::DashMap;

/// Which classification bucket a borrower's position currently falls into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PositionBucket {
    /// Safely over-collateralized; nothing to do.
    Healthy,
    /// Close to the liquidation boundary — watched more aggressively.
    NearLiquidation,
    /// Currently liquidatable.
    Liquidatable,
}

/// Cached per-borrower state tracked by the scanner.
#[derive(Debug, Clone)]
pub struct BucketedPosition {
    pub position: Position,
    pub bucket: PositionBucket,
}

/// Populated count summary of each bucket — emitted once per block.
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

/// 3-bucket health-factor scanner.
///
/// Thresholds are configured in float form (see [`BotConfig`]) and scaled
/// to 1e18-fixed `U256` inside [`HealthScanner::new`]. At comparison time
/// we stay entirely in integer arithmetic — no float drift per tick.
///
/// [`BotConfig`]: charon_core::config::BotConfig
pub struct HealthScanner {
    /// Positions with `health_factor < this` are liquidatable.
    liquidatable_threshold: U256,
    /// Upper bound of the near-liquidation band.
    near_liq_threshold: U256,
    /// Borrower → (latest position, current bucket).
    positions: DashMap<alloy::primitives::Address, BucketedPosition>,
}

impl HealthScanner {
    /// Build a scanner from float thresholds (e.g. `1.0`, `1.05`).
    ///
    /// Floats are scaled to 1e18-fixed U256 once, here. Values are
    /// validated: `liquidatable ≤ near_liq` must hold, otherwise the
    /// classifier would leave a gap where positions match neither bucket.
    pub fn new(liquidatable: f64, near_liq: f64) -> anyhow::Result<Self> {
        if !(liquidatable.is_finite() && near_liq.is_finite()) {
            anyhow::bail!("scanner thresholds must be finite floats");
        }
        if liquidatable < 0.0 || near_liq < 0.0 {
            anyhow::bail!("scanner thresholds must be non-negative");
        }
        if liquidatable > near_liq {
            anyhow::bail!(
                "liquidatable_threshold ({liquidatable}) must be ≤ near_liq_threshold ({near_liq})"
            );
        }
        Ok(Self {
            liquidatable_threshold: f64_to_1e18(liquidatable),
            near_liq_threshold: f64_to_1e18(near_liq),
            positions: DashMap::new(),
        })
    }

    /// Classify a single health-factor reading into a bucket.
    pub fn classify(&self, health_factor: U256) -> PositionBucket {
        if health_factor < self.liquidatable_threshold {
            PositionBucket::Liquidatable
        } else if health_factor < self.near_liq_threshold {
            PositionBucket::NearLiquidation
        } else {
            PositionBucket::Healthy
        }
    }

    /// Upsert a batch of freshly-fetched positions. Each borrower's
    /// previous bucket is overwritten with its latest reading.
    pub fn upsert(&self, positions: impl IntoIterator<Item = Position>) {
        for p in positions {
            let bucket = self.classify(p.health_factor);
            self.positions.insert(
                p.borrower,
                BucketedPosition {
                    position: p,
                    bucket,
                },
            );
        }
    }

    /// Snapshot the current bucket populations.
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

    /// Clone out all currently-liquidatable positions for downstream
    /// stages (profit calc, flash-loan router). Called once per block.
    pub fn liquidatable(&self) -> Vec<Position> {
        self.positions
            .iter()
            .filter(|e| e.value().bucket == PositionBucket::Liquidatable)
            .map(|e| e.value().position.clone())
            .collect()
    }

    /// Same, but for near-liquidation positions — these are the ones the
    /// mempool monitor / pre-computation layer will want pre-signed txs
    /// for (follow-up task).
    pub fn near_liquidation(&self) -> Vec<Position> {
        self.positions
            .iter()
            .filter(|e| e.value().bucket == PositionBucket::NearLiquidation)
            .map(|e| e.value().position.clone())
            .collect()
    }
}

/// Scale a float in `[0, ~10]` to a 1e18-fixed `U256`.
///
/// Bounded to `u128` capacity on the scaled value — with the 1.0–2.0
/// range we use in practice, this is many orders of magnitude below the
/// overflow limit. Saturating conversion keeps us safe if config ever
/// passes something absurd instead of panicking.
fn f64_to_1e18(x: f64) -> U256 {
    let scaled = x * 1e18;
    if scaled.is_nan() || scaled < 0.0 {
        U256::ZERO
    } else if scaled > u128::MAX as f64 {
        U256::from(u128::MAX)
    } else {
        U256::from(scaled as u128)
    }
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
            borrower: alloy::primitives::Address::from(bytes),
            collateral_token: address!("0000000000000000000000000000000000000001"),
            debt_token: address!("0000000000000000000000000000000000000002"),
            collateral_amount: U256::ZERO,
            debt_amount: U256::ZERO,
            health_factor: hf,
            liquidation_bonus_bps: 1000,
        }
    }

    #[test]
    fn classify_partitions_positions_into_three_buckets() {
        let s = HealthScanner::new(1.0, 1.05).unwrap();
        let e18 = one_e18();

        // 0.5e18 = liquidatable
        assert_eq!(
            s.classify(e18 / U256::from(2u64)),
            PositionBucket::Liquidatable
        );
        // 1.0e18 exactly = not liquidatable (strict `<`) — falls into near-liq
        assert_eq!(s.classify(e18), PositionBucket::NearLiquidation);
        // 1.04e18 = near-liq
        let p104 = U256::from(1_040_000_000_000_000_000u128);
        assert_eq!(s.classify(p104), PositionBucket::NearLiquidation);
        // 1.05e18 = healthy (boundary: `near_liq_threshold` is exclusive top)
        let p105 = U256::from(1_050_000_000_000_000_000u128);
        assert_eq!(s.classify(p105), PositionBucket::Healthy);
        // 2e18 = healthy
        assert_eq!(s.classify(e18 * U256::from(2u64)), PositionBucket::Healthy);
    }

    #[test]
    fn upsert_updates_buckets_and_counts() {
        let s = HealthScanner::new(1.0, 1.05).unwrap();
        let e18 = one_e18();
        s.upsert([
            mk_position(1, U256::from(0u64)), // liquidatable
            mk_position(2, U256::from(1_020_000_000_000_000_000u128)), // near-liq
            mk_position(3, e18 * U256::from(3u64)), // healthy
        ]);
        let c = s.bucket_counts();
        assert_eq!(c.liquidatable, 1);
        assert_eq!(c.near_liquidation, 1);
        assert_eq!(c.healthy, 1);
        assert_eq!(c.total(), 3);
        assert_eq!(s.liquidatable().len(), 1);
        assert_eq!(s.near_liquidation().len(), 1);
    }

    #[test]
    fn upsert_overwrites_previous_classification() {
        let s = HealthScanner::new(1.0, 1.05).unwrap();
        s.upsert([mk_position(1, U256::from(0u64))]);
        assert_eq!(s.bucket_counts().liquidatable, 1);
        // Same borrower bounces back to healthy.
        s.upsert([mk_position(1, one_e18() * U256::from(5u64))]);
        let c = s.bucket_counts();
        assert_eq!(c.liquidatable, 0);
        assert_eq!(c.healthy, 1);
    }

    #[test]
    fn rejects_inverted_thresholds() {
        assert!(HealthScanner::new(1.05, 1.0).is_err());
    }

    #[test]
    fn rejects_nan_thresholds() {
        assert!(HealthScanner::new(f64::NAN, 1.05).is_err());
    }
}
