/// ACE (Atomic Compute Engine) Plugin framework for Jito BAM.
///
/// ACE plugins are conditional-execution logic modules that run against
/// real-time data feeds **inside the BAM TEE**.  When a trigger condition
/// is met, the plugin's pre-signed transaction is placed at the **top of
/// the next BAM-assembled block** — giving you first-mover advantage
/// without exposing your intent to the public mempool.
///
/// # Architecture
///
/// ```text
/// ┌────────────────────────────────────────────────────────────┐
/// │                  Your Bot Process                          │
/// │                                                            │
/// │  ┌──────────────┐   ┌─────────────────┐   ┌────────────┐  │
/// │  │ PythMonitor  │──▶│ TriggerEngine   │──▶│ AcePlugin  │  │
/// │  │ (price feed) │   │ (evaluates      │   │ (submits   │  │
/// │  │              │   │  conditions)     │   │  to BAM)   │  │
/// │  └──────────────┘   └─────────────────┘   └─────┬──────┘  │
/// │                                                  │         │
/// └──────────────────────────────────────────────────┼─────────┘
///                                                    │ gRPC/TLS
///                                                    ▼
///                                           ┌─────────────────┐
///                                           │  BAM TEE Node   │
///                                           │  ┌───────────┐  │
///                                           │  │ ACE Eval   │  │
///                                           │  │ (re-checks │  │
///                                           │  │  trigger)  │  │
///                                           │  └─────┬─────┘  │
///                                           │        │ TOP    │
///                                           │        ▼        │
///                                           │  ┌───────────┐  │
///                                           │  │ Block Asm  │  │
///                                           │  │ (position  │  │
///                                           │  │  = 0)      │  │
///                                           │  └───────────┘  │
///                                           └─────────────────┘
/// ```
///
/// # Trigger model
///
/// A [`TriggerCondition`] defines **when** to fire.  Multiple conditions
/// can be combined with AND / OR logic via [`CompositeTrigger`].
///
/// The plugin pre-signs a transaction bundle and registers it with the
/// BAM node together with the trigger condition.  The BAM TEE
/// continuously evaluates the condition against live oracle data; when
/// it fires, the bundle is atomically placed at position 0.
///
/// # Example: SOL price momentum trigger
///
/// ```rust,no_run
/// # use jito_bam_client::ace::{
/// #     AcePlugin, AcePluginConfig, TriggerCondition,
/// #     PriceMoveTrigger, OrderPlacement,
/// # };
/// # async fn demo() {
/// let trigger = TriggerCondition::PriceMove(PriceMoveTrigger {
///     feed_id: "SOL/USD".into(),
///     threshold_bps: 10,   // 0.1 %
///     window_ms: 400,
///     direction: None,     // either direction
/// });
///
/// let plugin = AcePlugin::builder()
///     .trigger(trigger)
///     .placement(OrderPlacement::TopOfBlock)
///     .tip_lamports(100_000)
///     .build();
/// # }
/// ```
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, instrument, warn};

use crate::bundle::BundleResult;
use crate::error::{JitoBamError, Result};

// ═══════════════════════════════════════════════════════════════════════════
//  Trigger conditions
// ═══════════════════════════════════════════════════════════════════════════

/// Direction filter for price-move triggers.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum PriceDirection {
    Up,
    Down,
}

impl std::fmt::Display for PriceDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Up => write!(f, "UP"),
            Self::Down => write!(f, "DOWN"),
        }
    }
}

/// Trigger when a Pyth price feed moves by more than `threshold_bps`
/// basis points within a rolling `window_ms` window.
///
/// 10 bps = 0.1 %, 100 bps = 1 %.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PriceMoveTrigger {
    /// Pyth feed symbol (e.g. `"SOL/USD"`) or hex feed ID.
    pub feed_id: String,
    /// Minimum move in basis points to trigger.
    pub threshold_bps: u32,
    /// Rolling observation window in milliseconds.
    pub window_ms: u64,
    /// `None` = either direction triggers.
    pub direction: Option<PriceDirection>,
}

/// Trigger when the price crosses a fixed level.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PriceLevelTrigger {
    pub feed_id: String,
    /// Price level (scaled to Pyth exponent, e.g. USD with 8 decimals).
    pub level: i64,
    /// `true` → trigger when price goes above; `false` → below.
    pub above: bool,
}

/// Trigger when the spread between two feeds exceeds a threshold.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpreadTrigger {
    pub feed_a: String,
    pub feed_b: String,
    /// Minimum spread in basis points.
    pub min_spread_bps: u32,
}

