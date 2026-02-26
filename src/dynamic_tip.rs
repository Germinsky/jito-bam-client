/// Dynamic tip calculation based on Jito bundle-status history.
///
/// Instead of using a static tip, this module queries the Jito block
/// engine's `getBundleStatuses` and tracks the landing tips of recent
/// successful bundles.  It then computes a competitive tip that is
/// **5% above the median** of the last N successful BAM blocks, ensuring
/// top-of-block inclusion while avoiding overpayment.
///
/// # Algorithm
///
/// ```text
///  1.  Maintain a ring buffer of recent landed-bundle tips.
///  2.  On each refresh, query Jito for any tracked bundle UUIDs that
///      have finalized, and record their tip amounts.
///  3.  Compute the **median** tip from the window.
///  4.  Return  median × 1.05  (5% edge), clamped to [floor, ceiling].
/// ```
///
/// # Usage
///
/// ```rust,no_run
/// use jito_bam_client::dynamic_tip::DynamicTipCalculator;
///
/// let mut calc = DynamicTipCalculator::new(10);
///
/// // After each successful bundle, record the tip you paid:
/// calc.record_tip(50_000);
/// calc.record_tip(45_000);
/// calc.record_tip(60_000);
///
/// // Get the next competitive tip:
/// let tip = calc.recommended_tip();
/// println!("Next tip: {} lamports", tip);
/// ```
///
/// For a fully automated version that queries Jito's `getBundleStatuses`:
///
/// ```rust,no_run
/// # use jito_bam_client::dynamic_tip::DynamicTipCalculator;
/// # use jito_bam_client::engine::PrivateExecutionEngine;
/// # async fn run(engine: &PrivateExecutionEngine) {
/// let mut calc = DynamicTipCalculator::new(10);
///
/// // Track a bundle after submission:
/// calc.track_bundle("uuid-123", 50_000);
///
/// // Later, refresh from Jito to see which bundles landed:
/// calc.refresh_from_jito(engine).await.ok();
///
/// let tip = calc.recommended_tip();
/// # }
/// ```
use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, instrument, warn};

use crate::error::{JitoBamError, Result};

// ─── Constants ──────────────────────────────────────────────────────────────

/// Default window size — track the last 10 successful bundles.
pub const DEFAULT_WINDOW_SIZE: usize = 10;

/// Premium above the median (5% = 1.05×).
pub const MEDIAN_PREMIUM_FACTOR: f64 = 1.05;

/// Absolute floor — never tip below 1 000 lamports (0.000001 SOL).
pub const TIP_FLOOR_LAMPORTS: u64 = 1_000;

/// Absolute ceiling — 5 SOL max tip as a safety rail.
pub const TIP_CEILING_LAMPORTS: u64 = 5_000_000_000;

/// Default fallback tip when there's no history yet.
pub const DEFAULT_FALLBACK_TIP: u64 = 50_000;

// ─── Tracked bundle ─────────────────────────────────────────────────────────

/// A bundle that we're tracking for tip calibration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrackedBundle {
    /// The bundle UUID returned by Jito.
    pub uuid: String,
    /// The tip (in lamports) that was included in this bundle.
    pub tip_lamports: u64,
    /// Whether we've confirmed this bundle landed on-chain.
    pub landed: bool,
    /// The slot it landed in (if known).
    pub landed_slot: Option<u64>,
}

// ─── Tip history snapshot ───────────────────────────────────────────────────

/// A snapshot of the tip calculation state for inspection / logging.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TipSnapshot {
    /// Number of data points in the window.
    pub sample_count: usize,
    /// The raw median tip from the window.
    pub median_tip: u64,
    /// The recommended tip (median × premium, clamped).
    pub recommended_tip: u64,
    /// Min tip in the window.
    pub min_tip: u64,
    /// Max tip in the window.
    pub max_tip: u64,
    /// Mean tip in the window.
    pub mean_tip: u64,
    /// The premium factor applied.
    pub premium_factor: f64,
}

// ─── Dynamic tip calculator ─────────────────────────────────────────────────

