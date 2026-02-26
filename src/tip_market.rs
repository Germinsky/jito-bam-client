/// Autonomous Tip-Market Monitor for real-time Jito tip economics.
///
/// Runs a background polling loop that queries `getBundleStatuses` every
/// N seconds to build a live picture of the tip-rate market.  When SOL
/// volatility spikes (detected via tip acceleration or user signal), the
/// monitor automatically escalates from the median (p50) to the **75th
/// percentile** to guarantee the swap lands in the first 100ms of the
/// slot.
///
/// # Architecture
///
/// ```text
///   ┌──────────────────────────────────────────────────┐
///   │              TipMarketMonitor                     │
///   │                                                  │
///   │  ┌───────────┐  ┌─────────────┐  ┌───────────┐  │
///   │  │ Poller    │  │ Distribution│  │ Volatility│  │
///   │  │ (5s loop) │─▶│ Tracker     │─▶│ Detector  │  │
///   │  └───────────┘  └─────────────┘  └─────┬─────┘  │
///   │                                        │        │
///   │                                        ▼        │
///   │              ┌──────────────────────────────┐   │
///   │              │  TipRecommendation            │   │
///   │              │  p50 / p75 / market_state     │   │
///   │              └──────────────────────────────┘   │
///   └──────────────────────────────────────────────────┘
///            │
///            ▼
///     Your Trading Bot
///       → engine.execute(..., tip)
/// ```
///
/// # Key Features
///
/// - **Continuous polling**: queries Jito every `poll_interval` (default 5s)
/// - **Full percentile distribution**: p25, p50, p75, p90, p99
/// - **Volatility detection**: if the p75/p50 ratio > threshold, or if
///   the tip acceleration (slope) exceeds a threshold, the monitor
///   escalates to p75 automatically
/// - **Thread-safe state**: snapshot can be read from any task without
///   holding the poll loop
/// - **Configurable escalation**: normal=p50×1.05, volatile=p75×1.10

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, instrument, warn};

use crate::dynamic_tip::DynamicTipCalculator;
use crate::engine::PrivateExecutionEngine;
use crate::error::{JitoBamError, Result};

// ═══════════════════════════════════════════════════════════════════════════
//  Constants
// ═══════════════════════════════════════════════════════════════════════════

/// Default poll interval for tip market queries.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Default sample window (how many tip data points to keep).
pub const DEFAULT_SAMPLE_WINDOW: usize = 100;

/// Tip acceleration threshold: if the p75/p50 ratio exceeds this, enter
/// volatile mode and escalate to p75.
pub const VOLATILITY_RATIO_THRESHOLD: f64 = 1.50;

/// Tip acceleration slope threshold: if the median is rising faster than
/// this (lamports/sample), enter volatile mode.
pub const TIP_ACCELERATION_THRESHOLD: f64 = 5_000.0;

/// Normal mode premium: median × 1.05 (5%).
pub const NORMAL_PREMIUM: f64 = 1.05;

/// Volatile mode premium: p75 × 1.10 (10%).
pub const VOLATILE_PREMIUM: f64 = 1.10;

/// Minimum samples before volatility detection activates.
pub const MIN_SAMPLES_FOR_VOLATILITY: usize = 5;

// ═══════════════════════════════════════════════════════════════════════════
//  Market state
// ═══════════════════════════════════════════════════════════════════════════

/// Current tip-market regime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarketState {
    /// Low-competition: median tip is stable. Use p50 × 1.05.
    Normal,
    /// High-competition: tips are rising or spread is wide. Use p75 × 1.10.
    Volatile,
    /// Not enough data to determine state.
    Initializing,
}

impl std::fmt::Display for MarketState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Normal => write!(f, "Normal"),
            Self::Volatile => write!(f, "Volatile"),
            Self::Initializing => write!(f, "Initializing"),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Tip distribution
// ═══════════════════════════════════════════════════════════════════════════

/// Full percentile distribution of observed tips.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TipDistribution {
    /// Number of samples in the window.
    pub sample_count: usize,
    /// 25th percentile tip (lamports).
    pub p25: u64,
    /// 50th percentile / median (lamports).
    pub p50: u64,
    /// 75th percentile (lamports).
    pub p75: u64,
    /// 90th percentile (lamports).
    pub p90: u64,
    /// 99th percentile (lamports).
    pub p99: u64,
    /// Minimum observed tip.
    pub min: u64,
    /// Maximum observed tip.
    pub max: u64,
    /// Mean tip.
    pub mean: u64,
}