/// Trigger on a time-based schedule (e.g. every N milliseconds).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimerTrigger {
    /// Interval between trigger evaluations.
    pub interval_ms: u64,
}

/// A single trigger condition.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TriggerCondition {
    /// Pyth price moves > threshold within a time window.
    PriceMove(PriceMoveTrigger),
    /// Price crosses a fixed level.
    PriceLevel(PriceLevelTrigger),
    /// Spread between two feeds exceeds threshold.
    Spread(SpreadTrigger),
    /// Time interval (heartbeat / TWAP rebalance).
    Timer(TimerTrigger),
    /// Always true — useful for testing or unconditional top-of-block.
    Always,
}

impl std::fmt::Display for TriggerCondition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PriceMove(t) => write!(
                f,
                "PriceMove({} >{} bps/{}ms{})",
                t.feed_id,
                t.threshold_bps,
                t.window_ms,
                t.direction
                    .as_ref()
                    .map(|d| format!(" {d}"))
                    .unwrap_or_default()
            ),
            Self::PriceLevel(t) => write!(
                f,
                "PriceLevel({} {} {})",
                t.feed_id,
                if t.above { ">" } else { "<" },
                t.level
            ),
            Self::Spread(t) => write!(
                f,
                "Spread({}/{} >{} bps)",
                t.feed_a, t.feed_b, t.min_spread_bps
            ),
            Self::Timer(t) => write!(f, "Timer({}ms)", t.interval_ms),
            Self::Always => write!(f, "Always"),
        }
    }
}

/// Composite trigger logic — combine multiple conditions.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum CompositeTrigger {
    /// Single condition.
    Single(TriggerCondition),
    /// ALL conditions must be met.
    And(Vec<CompositeTrigger>),
    /// ANY condition suffices.
    Or(Vec<CompositeTrigger>),
}

impl std::fmt::Display for CompositeTrigger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Single(c) => write!(f, "{c}"),
            Self::And(cs) => {
                let parts: Vec<String> = cs.iter().map(|c| format!("{c}")).collect();
                write!(f, "AND({})", parts.join(", "))
            }
            Self::Or(cs) => {
                let parts: Vec<String> = cs.iter().map(|c| format!("{c}")).collect();
                write!(f, "OR({})", parts.join(", "))
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Block placement
// ═══════════════════════════════════════════════════════════════════════════

/// Where in the BAM-assembled block the transaction should be placed.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum OrderPlacement {
    /// Position 0 — first transaction in the block.
    TopOfBlock,
    /// Position N — specific index (0-based).
    AtPosition(u32),
    /// Let the block-engine decide (default auction behaviour).
    BestEffort,
}

impl Default for OrderPlacement {
    fn default() -> Self {
        Self::TopOfBlock
    }
}

impl std::fmt::Display for OrderPlacement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TopOfBlock => write!(f, "TOP_OF_BLOCK"),
            Self::AtPosition(n) => write!(f, "position[{n}]"),
            Self::BestEffort => write!(f, "BEST_EFFORT"),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Price snapshot ring buffer
// ═══════════════════════════════════════════════════════════════════════════

/// A single price observation.
#[derive(Clone, Debug)]
pub struct PriceSnapshot {
    /// Price in the feed's native precision (e.g. Pyth i64 with exponent).
    pub price: i64,
    /// Confidence interval (Pyth conf).
    pub confidence: u64,
    /// When this observation was recorded (monotonic).
    pub timestamp: Instant,
    /// Epoch millis from the oracle (for logging).
    pub oracle_timestamp_ms: u64,
}

/// Fixed-capacity ring buffer for recent price observations.
///
/// Used by the trigger engine to evaluate rolling-window conditions
/// without allocating.
#[derive(Debug)]
pub struct PriceWindow {
    buffer: Vec<PriceSnapshot>,
    capacity: usize,
    head: usize,
    len: usize,
}

impl PriceWindow {
    pub fn new(capacity: usize) -> Self {
        Self {
            buffer: Vec::with_capacity(capacity),
            capacity,
            head: 0,
            len: 0,
        }
    }

    /// Push a new observation, evicting the oldest if full.
    pub fn push(&mut self, snap: PriceSnapshot) {
        if self.buffer.len() < self.capacity {
            self.buffer.push(snap);
            self.len = self.buffer.len();
        } else {
            self.buffer[self.head] = snap;
            self.head = (self.head + 1) % self.capacity;
            self.len = self.capacity;
        }
    }