/// Calculates competitive Jito tips based on recent landing history.
///
/// Maintains a sliding window of tip amounts from successfully landed
/// bundles and recommends a tip that's 5% above the median.
pub struct DynamicTipCalculator {
    /// Ring buffer of landed-bundle tips (most recent at back).
    tip_window: VecDeque<u64>,
    /// Maximum window size.
    window_size: usize,
    /// Bundles we're tracking but haven't confirmed yet.
    pending_bundles: Vec<TrackedBundle>,
    /// Fallback tip when no history is available.
    fallback_tip: u64,
    /// Premium multiplier above median (default: 1.05 = 5%).
    premium_factor: f64,
    /// Minimum tip floor.
    floor: u64,
    /// Maximum tip ceiling.
    ceiling: u64,
}

impl DynamicTipCalculator {
    // ── Constructors ────────────────────────────────────────────────────

    /// Create a calculator with the given window size.
    ///
    /// ```rust
    /// use jito_bam_client::dynamic_tip::DynamicTipCalculator;
    /// let calc = DynamicTipCalculator::new(10);
    /// assert_eq!(calc.window_size(), 10);
    /// ```
    pub fn new(window_size: usize) -> Self {
        Self {
            tip_window: VecDeque::with_capacity(window_size),
            window_size: window_size.max(1),
            pending_bundles: Vec::new(),
            fallback_tip: DEFAULT_FALLBACK_TIP,
            premium_factor: MEDIAN_PREMIUM_FACTOR,
            floor: TIP_FLOOR_LAMPORTS,
            ceiling: TIP_CEILING_LAMPORTS,
        }
    }

    /// Create with custom parameters.
    pub fn custom(
        window_size: usize,
        fallback_tip: u64,
        premium_factor: f64,
        floor: u64,
        ceiling: u64,
    ) -> Self {
        Self {
            tip_window: VecDeque::with_capacity(window_size),
            window_size: window_size.max(1),
            pending_bundles: Vec::new(),
            fallback_tip,
            premium_factor,
            floor,
            ceiling,
        }
    }

    // ── Recording tips ──────────────────────────────────────────────────

    /// Record a tip from a successfully landed bundle.
    ///
    /// If the window is full, the oldest entry is evicted.
    pub fn record_tip(&mut self, tip_lamports: u64) {
        if self.tip_window.len() >= self.window_size {
            self.tip_window.pop_front();
        }
        self.tip_window.push_back(tip_lamports);
        debug!(tip = tip_lamports, window = self.tip_window.len(), "recorded tip");
    }

    /// Track a bundle for later confirmation via Jito status queries.
    ///
    /// Call this after `send_bundle()` succeeds so we can later check
    /// if the bundle actually landed and include its tip in our window.
    pub fn track_bundle(&mut self, uuid: &str, tip_lamports: u64) {
        self.pending_bundles.push(TrackedBundle {
            uuid: uuid.to_string(),
            tip_lamports,
            landed: false,
            landed_slot: None,
        });
        debug!(uuid, tip = tip_lamports, "tracking bundle for tip calibration");
    }

    // ── Refresh from Jito ───────────────────────────────────────────────

