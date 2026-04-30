//! Persistent on-disk checkpoint for [`BorrowerSet`] — issue #349.
//!
//! Without persistence, every restart re-runs the 7-day `Borrow`
//! backfill: ~22 `eth_getLogs` chunks plus retry latency on every
//! boot, plus a window during which the bot misses any new
//! liquidatable account that surfaces while the backfill catches up.
//!
//! This module persists the borrower set + per-borrower
//! `last_seen_block` to a line-delimited JSON file under
//! `CHARON_STATE_DIR` (default `./state`). On the next boot the
//! caller loads the file, restores the in-memory `BorrowerSet`, and
//! caps the backfill at `max(persisted_max + 1, head -
//! DEFAULT_BACKFILL_BLOCKS)` so a long downtime still bounds the
//! lookback to the cold-start budget.
//!
//! ### Format
//!
//! One JSON object per line: `{"addr":"0x...","block":N,"active":true}`.
//! `active` is optional and defaults to `true` when missing
//! (forward-compat for stores written before #356 landed). LDJSON is
//! deliberately chosen over a single JSON array so a corrupt tail
//! cannot poison the rest of the file — the loader skips any line
//! that fails to parse and logs at `warn`.
//!
//! ### Atomicity
//!
//! Flushes write to `<path>.tmp` and `rename()` over the destination,
//! so a crash mid-write never leaves a half-written file readable to
//! the next boot. Parent directory is created with default umask
//! permissions (the caller can apply a stricter umask before
//! invocation if needed); the issue spec asks for `0o700` and that
//! is the responsibility of the calling shell / systemd unit on
//! POSIX targets — applying it here would not portably work on
//! non-Unix runners.

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use alloy::primitives::Address;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::discovery::BorrowerSet;

/// Default flush interval for the background persistence task.
pub const DEFAULT_FLUSH_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// One persisted borrower row. `active` is forward-compat with the
/// #356 active-flag work — present in the on-disk format, defaulted
/// to `true` on read so an older checkpoint loads cleanly.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BorrowerRow {
    addr: Address,
    block: u64,
    #[serde(default = "row_active_default")]
    active: bool,
}

fn row_active_default() -> bool {
    true
}

/// Compose the canonical state path for `chain` under `state_dir`.
/// Operators set `state_dir` from `CHARON_STATE_DIR` (default
/// `./state`).
pub fn checkpoint_path(state_dir: &Path, chain: &str) -> PathBuf {
    state_dir.join(format!("borrowers.{chain}.jsonl"))
}

/// Load any existing checkpoint at `path` into `set`. Returns the
/// highest `block` observed across the file (or `0` when the file is
/// missing / empty / fully corrupt). Best-effort: a corrupt line is
/// logged at `warn` and the loader keeps reading subsequent lines.
pub fn load_into(path: &Path, set: &BorrowerSet) -> Result<u64> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            debug!(
                path = %path.display(),
                "no borrower checkpoint found — starting cold"
            );
            return Ok(0);
        }
        Err(err) => {
            return Err(err)
                .with_context(|| format!("open borrower checkpoint at {}", path.display()));
        }
    };
    let reader = BufReader::new(file);
    let mut max_block = 0u64;
    let mut accepted = 0usize;
    let mut rejected = 0usize;
    for (lineno, line) in reader.lines().enumerate() {
        let line = match line {
            Ok(l) => l,
            Err(err) => {
                warn!(
                    path = %path.display(),
                    lineno = lineno + 1,
                    error = %err,
                    "checkpoint read error — skipping line"
                );
                rejected = rejected.saturating_add(1);
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let row: BorrowerRow = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(err) => {
                warn!(
                    path = %path.display(),
                    lineno = lineno + 1,
                    error = %err,
                    "checkpoint parse error — skipping line"
                );
                rejected = rejected.saturating_add(1);
                continue;
            }
        };
        // Forward-compat: an `active=false` row from a future
        // checkpoint is still re-tracked here as active because this
        // branch does not yet model the inactive flag (lands in #356).
        // Re-loading the same address from a live `Borrow` event in
        // the post-#356 world will re-activate it anyway.
        let _ = row.active;
        set.upsert(row.addr, row.block);
        if row.block > max_block {
            max_block = row.block;
        }
        accepted = accepted.saturating_add(1);
    }
    info!(
        path = %path.display(),
        accepted,
        rejected,
        max_block,
        "borrower checkpoint loaded"
    );
    Ok(max_block)
}