    /// Most recent observation.
    pub fn latest(&self) -> Option<&PriceSnapshot> {
        if self.len == 0 {
            return None;
        }
        let idx = if self.buffer.len() < self.capacity {
            self.len - 1
        } else {
            (self.head + self.capacity - 1) % self.capacity
        };
        Some(&self.buffer[idx])
    }

    /// Oldest observation still within `window` duration.
    pub fn oldest_within(&self, window: Duration) -> Option<&PriceSnapshot> {
        if self.len == 0 {
            return None;
        }
        let now = self.latest()?.timestamp;
        let cutoff = now - window;

        // Scan from oldest to newest, return the first one within window.
        for i in 0..self.len {
            let idx = if self.buffer.len() < self.capacity {
                i
            } else {
                (self.head + i) % self.capacity
            };
            if self.buffer[idx].timestamp >= cutoff {
                return Some(&self.buffer[idx]);
            }
        }
        None
    }

    /// Calculate the price move in basis points between the oldest
    /// observation within `window` and the latest.
    pub fn move_bps(&self, window: Duration) -> Option<(i64, i64, f64)> {
        let oldest = self.oldest_within(window)?;
        let latest = self.latest()?;

        if oldest.price == 0 {
            return None;
        }

        let delta = latest.price - oldest.price;
        let bps = (delta as f64 / oldest.price as f64) * 10_000.0;
        Some((oldest.price, latest.price, bps))
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Trigger evaluation engine
// ═══════════════════════════════════════════════════════════════════════════

/// Result of evaluating a trigger condition.
#[derive(Clone, Debug)]
pub struct TriggerEvaluation {
    /// Did the condition fire?
    pub fired: bool,
    /// Human-readable description of the evaluation.
    pub reason: String,
    /// Observed move in basis points (for price-move triggers).
    pub observed_bps: Option<f64>,
    /// Direction of the observed move.
    pub observed_direction: Option<PriceDirection>,
    /// Evaluation timestamp.
    pub evaluated_at: Instant,
}

impl std::fmt::Display for TriggerEvaluation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let icon = if self.fired { "🔥" } else { "·" };
        write!(f, "[{icon}] {}", self.reason)?;
        if let Some(bps) = self.observed_bps {
            write!(f, " ({bps:+.1} bps")?;
            if let Some(ref dir) = self.observed_direction {
                write!(f, " {dir}")?;
            }
            write!(f, ")")?;
        }
        Ok(())
    }
}

/// The trigger-evaluation engine.  Holds price windows and evaluates
/// conditions each tick.
pub struct TriggerEngine {
    /// Per-feed price windows.  Key = feed_id.
    price_windows: std::collections::HashMap<String, PriceWindow>,
    /// Ring buffer capacity per feed.
    window_capacity: usize,
    /// Last time a Timer trigger fired (per interval).
    last_timer_fire: std::collections::HashMap<u64, Instant>,
}

impl TriggerEngine {
    pub fn new(window_capacity: usize) -> Self {
        Self {
            price_windows: std::collections::HashMap::new(),
            window_capacity,
            last_timer_fire: std::collections::HashMap::new(),
        }
    }

    /// Record a new price observation for a feed.
    pub fn record_price(
        &mut self,
        feed_id: &str,
        price: i64,
        confidence: u64,
        oracle_timestamp_ms: u64,
    ) {
        let window = self
            .price_windows
            .entry(feed_id.to_string())
            .or_insert_with(|| PriceWindow::new(self.window_capacity));

        window.push(PriceSnapshot {
            price,
            confidence,
            timestamp: Instant::now(),
            oracle_timestamp_ms,
        });
    }

    /// Evaluate a single trigger condition.
    #[instrument(skip(self), fields(%condition))]
    pub fn evaluate(&mut self, condition: &TriggerCondition) -> TriggerEvaluation {
        match condition {
            TriggerCondition::PriceMove(trigger) => self.eval_price_move(trigger),
            TriggerCondition::PriceLevel(trigger) => self.eval_price_level(trigger),
            TriggerCondition::Spread(trigger) => self.eval_spread(trigger),
            TriggerCondition::Timer(trigger) => self.eval_timer(trigger),
            TriggerCondition::Always => TriggerEvaluation {
                fired: true,
                reason: "Always trigger".into(),
                observed_bps: None,
                observed_direction: None,
                evaluated_at: Instant::now(),
            },
        }
    }