    /// Query Jito's `getBundleStatuses` for all tracked bundles and
    /// move landed ones into the tip window.
    ///
    /// This is the core integration with `jito-sdk-rust`:
    /// - Collects all pending bundle UUIDs
    /// - Calls `getBundleStatuses` in a single batch
    /// - Parses `confirmation_status` == `"finalized"` | `"confirmed"`
    /// - Records the tip for each landed bundle
    /// - Removes landed and stale bundles from the pending list
    #[instrument(skip_all)]
    pub async fn refresh_from_jito(
        &mut self,
        engine: &crate::engine::PrivateExecutionEngine,
    ) -> Result<usize> {
        if self.pending_bundles.is_empty() {
            return Ok(0);
        }

        let uuids: Vec<String> = self
            .pending_bundles
            .iter()
            .filter(|b| !b.landed)
            .map(|b| b.uuid.clone())
            .collect();

        if uuids.is_empty() {
            return Ok(0);
        }

        info!(pending = uuids.len(), "checking bundle statuses for tip calibration");

        let response = engine
            .jito_sdk()
            .get_bundle_statuses(uuids.clone())
            .await
            .map_err(|e| JitoBamError::RpcError(format!("getBundleStatuses: {e}")))?;

        let mut landed_count = 0;
        let mut landed_tips: Vec<u64> = Vec::new();

        // Parse response: { "result": { "value": [ { "bundle_id": "...", "confirmation_status": "..." }, ... ] } }
        if let Some(entries) = response
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_array())
        {
            for entry in entries {
                let bundle_id = entry
                    .get("bundle_id")
                    .and_then(|b| b.as_str())
                    .unwrap_or("");
                let status = entry
                    .get("confirmation_status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("");
                let slot = entry.get("slot").and_then(|s| s.as_u64());

                if status == "finalized" || status == "confirmed" {
                    // Find the matching pending bundle and mark it landed.
                    // We collect tip data first to avoid double-mutable-borrow.
                    if let Some(bundle) = self
                        .pending_bundles
                        .iter_mut()
                        .find(|b| b.uuid == bundle_id && !b.landed)
                    {
                        bundle.landed = true;
                        bundle.landed_slot = slot;
                        let tip = bundle.tip_lamports;
                        landed_tips.push(tip);
                        landed_count += 1;

                        info!(
                            uuid = bundle_id,
                            tip,
                            slot = ?slot,
                            "bundle landed — tip recorded"
                        );
                    }
                }
            }
        }

        // Record all landed tips (outside the pending_bundles borrow)
        for tip in &landed_tips {
            self.record_tip(*tip);
        }

        // Prune landed and stale bundles (keep pending list lean)
        self.pending_bundles.retain(|b| !b.landed);

        // Also prune if the pending list grows too large (stale bundles)
        const MAX_PENDING: usize = 100;
        if self.pending_bundles.len() > MAX_PENDING {
            let to_remove = self.pending_bundles.len() - MAX_PENDING;
            self.pending_bundles.drain(0..to_remove);
            warn!(removed = to_remove, "pruned stale pending bundles");
        }

        info!(
            landed = landed_count,
            window = self.tip_window.len(),
            remaining_pending = self.pending_bundles.len(),
            "tip calibration refresh complete"
        );

        Ok(landed_count)
    }

    // ── Tip calculation ─────────────────────────────────────────────────

    /// Return the recommended tip: median of the window × premium,
    /// clamped to [floor, ceiling].
    ///
    /// If the window is empty, returns the fallback tip.
    pub fn recommended_tip(&self) -> u64 {
        if self.tip_window.is_empty() {
            return self.fallback_tip;
        }

        let median = self.median();
        let raw = (median as f64 * self.premium_factor) as u64;
        raw.clamp(self.floor, self.ceiling)
    }

    /// Compute the median tip from the current window.
    ///
    /// Returns 0 if the window is empty.
    pub fn median(&self) -> u64 {
        if self.tip_window.is_empty() {
            return 0;
        }

        let mut sorted: Vec<u64> = self.tip_window.iter().copied().collect();
        sorted.sort_unstable();

        let len = sorted.len();
        if len % 2 == 0 {
            // Average of the two middle elements
            (sorted[len / 2 - 1] + sorted[len / 2]) / 2
        } else {
            sorted[len / 2]
        }
    }

    /// Return a full snapshot of the current tip calculation state.
    pub fn snapshot(&self) -> TipSnapshot {
        let sample_count = self.tip_window.len();

        if sample_count == 0 {
            return TipSnapshot {
                sample_count: 0,
                median_tip: 0,
                recommended_tip: self.fallback_tip,
                min_tip: 0,
                max_tip: 0,
                mean_tip: 0,
                premium_factor: self.premium_factor,
            };
        }

        let median_tip = self.median();
        let recommended_tip = self.recommended_tip();
        let min_tip = self.tip_window.iter().copied().min().unwrap_or(0);
        let max_tip = self.tip_window.iter().copied().max().unwrap_or(0);
        let mean_tip = self.tip_window.iter().sum::<u64>() / sample_count as u64;

        TipSnapshot {
            sample_count,
            median_tip,
            recommended_tip,
            min_tip,
            max_tip,
            mean_tip,
            premium_factor: self.premium_factor,
        }
    }

    // ── Accessors ───────────────────────────────────────────────────────

    pub fn window_size(&self) -> usize {
        self.window_size
    }