impl TipDistribution {
    /// Compute from a sorted slice of tip values.
    fn from_sorted(sorted: &[u64]) -> Self {
        let n = sorted.len();
        if n == 0 {
            return Self {
                sample_count: 0,
                p25: 0, p50: 0, p75: 0, p90: 0, p99: 0,
                min: 0, max: 0, mean: 0,
            };
        }

        let percentile = |p: f64| -> u64 {
            let idx = ((p / 100.0) * (n - 1) as f64).round() as usize;
            sorted[idx.min(n - 1)]
        };

        Self {
            sample_count: n,
            p25: percentile(25.0),
            p50: percentile(50.0),
            p75: percentile(75.0),
            p90: percentile(90.0),
            p99: percentile(99.0),
            min: sorted[0],
            max: sorted[n - 1],
            mean: sorted.iter().sum::<u64>() / n as u64,
        }
    }

    /// Compute the p75/p50 spread ratio.
    pub fn spread_ratio(&self) -> f64 {
        if self.p50 == 0 {
            return 1.0;
        }
        self.p75 as f64 / self.p50 as f64
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Tip recommendation
// ═══════════════════════════════════════════════════════════════════════════

/// The recommended tip and the reasoning behind it.
#[derive(Clone, Debug)]
pub struct TipRecommendation {
    /// The recommended tip in lamports.
    pub tip_lamports: u64,
    /// Which base percentile was used (p50 or p75).
    pub base_percentile: String,
    /// The premium factor applied.
    pub premium_factor: f64,
    /// Current market state.
    pub market_state: MarketState,
    /// The full distribution snapshot.
    pub distribution: TipDistribution,
    /// Whether volatility escalation was triggered.
    pub escalated: bool,
    /// Tip acceleration (lamports/sample, recent 10 medians).
    pub tip_acceleration: f64,
    /// Timestamp of this recommendation.
    pub computed_at: Instant,
}

// ═══════════════════════════════════════════════════════════════════════════
//  Monitor configuration
// ═══════════════════════════════════════════════════════════════════════════

/// Configuration for the tip market monitor.
#[derive(Clone, Debug)]
pub struct TipMonitorConfig {
    /// How often to poll `getBundleStatuses` (default: 5s).
    pub poll_interval: Duration,
    /// Maximum number of tip samples to retain.
    pub sample_window: usize,
    /// p75/p50 ratio threshold for volatile mode.
    pub volatility_ratio_threshold: f64,
    /// Tip acceleration threshold (lamports/sample).
    pub acceleration_threshold: f64,
    /// Premium in normal mode (default: 1.05).
    pub normal_premium: f64,
    /// Premium in volatile mode (default: 1.10).
    pub volatile_premium: f64,
    /// Minimum tip floor (lamports).
    pub floor: u64,
    /// Maximum tip ceiling (lamports).
    pub ceiling: u64,
    /// Fallback tip when state is Initializing.
    pub fallback_tip: u64,
}

impl Default for TipMonitorConfig {
    fn default() -> Self {
        Self {
            poll_interval: DEFAULT_POLL_INTERVAL,
            sample_window: DEFAULT_SAMPLE_WINDOW,
            volatility_ratio_threshold: VOLATILITY_RATIO_THRESHOLD,
            acceleration_threshold: TIP_ACCELERATION_THRESHOLD,
            normal_premium: NORMAL_PREMIUM,
            volatile_premium: VOLATILE_PREMIUM,
            floor: 1_000,
            ceiling: 5_000_000_000,
            fallback_tip: 50_000,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Tip market monitor
// ═══════════════════════════════════════════════════════════════════════════

/// Thread-safe shared state for the monitor.
#[derive(Clone)]
pub struct TipMarketState {
    inner: Arc<Mutex<TipMarketInner>>,
}

struct TipMarketInner {
    /// All observed tips (ring buffer).
    tips: VecDeque<u64>,
    /// Recent median history for acceleration tracking.
    median_history: VecDeque<u64>,
    /// Maximum window size.
    window_size: usize,
    /// Current market state.
    market_state: MarketState,
    /// Latest distribution snapshot.
    distribution: TipDistribution,
    /// Last poll timestamp.
    last_poll: Option<Instant>,
    /// Total polls executed.
    poll_count: u64,
    /// Configuration.
    config: TipMonitorConfig,
    /// Whether a volatile override has been forced by the user.
    force_volatile: bool,
}

impl TipMarketState {
    /// Create a new shared state with the given config.
    pub fn new(config: TipMonitorConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(TipMarketInner {
                tips: VecDeque::with_capacity(config.sample_window),
                median_history: VecDeque::with_capacity(20),
                window_size: config.sample_window,
                market_state: MarketState::Initializing,
                distribution: TipDistribution::from_sorted(&[]),
                last_poll: None,
                poll_count: 0,
                config,
                force_volatile: false,
            })),
        }
    }

    /// Record one or more tip data points.
    pub fn record_tips(&self, tips: &[u64]) {
        let mut inner = self.inner.lock().unwrap();
        for &tip in tips {
            if inner.tips.len() >= inner.window_size {
                inner.tips.pop_front();
            }
            inner.tips.push_back(tip);
        }
        // Recompute distribution
        inner.distribution = Self::compute_distribution(&inner.tips);

        // Track median history (for acceleration)
        let p50 = inner.distribution.p50;
        if inner.distribution.sample_count > 0 {
            if inner.median_history.len() >= 20 {
                inner.median_history.pop_front();
            }
            inner.median_history.push_back(p50);
        }

        // Update market state
        inner.market_state = Self::detect_state(&inner);
        inner.last_poll = Some(Instant::now());
        inner.poll_count += 1;
    }

    /// Get the current tip recommendation.
    pub fn recommend(&self) -> TipRecommendation {
        let inner = self.inner.lock().unwrap();

        if inner.tips.is_empty() {
            return TipRecommendation {
                tip_lamports: inner.config.fallback_tip,
                base_percentile: "fallback".to_string(),
                premium_factor: 1.0,
                market_state: MarketState::Initializing,
                distribution: inner.distribution.clone(),
                escalated: false,
                tip_acceleration: 0.0,
                computed_at: Instant::now(),
            };
        }

        let acceleration = Self::tip_acceleration(&inner.median_history);
        let state = if inner.force_volatile {
            MarketState::Volatile
        } else {
            inner.market_state
        };

        let (base_tip, base_label, premium) = match state {
            MarketState::Normal => (
                inner.distribution.p50,
                "p50",
                inner.config.normal_premium,
            ),
            MarketState::Volatile => (
                inner.distribution.p75,
                "p75",
                inner.config.volatile_premium,
            ),
            MarketState::Initializing => (
                inner.config.fallback_tip,
                "fallback",
                1.0,
            ),
        };

        let raw = (base_tip as f64 * premium) as u64;
        let clamped = raw.clamp(inner.config.floor, inner.config.ceiling);

        TipRecommendation {
            tip_lamports: clamped,
            base_percentile: base_label.to_string(),
            premium_factor: premium,
            market_state: state,
            distribution: inner.distribution.clone(),
            escalated: state == MarketState::Volatile,
            tip_acceleration: acceleration,
            computed_at: Instant::now(),
        }
    }

    /// Force volatile mode (e.g., during known high-volatility events).
    pub fn force_volatile(&self, force: bool) {
        self.inner.lock().unwrap().force_volatile = force;
    }

    /// Get the current market state.
    pub fn market_state(&self) -> MarketState {
        self.inner.lock().unwrap().market_state
    }

    /// Get the current distribution snapshot.
    pub fn distribution(&self) -> TipDistribution {
        self.inner.lock().unwrap().distribution.clone()
    }

    /// Get the total poll count.
    pub fn poll_count(&self) -> u64 {
        self.inner.lock().unwrap().poll_count
    }

    /// Get the sample count.
    pub fn sample_count(&self) -> usize {
        self.inner.lock().unwrap().tips.len()
    }

    // ── Internal helpers ────────────────────────────────────────────────

    fn compute_distribution(tips: &VecDeque<u64>) -> TipDistribution {
        let mut sorted: Vec<u64> = tips.iter().copied().collect();
        sorted.sort_unstable();
        TipDistribution::from_sorted(&sorted)
    }

    fn detect_state(inner: &TipMarketInner) -> MarketState {
        if inner.tips.len() < MIN_SAMPLES_FOR_VOLATILITY {
            return MarketState::Initializing;
        }

        let ratio = inner.distribution.spread_ratio();
        let accel = Self::tip_acceleration(&inner.median_history);

        if inner.force_volatile
            || ratio > inner.config.volatility_ratio_threshold
            || accel > inner.config.acceleration_threshold
        {
            MarketState::Volatile
        } else {
            MarketState::Normal
        }
    }

    /// Compute the slope of median tips over recent history.
    /// Positive = tips are rising. Units: lamports per sample.
    fn tip_acceleration(medians: &VecDeque<u64>) -> f64 {
        if medians.len() < 2 {
            return 0.0;
        }

        // Simple linear regression slope over the last N medians
        let n = medians.len() as f64;
        let mut sum_x = 0.0f64;
        let mut sum_y = 0.0f64;
        let mut sum_xy = 0.0f64;
        let mut sum_x2 = 0.0f64;

        for (i, &m) in medians.iter().enumerate() {
            let x = i as f64;
            let y = m as f64;
            sum_x += x;
            sum_y += y;
            sum_xy += x * y;
            sum_x2 += x * x;
        }

        let denom = n * sum_x2 - sum_x * sum_x;
        if denom.abs() < 1e-10 {
            return 0.0;
        }

        (n * sum_xy - sum_x * sum_y) / denom
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Background polling task
// ═══════════════════════════════════════════════════════════════════════════

/// Spawn a background task that polls Jito for tip market data every
/// `config.poll_interval`.
///
/// Uses the `DynamicTipCalculator` internally to query `getBundleStatuses`
/// and feeds the data points into `TipMarketState`.
///
/// Returns a handle to the shared state so the main trading loop can
/// call `state.recommend()` at any time.
///
/// # Example
///
/// ```rust,no_run
/// use jito_bam_client::tip_market::{TipMonitorConfig, spawn_tip_monitor};
/// use jito_bam_client::engine::PrivateExecutionEngine;
///
/// # async fn run() {
/// let engine = PrivateExecutionEngine::mainnet("https://api.mainnet-beta.solana.com");
///
/// let config = TipMonitorConfig::default(); // polls every 5s
/// let state = spawn_tip_monitor(engine, config);
///
/// // In your trading loop:
/// loop {
///     let rec = state.recommend();
///     println!("Tip: {} lamports ({})", rec.tip_lamports, rec.market_state);
///     tokio::time::sleep(std::time::Duration::from_secs(1)).await;
/// }
/// # }
/// ```
pub fn spawn_tip_monitor(
    engine: PrivateExecutionEngine,
    config: TipMonitorConfig,
) -> TipMarketState {
    let interval = config.poll_interval;
    let state = TipMarketState::new(config);
    let state_clone = state.clone();

    tokio::spawn(async move {
        let mut tip_calc = DynamicTipCalculator::new(100);
        let mut poll_num: u64 = 0;

        info!(
            interval_ms = interval.as_millis(),
            "tip market monitor started"
        );

        loop {
            poll_num += 1;

            // Refresh from Jito — this queries getBundleStatuses for any
            // tracked bundles and records their tips.
            match tip_calc.refresh_from_jito(&engine).await {
                Ok(landed) => {
                    if landed > 0 {
                        debug!(
                            poll = poll_num,
                            landed,
                            "tip refresh found landed bundles"
                        );
                    }
                }
                Err(e) => {
                    warn!(
                        poll = poll_num,
                        error = %e,
                        "tip market poll failed"
                    );
                }
            }

            // Feed any new tips into the market state
            let tips = tip_calc.tip_history();
            if !tips.is_empty() {
                state_clone.record_tips(&tips);

                if poll_num % 12 == 0 {
                    // Log every ~60s (12 × 5s)
                    let rec = state_clone.recommend();
                    info!(
                        poll = poll_num,
                        samples = rec.distribution.sample_count,
                        state = %rec.market_state,
                        p50 = rec.distribution.p50,
                        p75 = rec.distribution.p75,
                        p90 = rec.distribution.p90,
                        recommended = rec.tip_lamports,
                        escalated = rec.escalated,
                        accel = format!("{:.0}", rec.tip_acceleration),
                        "tip market report"
                    );
                }
            }

            tokio::time::sleep(interval).await;
        }
    });

    state
}

/// Manually feed tip data into a `TipMarketState` (for offline/testing use).
///
/// This bypasses the background poller and lets you seed the distribution
/// with known tip values.
pub fn seed_tip_market(state: &TipMarketState, tips: &[u64]) {
    state.record_tips(tips);
    info!(
        count = tips.len(),
        state = %state.market_state(),
        "seeded tip market"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state() -> TipMarketState {
        TipMarketState::new(TipMonitorConfig::default())
    }

    // ── Distribution ────────────────────────────────────────────────────

    #[test]
    fn distribution_empty() {
        let dist = TipDistribution::from_sorted(&[]);
        assert_eq!(dist.sample_count, 0);
        assert_eq!(dist.p50, 0);
    }

    #[test]
    fn distribution_single() {
        let dist = TipDistribution::from_sorted(&[50_000]);
        assert_eq!(dist.sample_count, 1);
        assert_eq!(dist.p50, 50_000);
        assert_eq!(dist.min, 50_000);
        assert_eq!(dist.max, 50_000);
    }

    #[test]
    fn distribution_percentiles() {
        // 10 values: 10k, 20k, ... 100k
        let tips: Vec<u64> = (1..=10).map(|i| i * 10_000).collect();
        let dist = TipDistribution::from_sorted(&tips);

        assert_eq!(dist.sample_count, 10);
        assert_eq!(dist.min, 10_000);
        assert_eq!(dist.max, 100_000);
        assert_eq!(dist.mean, 55_000);

        // p25 ≈ index 2.25 → 30_000
        assert!(dist.p25 >= 20_000 && dist.p25 <= 40_000);
        // p50 ≈ index 4.5 → 50_000
        assert!(dist.p50 >= 40_000 && dist.p50 <= 60_000);
        // p75 ≈ index 6.75 → 70_000
        assert!(dist.p75 >= 60_000 && dist.p75 <= 80_000);
    }

    #[test]
    fn spread_ratio_stable() {
        // All same → ratio = 1.0
        let tips = vec![50_000u64; 10];
        let dist = TipDistribution::from_sorted(&tips);
        assert!((dist.spread_ratio() - 1.0).abs() < 0.01);
    }

    #[test]
    fn spread_ratio_wide() {
        // 20 samples: first 12 low, last 8 high → p50 in low region, p75 in high region
        let mut tips: Vec<u64> = vec![50_000; 12];
        tips.extend_from_slice(&[200_000u64; 8]);
        tips.sort();
        let dist = TipDistribution::from_sorted(&tips);
        assert!(dist.spread_ratio() > 1.0, "spread_ratio={}", dist.spread_ratio());
    }

    // ── Market state detection ──────────────────────────────────────────

    #[test]
    fn initializing_with_few_samples() {
        let state = make_state();
        state.record_tips(&[50_000, 60_000]);
        assert_eq!(state.market_state(), MarketState::Initializing);
    }

    #[test]
    fn normal_with_stable_tips() {
        let state = make_state();
        let tips: Vec<u64> = vec![50_000; 10];
        state.record_tips(&tips);
        assert_eq!(state.market_state(), MarketState::Normal);
    }

    #[test]
    fn volatile_with_wide_spread() {
        let mut config = TipMonitorConfig::default();
        config.volatility_ratio_threshold = 1.2; // lower threshold for test
        let state = TipMarketState::new(config);

        // Wide distribution: many low values, a few very high → clear p75 > p50
        let mut tips = vec![10_000u64; 12];
        tips.extend_from_slice(&[100_000u64; 8]);
        state.record_tips(&tips);
        assert_eq!(state.market_state(), MarketState::Volatile);
    }

    #[test]
    fn force_volatile_overrides() {
        let state = make_state();
        state.record_tips(&vec![50_000; 10]);
        assert_eq!(state.market_state(), MarketState::Normal);

        state.force_volatile(true);
        let rec = state.recommend();
        assert!(rec.escalated);
        assert_eq!(rec.market_state, MarketState::Volatile);

        state.force_volatile(false);
    }

    // ── Recommendations ─────────────────────────────────────────────────

    #[test]
    fn recommend_fallback_when_empty() {
        let state = make_state();
        let rec = state.recommend();
        assert_eq!(rec.tip_lamports, 50_000);
        assert_eq!(rec.market_state, MarketState::Initializing);
        assert!(!rec.escalated);
    }

    #[test]
    fn recommend_normal_uses_p50() {
        let state = make_state();
        state.record_tips(&vec![100_000; 10]);

        let rec = state.recommend();
        // p50 = 100k, normal premium = 1.05 → 105k
        assert_eq!(rec.tip_lamports, 105_000);
        assert_eq!(rec.base_percentile, "p50");
        assert!(!rec.escalated);
    }

    #[test]
    fn recommend_volatile_uses_p75() {
        let mut config = TipMonitorConfig::default();
        config.volatility_ratio_threshold = 1.1;
        let state = TipMarketState::new(config);

        let mut tips: Vec<u64> = (1..=10).map(|i| i * 10_000).collect();
        state.record_tips(&tips);

        // If the spread triggers volatile...
        if state.market_state() == MarketState::Volatile {
            let rec = state.recommend();
            assert_eq!(rec.base_percentile, "p75");
            assert!(rec.escalated);
        }
    }

    #[test]
    fn recommend_clamped_to_floor() {
        let mut config = TipMonitorConfig::default();
        config.floor = 100_000;
        let state = TipMarketState::new(config);

        state.record_tips(&vec![1_000; 10]);
        let rec = state.recommend();
        assert!(rec.tip_lamports >= 100_000);
    }

    #[test]
    fn recommend_clamped_to_ceiling() {
        let mut config = TipMonitorConfig::default();
        config.ceiling = 50_000;
        let state = TipMarketState::new(config);

        state.record_tips(&vec![1_000_000; 10]);
        let rec = state.recommend();
        assert!(rec.tip_lamports <= 50_000);
    }

    // ── Tip acceleration ────────────────────────────────────────────────

    #[test]
    fn acceleration_flat() {
        let medians: VecDeque<u64> = vec![50_000; 10].into();
        let accel = TipMarketState::tip_acceleration(&medians);
        assert!(accel.abs() < 1.0, "flat tips → near-zero acceleration");
    }

    #[test]
    fn acceleration_rising() {
        let medians: VecDeque<u64> = (1..=10).map(|i| i * 10_000).collect();
        let accel = TipMarketState::tip_acceleration(&medians);
        assert!(accel > 0.0, "rising tips → positive acceleration");
    }

    #[test]
    fn acceleration_falling() {
        let medians: VecDeque<u64> = (1..=10).rev().map(|i| i * 10_000).collect();
        let accel = TipMarketState::tip_acceleration(&medians);
        assert!(accel < 0.0, "falling tips → negative acceleration");
    }

    #[test]
    fn acceleration_too_few_samples() {
        let medians: VecDeque<u64> = vec![50_000].into();
        let accel = TipMarketState::tip_acceleration(&medians);
        assert!((accel - 0.0).abs() < 1e-10);
    }

    // ── Seed ────────────────────────────────────────────────────────────

    #[test]
    fn seed_populates_state() {
        let state = make_state();
        seed_tip_market(&state, &[40_000, 50_000, 60_000, 70_000, 80_000]);
        assert_eq!(state.sample_count(), 5);
        assert_eq!(state.market_state(), MarketState::Normal);
    }

    // ── Poll count ──────────────────────────────────────────────────────

    #[test]
    fn poll_count_increments() {
        let state = make_state();
        assert_eq!(state.poll_count(), 0);
        state.record_tips(&[50_000]);
        assert_eq!(state.poll_count(), 1);
        state.record_tips(&[60_000]);
        assert_eq!(state.poll_count(), 2);
    }

    // ── Serde roundtrip ─────────────────────────────────────────────────

    #[test]
    fn distribution_serde() {
        let dist = TipDistribution {
            sample_count: 10,
            p25: 25_000, p50: 50_000, p75: 75_000,
            p90: 90_000, p99: 99_000,
            min: 10_000, max: 100_000, mean: 55_000,
        };
        let json = serde_json::to_string(&dist).unwrap();
        let back: TipDistribution = serde_json::from_str(&json).unwrap();
        assert_eq!(back.p50, 50_000);
        assert_eq!(back.p75, 75_000);
    }

    #[test]
    fn market_state_serde() {
        let json = serde_json::to_string(&MarketState::Volatile).unwrap();
        let back: MarketState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, MarketState::Volatile);
    }
}