    /// Evaluate a composite trigger (AND / OR tree).
    pub fn evaluate_composite(&mut self, trigger: &CompositeTrigger) -> TriggerEvaluation {
        match trigger {
            CompositeTrigger::Single(c) => self.evaluate(c),
            CompositeTrigger::And(conditions) => {
                let evals: Vec<TriggerEvaluation> = conditions
                    .iter()
                    .map(|c| self.evaluate_composite(c))
                    .collect();
                let all_fired = evals.iter().all(|e| e.fired);
                TriggerEvaluation {
                    fired: all_fired,
                    reason: format!(
                        "AND({}/{})",
                        evals.iter().filter(|e| e.fired).count(),
                        evals.len()
                    ),
                    observed_bps: evals.iter().find_map(|e| e.observed_bps),
                    observed_direction: evals.iter().find_map(|e| e.observed_direction.clone()),
                    evaluated_at: Instant::now(),
                }
            }
            CompositeTrigger::Or(conditions) => {
                let evals: Vec<TriggerEvaluation> = conditions
                    .iter()
                    .map(|c| self.evaluate_composite(c))
                    .collect();
                let any_fired = evals.iter().any(|e| e.fired);
                TriggerEvaluation {
                    fired: any_fired,
                    reason: format!(
                        "OR({}/{})",
                        evals.iter().filter(|e| e.fired).count(),
                        evals.len()
                    ),
                    observed_bps: evals.iter().find(|e| e.fired).and_then(|e| e.observed_bps),
                    observed_direction: evals
                        .iter()
                        .find(|e| e.fired)
                        .and_then(|e| e.observed_direction.clone()),
                    evaluated_at: Instant::now(),
                }
            }
        }
    }

    // ── Individual evaluators ───────────────────────────────────────────

    fn eval_price_move(&self, trigger: &PriceMoveTrigger) -> TriggerEvaluation {
        let window = match self.price_windows.get(&trigger.feed_id) {
            Some(w) => w,
            None => {
                return TriggerEvaluation {
                    fired: false,
                    reason: format!("no data for feed {}", trigger.feed_id),
                    observed_bps: None,
                    observed_direction: None,
                    evaluated_at: Instant::now(),
                }
            }
        };

        let duration = Duration::from_millis(trigger.window_ms);
        let result = window.move_bps(duration);

        match result {
            Some((old_price, new_price, bps)) => {
                let abs_bps = bps.abs();
                let direction = if bps > 0.0 {
                    PriceDirection::Up
                } else {
                    PriceDirection::Down
                };

                // Check threshold
                let threshold_met = abs_bps >= trigger.threshold_bps as f64;

                // Check direction filter
                let direction_ok = trigger
                    .direction
                    .as_ref()
                    .map(|d| *d == direction)
                    .unwrap_or(true);

                let fired = threshold_met && direction_ok;

                debug!(
                    feed = trigger.feed_id,
                    old_price,
                    new_price,
                    bps = format!("{bps:+.2}"),
                    threshold = trigger.threshold_bps,
                    fired,
                    "price-move evaluation"
                );

                TriggerEvaluation {
                    fired,
                    reason: format!(
                        "{}: {old_price}→{new_price} ({bps:+.1} bps / {}ms window)",
                        trigger.feed_id, trigger.window_ms
                    ),
                    observed_bps: Some(bps),
                    observed_direction: Some(direction),
                    evaluated_at: Instant::now(),
                }
            }
            None => TriggerEvaluation {
                fired: false,
                reason: format!(
                    "{}: insufficient data in {}ms window",
                    trigger.feed_id, trigger.window_ms
                ),
                observed_bps: None,
                observed_direction: None,
                evaluated_at: Instant::now(),
            },
        }
    }

    fn eval_price_level(&self, trigger: &PriceLevelTrigger) -> TriggerEvaluation {
        let window = match self.price_windows.get(&trigger.feed_id) {
            Some(w) => w,
            None => {
                return TriggerEvaluation {
                    fired: false,
                    reason: format!("no data for feed {}", trigger.feed_id),
                    observed_bps: None,
                    observed_direction: None,
                    evaluated_at: Instant::now(),
                }
            }
        };

        match window.latest() {
            Some(snap) => {
                let fired = if trigger.above {
                    snap.price > trigger.level
                } else {
                    snap.price < trigger.level
                };

                TriggerEvaluation {
                    fired,
                    reason: format!(
                        "{}: {} {} {} (current={})",
                        trigger.feed_id,
                        snap.price,
                        if trigger.above { ">" } else { "<" },
                        trigger.level,
                        snap.price
                    ),
                    observed_bps: None,
                    observed_direction: None,
                    evaluated_at: Instant::now(),
                }
            }
            None => TriggerEvaluation {
                fired: false,
                reason: format!("no data for feed {}", trigger.feed_id),
                observed_bps: None,
                observed_direction: None,
                evaluated_at: Instant::now(),
            },
        }
    }