    pub fn current_window_len(&self) -> usize {
        self.tip_window.len()
    }

    pub fn pending_count(&self) -> usize {
        self.pending_bundles.len()
    }

    pub fn fallback_tip(&self) -> u64 {
        self.fallback_tip
    }

    pub fn set_fallback_tip(&mut self, tip: u64) {
        self.fallback_tip = tip;
    }

    pub fn set_premium_factor(&mut self, factor: f64) {
        self.premium_factor = factor;
    }

    pub fn set_floor(&mut self, floor: u64) {
        self.floor = floor;
    }

    pub fn set_ceiling(&mut self, ceiling: u64) {
        self.ceiling = ceiling;
    }

    /// Get a copy of all tips in the window (oldest first).
    pub fn tip_history(&self) -> Vec<u64> {
        self.tip_window.iter().copied().collect()
    }

    /// Get all tracked (pending) bundles.
    pub fn pending_bundles(&self) -> &[TrackedBundle] {
        &self.pending_bundles
    }

    /// Clear the tip window and pending list.
    pub fn reset(&mut self) {
        self.tip_window.clear();
        self.pending_bundles.clear();
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_window_returns_fallback() {
        let calc = DynamicTipCalculator::new(10);
        assert_eq!(calc.recommended_tip(), DEFAULT_FALLBACK_TIP);
        assert_eq!(calc.median(), 0);
    }

    #[test]
    fn single_entry_median() {
        let mut calc = DynamicTipCalculator::new(10);
        calc.record_tip(50_000);
        assert_eq!(calc.median(), 50_000);
    }

    #[test]
    fn odd_window_median() {
        let mut calc = DynamicTipCalculator::new(10);
        calc.record_tip(30_000);
        calc.record_tip(50_000);
        calc.record_tip(70_000);
        assert_eq!(calc.median(), 50_000);
    }

    #[test]
    fn even_window_median() {
        let mut calc = DynamicTipCalculator::new(10);
        calc.record_tip(30_000);
        calc.record_tip(50_000);
        calc.record_tip(70_000);
        calc.record_tip(90_000);
        // median of [30k, 50k, 70k, 90k] = (50k + 70k) / 2 = 60k
        assert_eq!(calc.median(), 60_000);
    }

    #[test]
    fn unsorted_input_still_correct_median() {
        let mut calc = DynamicTipCalculator::new(10);
        calc.record_tip(90_000);
        calc.record_tip(30_000);
        calc.record_tip(70_000);
        calc.record_tip(50_000);
        calc.record_tip(10_000);
        // sorted: [10k, 30k, 50k, 70k, 90k] → median = 50k
        assert_eq!(calc.median(), 50_000);
    }

    #[test]
    fn premium_applied_correctly() {
        let mut calc = DynamicTipCalculator::new(10);
        calc.record_tip(100_000);
        // recommended = 100_000 * 1.05 = 105_000
        assert_eq!(calc.recommended_tip(), 105_000);
    }

    #[test]
    fn custom_premium_factor() {
        let mut calc = DynamicTipCalculator::custom(10, 50_000, 1.10, 1_000, 5_000_000_000);
        calc.record_tip(100_000);
        // 100_000 * 1.10 = 110_000
        assert_eq!(calc.recommended_tip(), 110_000);
    }

    #[test]
    fn tip_clamped_to_floor() {
        let mut calc = DynamicTipCalculator::new(10);
        calc.set_floor(100_000);
        calc.record_tip(1_000); // median = 1000, × 1.05 = 1050, but floor = 100k
        assert_eq!(calc.recommended_tip(), 100_000);
    }

    #[test]
    fn tip_clamped_to_ceiling() {
        let mut calc = DynamicTipCalculator::new(10);
        calc.set_ceiling(50_000);
        calc.record_tip(100_000); // median = 100k, × 1.05 = 105k, but ceiling = 50k
        assert_eq!(calc.recommended_tip(), 50_000);
    }

    #[test]
    fn window_eviction() {
        let mut calc = DynamicTipCalculator::new(3);
        calc.record_tip(10_000);
        calc.record_tip(20_000);
        calc.record_tip(30_000);
        assert_eq!(calc.current_window_len(), 3);

        // Adding a 4th evicts the oldest (10k)
        calc.record_tip(40_000);
        assert_eq!(calc.current_window_len(), 3);
        assert_eq!(calc.tip_history(), vec![20_000, 30_000, 40_000]);
    }

    #[test]
    fn track_bundle_adds_to_pending() {
        let mut calc = DynamicTipCalculator::new(10);
        calc.track_bundle("abc-123", 55_000);
        assert_eq!(calc.pending_count(), 1);
        assert_eq!(calc.pending_bundles()[0].uuid, "abc-123");
        assert_eq!(calc.pending_bundles()[0].tip_lamports, 55_000);
        assert!(!calc.pending_bundles()[0].landed);
    }

    #[test]
    fn snapshot_empty() {
        let calc = DynamicTipCalculator::new(10);
        let snap = calc.snapshot();
        assert_eq!(snap.sample_count, 0);
        assert_eq!(snap.recommended_tip, DEFAULT_FALLBACK_TIP);
        assert_eq!(snap.median_tip, 0);
    }

    #[test]
    fn snapshot_with_data() {
        let mut calc = DynamicTipCalculator::new(10);
        calc.record_tip(40_000);
        calc.record_tip(60_000);
        calc.record_tip(80_000);

        let snap = calc.snapshot();
        assert_eq!(snap.sample_count, 3);
        assert_eq!(snap.median_tip, 60_000);
        assert_eq!(snap.min_tip, 40_000);
        assert_eq!(snap.max_tip, 80_000);
        assert_eq!(snap.mean_tip, 60_000);
        assert_eq!(snap.recommended_tip, 63_000); // 60k * 1.05
    }

    #[test]
    fn reset_clears_everything() {
        let mut calc = DynamicTipCalculator::new(10);
        calc.record_tip(50_000);
        calc.track_bundle("abc", 50_000);
        calc.reset();
        assert_eq!(calc.current_window_len(), 0);
        assert_eq!(calc.pending_count(), 0);
    }

    #[test]
    fn window_size_minimum_one() {
        let calc = DynamicTipCalculator::new(0);
        assert_eq!(calc.window_size(), 1);
    }

    #[test]
    fn recommended_tip_with_realistic_distribution() {
        // Simulate 10 bundles with realistic tip spread
        let mut calc = DynamicTipCalculator::new(10);
        let tips = [45_000, 48_000, 50_000, 50_000, 52_000,
                     55_000, 58_000, 60_000, 65_000, 70_000];
        for tip in tips {
            calc.record_tip(tip);
        }
        // sorted = same. median of 10 = avg(52k, 55k) = 53_500
        // recommended = 53_500 * 1.05 = 56_175
        assert_eq!(calc.median(), 53_500);
        assert_eq!(calc.recommended_tip(), 56_175);
    }

    #[test]
    fn all_same_tips() {
        let mut calc = DynamicTipCalculator::new(5);
        for _ in 0..5 {
            calc.record_tip(50_000);
        }
        assert_eq!(calc.median(), 50_000);
        assert_eq!(calc.recommended_tip(), 52_500); // 50k * 1.05
    }

    #[test]
    fn set_fallback_tip_works() {
        let mut calc = DynamicTipCalculator::new(10);
        calc.set_fallback_tip(100_000);
        assert_eq!(calc.recommended_tip(), 100_000);
    }

    #[test]
    fn snapshot_serde_roundtrip() {
        let snap = TipSnapshot {
            sample_count: 5,
            median_tip: 50_000,
            recommended_tip: 52_500,
            min_tip: 40_000,
            max_tip: 70_000,
            mean_tip: 53_000,
            premium_factor: 1.05,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: TipSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.median_tip, 50_000);
        assert_eq!(back.recommended_tip, 52_500);
    }

    #[test]
    fn tracked_bundle_serde_roundtrip() {
        let tb = TrackedBundle {
            uuid: "test-uuid".into(),
            tip_lamports: 55_000,
            landed: true,
            landed_slot: Some(12345),
        };
        let json = serde_json::to_string(&tb).unwrap();
        let back: TrackedBundle = serde_json::from_str(&json).unwrap();
        assert_eq!(back.uuid, "test-uuid");
        assert_eq!(back.landed_slot, Some(12345));
    }
}