/// Snapshot the current `set` and atomically replace `path` with the
/// new contents. Writes to `<path>.tmp` first, then `rename()` over
/// the destination — a crash mid-write leaves the previous file in
/// place and the loader recovers cleanly. Returns the number of rows
/// written.
pub fn save_atomic(path: &Path, set: &BorrowerSet) -> Result<usize> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent dir {}", parent.display()))?;
    }
    let tmp = path.with_extension("jsonl.tmp");
    let file = std::fs::File::create(&tmp)
        .with_context(|| format!("create tmp checkpoint {}", tmp.display()))?;
    let mut w = BufWriter::new(file);
    let rows: Vec<BorrowerRow> = set
        .entries()
        .into_iter()
        .map(|(addr, info)| BorrowerRow {
            addr,
            block: info.last_seen_block,
            active: true,
        })
        .collect();
    let count = rows.len();
    for row in &rows {
        let json = serde_json::to_string(row).context("serialize BorrowerRow")?;
        w.write_all(json.as_bytes())
            .with_context(|| format!("write tmp checkpoint {}", tmp.display()))?;
        w.write_all(b"\n")
            .with_context(|| format!("write tmp checkpoint {}", tmp.display()))?;
    }
    w.flush()
        .with_context(|| format!("flush tmp checkpoint {}", tmp.display()))?;
    drop(w);
    std::fs::rename(&tmp, path)
        .with_context(|| format!("atomic rename {} -> {}", tmp.display(), path.display()))?;
    debug!(path = %path.display(), count, "borrower checkpoint flushed");
    Ok(count)
}

/// Spawn a background task that snapshots `set` to `path` every
/// `interval` until cancelled by dropping the returned
/// [`tokio::task::JoinHandle`]. Surface metric:
/// `charon_discovery_borrowers_persisted_total{chain}` increments on
/// each successful flush.
pub fn spawn_flush_task(
    path: PathBuf,
    set: BorrowerSet,
    chain: String,
    interval: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // Skip the immediate first tick; the bot just loaded — no
        // need to rewrite the file before any new live event lands.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            match save_atomic(&path, &set) {
                Ok(count) => {
                    charon_metrics::record_discovery_borrowers_persisted(&chain, count as u64);
                }
                Err(err) => {
                    warn!(
                        chain = %chain,
                        path = %path.display(),
                        error = %format!("{err:#}"),
                        "borrower checkpoint flush failed"
                    );
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    #[test]
    fn round_trip_persists_addr_and_block() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = checkpoint_path(dir.path(), "bnb");

        let set = BorrowerSet::new();
        let a = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let b = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        set.upsert(a, 100);
        set.upsert(b, 200);

        save_atomic(&path, &set).expect("save");

        let restored = BorrowerSet::new();
        let max = load_into(&path, &restored).expect("load");
        assert_eq!(max, 200);
        assert_eq!(restored.len(), 2);
        let snap = restored.snapshot();
        assert!(snap.contains(&a));
        assert!(snap.contains(&b));
    }

    #[test]
    fn load_tolerates_corrupt_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = checkpoint_path(dir.path(), "bnb");
        let bad = format!(
            "{{\"addr\":\"{}\",\"block\":7}}\n",
            address!("dddddddddddddddddddddddddddddddddddddddd")
        ) + "this is not json\n"
            + &format!(
                "{{\"addr\":\"{}\",\"block\":42}}\n",
                address!("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee")
            );
        std::fs::write(&path, bad).expect("write");
        let set = BorrowerSet::new();
        let max = load_into(&path, &set).expect("load");
        // Two valid lines round-trip; one bad line gets logged + skipped.
        assert_eq!(max, 42);
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn missing_file_is_a_clean_cold_start() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = checkpoint_path(dir.path(), "bnb");
        let set = BorrowerSet::new();
        let max = load_into(&path, &set).expect("missing file is OK");
        assert_eq!(max, 0);
        assert!(set.is_empty());
    }

    /// Atomic-replace: writing twice must fully replace the previous
    /// file (no trailing residue from the older, longer write).
    #[test]
    fn save_atomic_replaces_previous_contents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = checkpoint_path(dir.path(), "bnb");

        let set = BorrowerSet::new();
        for i in 0..10u8 {
            set.upsert(Address::from_slice(&[i; 20]), 100);
        }
        save_atomic(&path, &set).expect("first save");
        let first_lines = std::fs::read_to_string(&path)
            .expect("read")
            .lines()
            .count();
        assert_eq!(first_lines, 10);

        // Shrink the in-memory set and re-save — the on-disk file must
        // shrink in lockstep, not retain trailing rows.
        let set2 = BorrowerSet::new();
        set2.upsert(Address::from_slice(&[0u8; 20]), 100);
        save_atomic(&path, &set2).expect("second save");
        let second_lines = std::fs::read_to_string(&path)
            .expect("read")
            .lines()
            .count();
        assert_eq!(second_lines, 1, "atomic replace must shrink the file");
    }
}