    fn eval_spread(&self, trigger: &SpreadTrigger) -> TriggerEvaluation {
        let price_a = self
            .price_windows
            .get(&trigger.feed_a)
            .and_then(|w| w.latest())
            .map(|s| s.price);
        let price_b = self
            .price_windows
            .get(&trigger.feed_b)
            .and_then(|w| w.latest())
            .map(|s| s.price);

        match (price_a, price_b) {
            (Some(a), Some(b)) if b != 0 => {
                let spread_bps = ((a - b) as f64 / b as f64).abs() * 10_000.0;
                let fired = spread_bps >= trigger.min_spread_bps as f64;

                TriggerEvaluation {
                    fired,
                    reason: format!(
                        "{}/{}: spread={spread_bps:.1} bps (threshold={})",
                        trigger.feed_a, trigger.feed_b, trigger.min_spread_bps
                    ),
                    observed_bps: Some(spread_bps),
                    observed_direction: None,
                    evaluated_at: Instant::now(),
                }
            }
            _ => TriggerEvaluation {
                fired: false,
                reason: format!(
                    "insufficient data for {}/{}",
                    trigger.feed_a, trigger.feed_b
                ),
                observed_bps: None,
                observed_direction: None,
                evaluated_at: Instant::now(),
            },
        }
    }

    fn eval_timer(&mut self, trigger: &TimerTrigger) -> TriggerEvaluation {
        let now = Instant::now();
        let interval = Duration::from_millis(trigger.interval_ms);

        let last = self.last_timer_fire.get(&trigger.interval_ms).copied();

        let fired = match last {
            Some(t) => now.duration_since(t) >= interval,
            None => true, // first evaluation always fires
        };

        if fired {
            self.last_timer_fire.insert(trigger.interval_ms, now);
        }

        TriggerEvaluation {
            fired,
            reason: format!("timer({}ms)", trigger.interval_ms),
            observed_bps: None,
            observed_direction: None,
            evaluated_at: now,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  ACE Plugin
// ═══════════════════════════════════════════════════════════════════════════

/// Configuration for an ACE plugin.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AcePluginConfig {
    /// Unique plugin identifier.
    pub plugin_id: String,
    /// Human-readable label.
    pub label: String,
    /// Tip for top-of-block placement (in lamports).
    pub tip_lamports: u64,
    /// Where in the block the tx should land.
    pub placement: OrderPlacement,
    /// Maximum number of consecutive trigger fires before cooldown.
    pub max_fires_per_minute: u32,
    /// Whether the plugin is currently armed.
    pub armed: bool,
}

impl Default for AcePluginConfig {
    fn default() -> Self {
        Self {
            plugin_id: format!("ace_{}", rand::random::<u32>()),
            label: "Unnamed ACE Plugin".into(),
            tip_lamports: 100_000, // 0.0001 SOL — aggressive top-of-block tip
            placement: OrderPlacement::TopOfBlock,
            max_fires_per_minute: 10,
            armed: true,
        }
    }
}

/// An ACE plugin instance — combines a trigger condition with execution
/// parameters and rate limiting.
pub struct AcePlugin {
    pub config: AcePluginConfig,
    pub trigger: CompositeTrigger,
    fire_count: u32,
    minute_start: Instant,
}

impl AcePlugin {
    /// Start building an ACE plugin with default config.
    pub fn builder() -> AcePluginBuilder {
        AcePluginBuilder::default()
    }

    /// Evaluate the trigger and return whether the plugin should fire.
    ///
    /// Respects rate limiting — if the fire count exceeds the configured
    /// maximum per minute, the trigger is suppressed.
    pub fn should_fire(&mut self, engine: &mut TriggerEngine) -> TriggerEvaluation {
        if !self.config.armed {
            return TriggerEvaluation {
                fired: false,
                reason: "plugin disarmed".into(),
                observed_bps: None,
                observed_direction: None,
                evaluated_at: Instant::now(),
            };
        }

        // Rate limit check
        let now = Instant::now();
        if now.duration_since(self.minute_start) > Duration::from_secs(60) {
            self.fire_count = 0;
            self.minute_start = now;
        }

        if self.fire_count >= self.config.max_fires_per_minute {
            return TriggerEvaluation {
                fired: false,
                reason: format!(
                    "rate limited ({}/{} fires/min)",
                    self.fire_count, self.config.max_fires_per_minute
                ),
                observed_bps: None,
                observed_direction: None,
                evaluated_at: now,
            };
        }

        let mut eval = engine.evaluate_composite(&self.trigger);

        if eval.fired {
            self.fire_count += 1;
            info!(
                plugin = self.config.label,
                fires = self.fire_count,
                reason = %eval.reason,
                "ACE trigger FIRED"
            );
        }

        eval
    }

    /// Arm or disarm the plugin.
    pub fn set_armed(&mut self, armed: bool) {
        self.config.armed = armed;
        info!(plugin = self.config.label, armed, "plugin armed state changed");
    }

    /// Reset the per-minute fire counter.
    pub fn reset_rate_limit(&mut self) {
        self.fire_count = 0;
        self.minute_start = Instant::now();
    }

    /// The registration payload that would be sent to the BAM node's ACE
    /// endpoint (serialisable).
    pub fn registration_payload(&self) -> AceRegistration {
        AceRegistration {
            plugin_id: self.config.plugin_id.clone(),
            label: self.config.label.clone(),
            trigger: self.trigger.clone(),
            placement: self.config.placement.clone(),
            tip_lamports: self.config.tip_lamports,
        }
    }
}

/// Serialisable registration payload sent to the BAM node.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AceRegistration {
    pub plugin_id: String,
    pub label: String,
    pub trigger: CompositeTrigger,
    pub placement: OrderPlacement,
    pub tip_lamports: u64,
}

// ═══════════════════════════════════════════════════════════════════════════
//  Builder
// ═══════════════════════════════════════════════════════════════════════════

/// Fluent builder for [`AcePlugin`].
pub struct AcePluginBuilder {
    config: AcePluginConfig,
    trigger: Option<CompositeTrigger>,
}

impl Default for AcePluginBuilder {
    fn default() -> Self {
        Self {
            config: AcePluginConfig::default(),
            trigger: None,
        }
    }
}

impl AcePluginBuilder {
    pub fn plugin_id(mut self, id: impl Into<String>) -> Self {
        self.config.plugin_id = id.into();
        self
    }

    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.config.label = label.into();
        self
    }

    /// Set a single trigger condition.
    pub fn trigger(mut self, condition: TriggerCondition) -> Self {
        self.trigger = Some(CompositeTrigger::Single(condition));
        self
    }

    /// Set a composite trigger (AND / OR combinations).
    pub fn composite_trigger(mut self, trigger: CompositeTrigger) -> Self {
        self.trigger = Some(trigger);
        self
    }

    pub fn placement(mut self, placement: OrderPlacement) -> Self {
        self.config.placement = placement;
        self
    }

    pub fn tip_lamports(mut self, tip: u64) -> Self {
        self.config.tip_lamports = tip;
        self
    }

    pub fn max_fires_per_minute(mut self, max: u32) -> Self {
        self.config.max_fires_per_minute = max;
        self
    }

    pub fn build(self) -> AcePlugin {
        AcePlugin {
            config: self.config,
            trigger: self.trigger.unwrap_or(CompositeTrigger::Single(TriggerCondition::Always)),
            fire_count: 0,
            minute_start: Instant::now(),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    // ── Price window tests ──────────────────────────────────────────────

    #[test]
    fn price_window_push_and_latest() {
        let mut w = PriceWindow::new(4);
        assert!(w.is_empty());

        w.push(PriceSnapshot {
            price: 100_00,
            confidence: 10,
            timestamp: Instant::now(),
            oracle_timestamp_ms: 0,
        });

        assert_eq!(w.len(), 1);
        assert_eq!(w.latest().unwrap().price, 100_00);
    }

    #[test]
    fn price_window_ring_eviction() {
        let mut w = PriceWindow::new(3);
        for i in 1..=5 {
            w.push(PriceSnapshot {
                price: i * 100,
                confidence: 0,
                timestamp: Instant::now(),
                oracle_timestamp_ms: 0,
            });
        }

        // Capacity is 3, so only items 3,4,5 remain
        assert_eq!(w.len(), 3);
        assert_eq!(w.latest().unwrap().price, 500);
    }

    #[test]
    fn price_window_move_bps() {
        let mut w = PriceWindow::new(100);
        let base = Instant::now();

        // Price at 100.00 (using 2 decimal precision as integer)
        w.push(PriceSnapshot {
            price: 10_000,
            confidence: 0,
            timestamp: base,
            oracle_timestamp_ms: 0,
        });
        // Price at 100.10 → +10 bps
        w.push(PriceSnapshot {
            price: 10_010,
            confidence: 0,
            timestamp: base, // same instant for determinism
            oracle_timestamp_ms: 0,
        });

        let (old, new, bps) = w.move_bps(Duration::from_secs(1)).unwrap();
        assert_eq!(old, 10_000);
        assert_eq!(new, 10_010);
        assert!((bps - 10.0).abs() < 0.01, "expected ~10 bps, got {bps}");
    }

    // ── Trigger engine tests ────────────────────────────────────────────

    #[test]
    fn trigger_price_move_fires() {
        let mut engine = TriggerEngine::new(1000);

        // Record a baseline price, then a moved price
        engine.record_price("SOL/USD", 15_000_000_000, 1000, 1000); // $150.00
        engine.record_price("SOL/USD", 15_015_000_000, 1000, 1400); // $150.15 → +10 bps

        let condition = TriggerCondition::PriceMove(PriceMoveTrigger {
            feed_id: "SOL/USD".into(),
            threshold_bps: 10,
            window_ms: 400,
            direction: None,
        });

        let eval = engine.evaluate(&condition);
        assert!(eval.fired, "should fire: {eval}");
        assert!(eval.observed_bps.unwrap() > 9.0);
    }

    #[test]
    fn trigger_price_move_below_threshold() {
        let mut engine = TriggerEngine::new(1000);

        engine.record_price("SOL/USD", 15_000_000_000, 1000, 1000);
        engine.record_price("SOL/USD", 15_001_000_000, 1000, 1400); // ~0.67 bps

        let condition = TriggerCondition::PriceMove(PriceMoveTrigger {
            feed_id: "SOL/USD".into(),
            threshold_bps: 10,
            window_ms: 400,
            direction: None,
        });

        let eval = engine.evaluate(&condition);
        assert!(!eval.fired, "should NOT fire: {eval}");
    }

    #[test]
    fn trigger_price_move_direction_filter() {
        let mut engine = TriggerEngine::new(1000);

        engine.record_price("SOL/USD", 15_000_000_000, 1000, 1000);
        engine.record_price("SOL/USD", 14_985_000_000, 1000, 1400); // −10 bps (down)

        // Trigger only on UP moves
        let condition = TriggerCondition::PriceMove(PriceMoveTrigger {
            feed_id: "SOL/USD".into(),
            threshold_bps: 10,
            window_ms: 400,
            direction: Some(PriceDirection::Up),
        });

        let eval = engine.evaluate(&condition);
        assert!(!eval.fired, "UP-only trigger should not fire on down move");

        // Trigger on DOWN moves
        let condition_down = TriggerCondition::PriceMove(PriceMoveTrigger {
            feed_id: "SOL/USD".into(),
            threshold_bps: 10,
            window_ms: 400,
            direction: Some(PriceDirection::Down),
        });

        let eval_down = engine.evaluate(&condition_down);
        assert!(eval_down.fired, "DOWN trigger should fire: {eval_down}");
    }

    #[test]
    fn trigger_price_level() {
        let mut engine = TriggerEngine::new(100);
        engine.record_price("SOL/USD", 15_500_000_000, 1000, 1000);

        let above = TriggerCondition::PriceLevel(PriceLevelTrigger {
            feed_id: "SOL/USD".into(),
            level: 15_000_000_000,
            above: true,
        });

        assert!(engine.evaluate(&above).fired);

        let below = TriggerCondition::PriceLevel(PriceLevelTrigger {
            feed_id: "SOL/USD".into(),
            level: 16_000_000_000,
            above: true,
        });

        assert!(!engine.evaluate(&below).fired);
    }

    #[test]
    fn trigger_always() {
        let mut engine = TriggerEngine::new(10);
        let eval = engine.evaluate(&TriggerCondition::Always);
        assert!(eval.fired);
    }

    #[test]
    fn trigger_timer() {
        let mut engine = TriggerEngine::new(10);
        let timer = TriggerCondition::Timer(TimerTrigger { interval_ms: 50 });

        // First eval always fires
        assert!(engine.evaluate(&timer).fired);
        // Immediate second eval should NOT fire (< 50ms)
        assert!(!engine.evaluate(&timer).fired);
    }

    // ── Composite trigger tests ─────────────────────────────────────────

    #[test]
    fn composite_and_both_true() {
        let mut engine = TriggerEngine::new(100);
        engine.record_price("SOL/USD", 15_000_000_000, 1000, 1000);
        engine.record_price("SOL/USD", 15_020_000_000, 1000, 1400);

        let trigger = CompositeTrigger::And(vec![
            CompositeTrigger::Single(TriggerCondition::PriceMove(PriceMoveTrigger {
                feed_id: "SOL/USD".into(),
                threshold_bps: 10,
                window_ms: 1000,
                direction: None,
            })),
            CompositeTrigger::Single(TriggerCondition::Always),
        ]);

        assert!(engine.evaluate_composite(&trigger).fired);
    }

    #[test]
    fn composite_and_one_false() {
        let mut engine = TriggerEngine::new(100);
        engine.record_price("SOL/USD", 15_000_000_000, 1000, 1000);
        engine.record_price("SOL/USD", 15_000_100_000, 1000, 1400); // tiny move

        let trigger = CompositeTrigger::And(vec![
            CompositeTrigger::Single(TriggerCondition::PriceMove(PriceMoveTrigger {
                feed_id: "SOL/USD".into(),
                threshold_bps: 10,
                window_ms: 1000,
                direction: None,
            })),
            CompositeTrigger::Single(TriggerCondition::Always),
        ]);

        assert!(!engine.evaluate_composite(&trigger).fired);
    }

    #[test]
    fn composite_or() {
        let mut engine = TriggerEngine::new(100);

        let trigger = CompositeTrigger::Or(vec![
            CompositeTrigger::Single(TriggerCondition::PriceMove(PriceMoveTrigger {
                feed_id: "MISSING".into(),
                threshold_bps: 10,
                window_ms: 400,
                direction: None,
            })),
            CompositeTrigger::Single(TriggerCondition::Always),
        ]);

        assert!(engine.evaluate_composite(&trigger).fired);
    }

    // ── ACE Plugin tests ────────────────────────────────────────────────

    #[test]
    fn plugin_builder() {
        let plugin = AcePlugin::builder()
            .label("SOL momentum")
            .trigger(TriggerCondition::PriceMove(PriceMoveTrigger {
                feed_id: "SOL/USD".into(),
                threshold_bps: 10,
                window_ms: 400,
                direction: None,
            }))
            .placement(OrderPlacement::TopOfBlock)
            .tip_lamports(100_000)
            .max_fires_per_minute(5)
            .build();

        assert_eq!(plugin.config.tip_lamports, 100_000);
        assert_eq!(plugin.config.placement, OrderPlacement::TopOfBlock);
        assert_eq!(plugin.config.max_fires_per_minute, 5);
    }

    #[test]
    fn plugin_rate_limiting() {
        let mut engine = TriggerEngine::new(100);
        let mut plugin = AcePlugin::builder()
            .trigger(TriggerCondition::Always)
            .max_fires_per_minute(2)
            .build();

        assert!(plugin.should_fire(&mut engine).fired);
        assert!(plugin.should_fire(&mut engine).fired);
        // Third time is rate-limited
        let eval = plugin.should_fire(&mut engine);
        assert!(!eval.fired, "should be rate-limited: {eval}");
        assert!(eval.reason.contains("rate limited"));
    }

    #[test]
    fn plugin_disarmed() {
        let mut engine = TriggerEngine::new(100);
        let mut plugin = AcePlugin::builder()
            .trigger(TriggerCondition::Always)
            .build();

        plugin.set_armed(false);
        assert!(!plugin.should_fire(&mut engine).fired);
    }

    #[test]
    fn plugin_registration_serializes() {
        let plugin = AcePlugin::builder()
            .plugin_id("test_001")
            .label("SOL momentum 0.1%/400ms")
            .trigger(TriggerCondition::PriceMove(PriceMoveTrigger {
                feed_id: "SOL/USD".into(),
                threshold_bps: 10,
                window_ms: 400,
                direction: None,
            }))
            .placement(OrderPlacement::TopOfBlock)
            .tip_lamports(100_000)
            .build();

        let reg = plugin.registration_payload();
        let json = serde_json::to_string_pretty(&reg).unwrap();
        assert!(json.contains("SOL/USD"));
        assert!(json.contains("TopOfBlock"));
        assert!(json.contains("100000"));
    }
}
