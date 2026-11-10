//! Budget-aware swarm strategy plugin (`swarm-strategy:budget_aware@v1`).
//!
//! Unlike `swarm-competitive`, which races N peers to completion on a single
//! model, this strategy treats a swarm as a *cost-aware* search:
//!
//! 1. **Per-peer model routing.** Each hypothesis carries a concrete `model`
//!    chosen from the caller-supplied `available_models` catalog (or the bundled
//!    OpenCode Go catalog when the caller supplies nothing). The host applies it
//!    per peer, so a swarm can mix a premium architect with bulk implementers
//!    and a free-tier fuzzer in the same run.
//! 2. **Successive halving.** Mid-flight evaluation returns `prune_peer_ids`
//!    so the host can cancel dominated peers instead of paying for a full race.
//! 3. **Cost telemetry.** Every candidate carries token/latency telemetry; the
//!    score subtracts a normalised cost penalty so a cheap peer wins ties it
//!    would otherwise lose to an expensive one.
//!
//! The plugin is a pure policy layer: it never launches processes, reads files,
//! or talks to a model. All execution stays in the Basalt host.

use std::collections::HashMap;

use basalt_plugin_sdk::prelude::*;
use serde::{Deserialize, Serialize};

pub const PLUGIN_NAME: &str = "swarm-budget";
pub const PLUGIN_VERSION: &str = "0.1.0";
pub const STRATEGY_ID: &str = "budget_aware";

/// Assumed share of prompt tokens served from the provider prompt cache.
/// Agentic loops resend a stable prefix every turn, so this is deliberately high.
pub const DEFAULT_CACHED_READ_RATIO: f64 = 0.85;

/// Assumed output tokens per peer for a planning-time cost estimate.
pub const ASSUMED_OUTPUT_TOKENS: f64 = 3_000.0;

    /// Assumed prompt tokens per peer for a planning-time cost estimate.
pub const ASSUMED_PROMPT_TOKENS: f64 = 70_000.0;

/// Conservative rates used when a model's pricing is unknown. An unpriced model
/// must never look free, or the router would prefer it by accident.
pub const UNKNOWN_INPUT_PER_MTOK: f64 = 1.0;
pub const UNKNOWN_OUTPUT_PER_MTOK: f64 = 4.0;

/// Round index before which mid-flight pruning is never allowed.
pub const MIN_PRUNE_ROUND: u32 = 2;

// ── Host imports for WASM target ──────────────────────────────────────────────

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "env")]
unsafe extern "C" {
    fn basalt_log(level: i32, msg_ptr: *const u8, msg_len: usize);
}

#[cfg(not(target_arch = "wasm32"))]
#[no_mangle]
unsafe extern "C" fn basalt_log(_: i32, _: *const u8, _: usize) {}

#[allow(dead_code)]
fn log_info(msg: &str) {
    unsafe {
        basalt_log(1, msg.as_ptr(), msg.len());
    }
}

// ── Plugin Metadata Declaration ───────────────────────────────────────────────

basalt_plugin_meta! {
    name:              "swarm-budget",
    version:           "0.1.0",
    hook_flags:        CAP_SWARM_STRATEGY | CAP_CAPABILITY_HANDLE,
    provides:          "swarm-strategy:budget_aware\nswarm-strategy:budget-aware\nswarm:successive-halving",
    requires:          "",
    // Soft pairing, not a gate: the host discovers this launcher's model list
    // and forwards it as `available_models`. The strategy still works with any
    // launcher (codex, copilot, ...) or with no launcher catalog at all.
    optional_requires: "agent-launcher@opencode/v1",
    file_globs:        "",
    activates_on:      "",
    activation_events: "",
}

// ── Model catalog ─────────────────────────────────────────────────────────────

/// One model the router may assign to a peer.
///
/// Prices are USD per million tokens. `bucket_usd` is the model's monthly
/// allowance (0 = unmetered). `peak_multiplier` is applied when the caller sets
/// `BudgetSpec::peak_pricing` (e.g. DeepSeek peak hours, which double prices).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelSpec {
    /// Fully-qualified model id understood by the peer launcher,
    /// e.g. `opencode-go/deepseek-v4.1-flash`.
    pub id: String,
    /// Launcher plugin this id belongs to (`opencode`, `codex`, ...).
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub input_per_mtok: f64,
    #[serde(default)]
    pub output_per_mtok: f64,
    #[serde(default)]
    pub cached_read_per_mtok: f64,
    #[serde(default)]
    pub cache_write_per_mtok: f64,
    /// Context size at which the provider switches to a more expensive rate.
    #[serde(default)]
    pub price_cliff_tokens: Option<u64>,
    /// Rate multiplier applied above `price_cliff_tokens` (1.0 = no cliff).
    #[serde(default)]
    pub cliff_multiplier: f64,
    /// Monthly allowance for this model, in USD (0 = unmetered).
    #[serde(default)]
    pub bucket_usd: f64,
    /// Rate multiplier during peak hours (1.0 = uniform pricing).
    #[serde(default)]
    pub peak_multiplier: f64,
    /// `free` | `bulk` | `workhorse` | `premium`.
    #[serde(default)]
    pub tier: String,
    /// Zero marginal cost.
    #[serde(default)]
    pub free: bool,
    /// The provider trains on request data. Excluded from routing unless
    /// `BudgetSpec::allow_training_models` is set.
    #[serde(default)]
    pub trains_on_data: bool,
    #[serde(default)]
    pub tags: Vec<String>,
}

impl ModelSpec {
    fn tier_or_default(&self) -> &str {
        if self.tier.is_empty() {
            if self.free {
                "free"
            } else if self.bucket_usd > 0.0 && self.bucket_usd <= 20.0 {
                "premium"
            } else if self.input_per_mtok >= 1.0 {
                "workhorse"
            } else {
                "bulk"
            }
        } else {
            self.tier.as_str()
        }
    }

    /// Explicitly zero-cost. Only `free: true` qualifies: a model with unknown
    /// pricing is *not* free, it is merely unpriced.
    fn is_free(&self) -> bool {
        self.free
    }

    /// Whether this spec carries usable pricing information.
    fn has_pricing(&self) -> bool {
        self.input_per_mtok > 0.0
            || self.output_per_mtok > 0.0
            || self.cached_read_per_mtok > 0.0
    }

    /// Input rate used for ordering candidates inside a tier.
    fn effective_input_rate(&self) -> f64 {
        if self.is_free() {
            0.0
        } else if self.has_pricing() {
            self.input_per_mtok
        } else {
            UNKNOWN_INPUT_PER_MTOK
        }
    }

    fn peak_mult(&self, peak: bool) -> f64 {
        if peak && self.peak_multiplier > 0.0 {
            self.peak_multiplier
        } else {
            1.0
        }
    }

    fn cliff_mult(&self, total_tokens: u64) -> f64 {
        match self.price_cliff_tokens {
            Some(cliff) if total_tokens > cliff && self.cliff_multiplier > 0.0 => {
                self.cliff_multiplier
            }
            _ => 1.0,
        }
    }
}

/// A caller may pass either a full spec or a bare model id string.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ModelEntry {
    Spec(ModelSpec),
    Id(String),
}

/// Bundled catalog for the OpenCode Go provider. Callers can override any of
/// this by passing `available_models` on `swarm_run`.
pub fn default_catalog() -> Vec<ModelSpec> {
    use ModelTier::{Bulk, Free, Premium, Workhorse};
    let specs: Vec<(&str, f64, f64, f64, f64, f64, Option<u64>, f64, ModelTier, bool, bool)> = vec![
        // id, in, out, cached_read, cache_write, bucket, cliff, cliff_mult, tier, free, trains
        ("opencode-go/deepseek-v4.1-flash", 0.15, 0.60, 0.003, 0.0, 80.0, None, 1.0, Bulk, false, false),
        ("opencode-go/glm-5.3-flash", 0.15, 0.50, 0.03, 0.0, 60.0, None, 1.0, Bulk, false, false),
        ("opencode-go/mimo-v2.6-flash", 0.14, 0.28, 0.0028, 0.0, 60.0, None, 1.0, Bulk, false, false),
        ("opencode-go/mimo-v2.5", 0.14, 0.28, 0.0028, 0.0, 60.0, None, 1.0, Bulk, false, false),
        ("opencode-go/longcat-2.0", 0.30, 1.20, 0.006, 0.0, 60.0, None, 1.0, Bulk, false, false),
        ("opencode-go/hy3", 0.14, 0.58, 0.035, 0.0, 60.0, None, 1.0, Bulk, false, false),
        ("opencode-go/minimax-m3.1-flash", 0.15, 0.60, 0.015, 0.0, 20.0, None, 1.0, Bulk, false, false),
        ("opencode-go/mimo-v2.6-pro", 0.435, 0.87, 0.003625, 0.0, 15.0, None, 1.0, Workhorse, false, false),
        ("opencode-go/kimi-k2.7-code", 0.95, 4.00, 0.19, 0.0, 60.0, None, 1.0, Workhorse, false, false),
        ("opencode-go/minimax-m3", 0.30, 1.20, 0.06, 0.0, 60.0, Some(512_000), 2.0, Workhorse, false, false),
        ("opencode-go/qwen3.7-plus", 0.40, 1.60, 0.04, 0.50, 60.0, Some(256_000), 3.0, Workhorse, false, false),
        ("opencode-go/qwen3.8-flash", 0.15, 0.47, 0.016, 0.20, 30.0, None, 1.0, Workhorse, false, false),
        ("opencode-go/glm-5.2", 1.40, 4.40, 0.26, 0.0, 60.0, None, 1.0, Workhorse, false, false),
        ("opencode-go/glm-5.3", 1.40, 4.40, 0.26, 0.0, 15.0, None, 1.0, Premium, false, false),
        ("opencode-go/kimi-k3.1-pro", 2.00, 10.00, 0.20, 2.00, 15.0, None, 1.0, Premium, false, false),
        ("opencode-go/qwen3.8-max", 2.00, 6.00, 0.25, 2.50, 15.0, None, 1.0, Premium, false, false),
        ("opencode-go/grok-4.7", 2.00, 6.00, 0.50, 0.0, 15.0, Some(200_000), 2.0, Premium, false, false),
        ("opencode-go/step-5-preview", 1.00, 2.70, 0.10, 0.0, 15.0, None, 1.0, Premium, false, false),
        ("opencode-go/deepseek-v4-pro", 0.66, 1.98, 0.022, 0.0, 15.0, None, 1.0, Premium, false, false),
        ("opencode-go/gpt-6-luna", 0.10, 0.50, 0.01, 0.125, 15.0, Some(272_000), 2.0, Premium, false, false),
        ("opencode-go/claude-haiku-5-5", 0.10, 0.50, 0.01, 0.125, 15.0, Some(100_000), 5.0, Premium, false, false),
        ("opencode-go/gemini-4-argon", 2.00, 10.00, 0.10, 0.0, 30.0, None, 1.0, Premium, false, false),
        ("opencode-go/beam-free", 0.0, 0.0, 0.0, 0.0, 0.0, None, 1.0, Free, true, false),
        ("opencode-go/iceman-contributor-free", 0.0, 0.0, 0.0, 0.0, 0.0, None, 1.0, Free, true, true),
    ];

    specs
        .into_iter()
        .map(|(id, input, output, cached, cache_write, bucket, cliff, cliff_mult, tier, free, trains)| {
            ModelSpec {
                id: id.to_string(),
                provider: "opencode".to_string(),
                input_per_mtok: input,
                output_per_mtok: output,
                cached_read_per_mtok: cached,
                cache_write_per_mtok: cache_write,
                price_cliff_tokens: cliff,
                cliff_multiplier: cliff_mult,
                bucket_usd: bucket,
                // DeepSeek is the only Go model with documented peak pricing.
                peak_multiplier: if id.contains("deepseek") { 2.0 } else { 1.0 },
                tier: tier.as_str().to_string(),
                free,
                trains_on_data: trains,
                tags: Vec::new(),
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelTier {
    Free,
    Bulk,
    Workhorse,
    Premium,
}

impl ModelTier {
    fn as_str(self) -> &'static str {
        match self {
            ModelTier::Free => "free",
            ModelTier::Bulk => "bulk",
            ModelTier::Workhorse => "workhorse",
            ModelTier::Premium => "premium",
        }
    }
}

fn tier_rank(spec: &ModelSpec) -> u8 {
    match spec.tier_or_default() {
        "free" => 0,
        "bulk" => 1,
        "workhorse" => 2,
        "premium" => 3,
        _ => 2,
    }
}

// ── Request / response contracts ──────────────────────────────────────────────

/// Budget policy for a swarm run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetSpec {
    /// Hard ceiling for the whole run, in USD. `None` = unconstrained.
    #[serde(default)]
    pub max_usd: Option<f64>,
    /// `cost` | `balanced` | `quality`. Controls the cost penalty weight.
    #[serde(default = "default_prefer")]
    pub prefer: String,
    /// Allow peers on models whose provider trains on request data.
    #[serde(default)]
    pub allow_training_models: bool,
    /// Apply peak-hour multipliers (e.g. DeepSeek peak windows).
    #[serde(default)]
    pub peak_pricing: bool,
    /// Share of prompt tokens assumed to hit the provider cache.
    #[serde(default = "default_cached_ratio")]
    pub cached_read_ratio: f64,
    /// Max peers pruned per mid-flight round.
    #[serde(default = "default_max_prune")]
    pub max_prune_per_round: usize,
}

fn default_prefer() -> String {
    "balanced".to_string()
}
fn default_cached_ratio() -> f64 {
    DEFAULT_CACHED_READ_RATIO
}
fn default_max_prune() -> usize {
    1
}

impl Default for BudgetSpec {
    fn default() -> Self {
        Self {
            max_usd: None,
            prefer: default_prefer(),
            allow_training_models: false,
            peak_pricing: false,
            cached_read_ratio: DEFAULT_CACHED_READ_RATIO,
            max_prune_per_round: default_max_prune(),
        }
    }
}

/// Incoming plan request from the host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmPlanRequest {
    pub task: String,
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    #[serde(default)]
    pub target_files: Vec<String>,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default)]
    pub plugin: Option<String>,
    /// Caller-forced model for every peer; disables tiering when present.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub available_models: Vec<ModelEntry>,
    #[serde(default)]
    pub budget: BudgetSpec,
    #[serde(default)]
    pub parameters: HashMap<String, serde_json::Value>,
}

fn default_concurrency() -> usize {
    3
}

/// One peer assignment. Superset of the competitive plugin's hypothesis: the
/// host applies `model` per peer and turns `max_steps` into a directive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerVariantHypothesis {
    pub peer_id: u32,
    pub role: String,
    pub hypothesis_title: String,
    pub focus_directive: String,
    pub reasoning_variant: String,
    pub tool_categories: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub budget_class: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_steps: Option<u32>,
    pub expected_usd: f64,
}

/// Plan response envelope. The host accepts both this object form and a bare
/// hypothesis array (for backward compatibility with `swarm-competitive`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmPlanResponse {
    pub strategy: String,
    pub hypotheses: Vec<PeerVariantHypothesis>,
    /// Seconds of probing before the first mid-flight evaluation.
    pub probe_secs: u64,
    /// Minimum number of evaluation rounds before pruning is allowed.
    pub prune_after_round: u32,
    pub estimated_usd: f64,
    pub budget_report: BudgetReport,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AstConvergenceTelemetry {
    #[serde(default)]
    pub overlap_risk: f64,
    #[serde(default)]
    pub intersecting_symbols: Vec<String>,
    #[serde(default)]
    pub auto_merge_guaranteed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PbtFuzzTelemetry {
    #[serde(default)]
    pub passed_iterations: u32,
    #[serde(default)]
    pub target_iterations: u32,
    #[serde(default)]
    pub invariants_verified: Vec<String>,
    #[serde(default)]
    pub execution_cycles: u64,
    #[serde(default)]
    pub failed_counterexample: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntentDriftTelemetry {
    #[serde(default)]
    pub drift_score: f64,
    #[serde(default)]
    pub within_safe_boundary: bool,
    #[serde(default)]
    pub flagged_deviations: Vec<String>,
}

/// Token/latency telemetry the host attaches to every candidate.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CandidateTelemetry {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub elapsed_ms: u64,
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PeerCandidateEvaluation {
    pub peer_id: u32,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub hypothesis: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub modified_files: Vec<String>,
    #[serde(default)]
    pub check_passed: bool,
    #[serde(default)]
    pub test_passed: bool,
    #[serde(default)]
    pub ast_convergence: AstConvergenceTelemetry,
    #[serde(default)]
    pub pbt_fuzz: PbtFuzzTelemetry,
    #[serde(default)]
    pub intent_drift: IntentDriftTelemetry,
    #[serde(default)]
    pub tool_categories: Vec<String>,
    #[serde(default)]
    pub composite_score: f64,
    #[serde(default)]
    pub pruned: bool,
    #[serde(default)]
    pub telemetry: CandidateTelemetry,
    #[serde(default)]
    pub estimated_usd: f64,
}

/// Incoming evaluate request from the host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmEvaluateRequest {
    pub task: String,
    #[serde(default)]
    pub swarm_id: String,
    #[serde(default)]
    pub candidates: Vec<PeerCandidateEvaluation>,
    #[serde(default)]
    pub available_models: Vec<ModelEntry>,
    #[serde(default)]
    pub budget: BudgetSpec,
    /// `true` when the host calls this mid-run and only wants `prune_peer_ids`.
    #[serde(default)]
    pub midflight: bool,
    /// 1-based evaluation round.
    #[serde(default)]
    pub round: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PeerCost {
    pub peer_id: u32,
    pub model: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub estimated_usd: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BudgetReport {
    pub estimated_usd: f64,
    /// Spend avoided by pruning peers before they ran to completion.
    pub estimated_saved_usd: f64,
    pub max_usd: Option<f64>,
    pub over_budget: bool,
    pub by_peer: Vec<PeerCost>,
}

/// Outgoing evaluation result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmRunResponse {
    pub swarm_id: String,
    pub strategy: String,
    pub status: String,
    pub winner_peer_id: Option<u32>,
    pub winner_role: Option<String>,
    pub winner_hypothesis: Option<String>,
    pub candidates_evaluated: usize,
    pub merged_files: Vec<String>,
    pub composite_winner_score: f64,
    pub summary: String,
    pub peers: Vec<PeerCandidateEvaluation>,
    /// Peers the host should cancel. Only populated for mid-flight calls.
    pub prune_peer_ids: Vec<u32>,
    pub budget_report: BudgetReport,
}

// ── Cost model ────────────────────────────────────────────────────────────────

/// Estimate USD for one peer given its token telemetry.
pub fn estimate_usd(
    spec: &ModelSpec,
    prompt_tokens: u64,
    completion_tokens: u64,
    budget: &BudgetSpec,
) -> f64 {
    if spec.is_free() {
        return 0.0;
    }
    let prompt = prompt_tokens as f64;
    let completion = completion_tokens as f64;
    let ratio = budget.cached_read_ratio.clamp(0.0, 1.0);
    let cached = prompt * ratio;
    let fresh = prompt - cached;

    let peak = spec.peak_mult(budget.peak_pricing);

    let (input_rate, cached_rate, output_rate, cache_write_rate) = if spec.has_pricing() {
        let cliff = spec.cliff_mult(prompt_tokens.saturating_add(completion_tokens));
        let input_rate = spec.input_per_mtok * peak * cliff;
        let cached_rate = if spec.cached_read_per_mtok > 0.0 {
            spec.cached_read_per_mtok * peak * cliff
        } else {
            input_rate
        };
        (
            input_rate,
            cached_rate,
            spec.output_per_mtok * peak * cliff,
            spec.cache_write_per_mtok * peak * cliff,
        )
    } else {
        // Unpriced model: assume a conservative mid-tier rate so the router does
        // not mistake "unknown" for "cheap".
        let rate = UNKNOWN_INPUT_PER_MTOK * peak;
        (rate, rate, UNKNOWN_OUTPUT_PER_MTOK * peak, 0.0)
    };

    (fresh * input_rate
        + cached * cached_rate
        + fresh * cache_write_rate
        + completion * output_rate)
        / 1_000_000.0
}

fn find_spec<'a>(catalog: &'a [ModelSpec], id: &str) -> Option<&'a ModelSpec> {
    catalog.iter().find(|m| m.id == id)
}

fn resolve_catalog(entries: &[ModelEntry]) -> Vec<ModelSpec> {
    if entries.is_empty() {
        return default_catalog();
    }
    let defaults = default_catalog();
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        match entry {
            ModelEntry::Spec(spec) => {
                let mut spec = spec.clone();
                if spec.tier.is_empty() {
                    spec.tier = spec.tier_or_default().to_string();
                }
                out.push(spec);
            }
            ModelEntry::Id(id) => {
                let spec = find_spec(&defaults, id).cloned().unwrap_or(ModelSpec {
                    id: id.clone(),
                    tier: "workhorse".to_string(),
                    ..ModelSpec::default()
                });
                out.push(spec);
            }
        }
    }
    out
}

// ── Engine ────────────────────────────────────────────────────────────────────

pub struct BudgetAwareEngine;

impl BudgetAwareEngine {
    pub fn metadata() -> SwarmStrategyMetadata {
        SwarmStrategyMetadata {
            strategy: STRATEGY_ID.to_string(),
            display_name: "Budget-Aware Successive Halving".to_string(),
            description: "Routes each peer to a different cost tier, scores candidates on quality minus normalised cost, and prunes dominated peers mid-flight instead of racing every branch to completion.".to_string(),
            default_concurrency: 4,
            max_concurrency: 16,
            supported_roles: vec![
                "architect".to_string(),
                "implementer".to_string(),
                "optimizer".to_string(),
                "adversarial_fuzzer".to_string(),
                "reviewer".to_string(),
                "explorer".to_string(),
            ],
            telemetry_features: vec![
                "per_peer_model_routing".to_string(),
                "token_cost_accounting".to_string(),
                "successive_halving".to_string(),
                "budget_ceiling".to_string(),
            ],
            supports_dynamic_steering: true,
        }
    }

    /// Formulate peer hypotheses with per-peer model assignment.
    pub fn plan(req: &SwarmPlanRequest) -> SwarmPlanResponse {
        let catalog = resolve_catalog(&req.available_models);
        let budget = normalize_budget(&req.budget);
        let count = req.concurrency.clamp(1, 16);
        let roles = normalize_roles(&req.roles, count);

        let mut eligible: Vec<&ModelSpec> = catalog
            .iter()
            .filter(|m| {
                if m.is_free() && m.trains_on_data && !budget.allow_training_models {
                    return false;
                }
                match req.plugin.as_deref() {
                    Some(plugin) if !plugin.trim().is_empty() => {
                        m.provider.is_empty() || m.provider == plugin
                    }
                    _ => true,
                }
            })
            .collect();

        // A caller-forced model disables tiering but still routes deterministically.
        if let Some(forced) = req.model.as_deref().filter(|m| !m.trim().is_empty()) {
            eligible = catalog.iter().filter(|m| m.id == forced).collect();
            if eligible.is_empty() {
                // Forced model is not in the catalog: use it with unknown pricing.
                eligible = Vec::new();
            }
        }

        let mut hypotheses = Vec::with_capacity(count);

        for (idx, role) in roles.iter().enumerate() {
            let peer_id = (idx + 1) as u32;
            let picked = pick_model(&eligible, &catalog, role, idx, req.model.as_deref(), &budget);

            let (model_id, spec) = match picked {
                Some(spec) => (Some(spec.id.clone()), Some(spec)),
                None => (req.model.clone(), None),
            };

            let expected = spec
                .map(|s| {
                    estimate_usd(
                        s,
                        ASSUMED_PROMPT_TOKENS as u64,
                        ASSUMED_OUTPUT_TOKENS as u64,
                        &budget,
                    )
                })
                .unwrap_or(0.0);

            hypotheses.push(PeerVariantHypothesis {
                peer_id,
                role: role.clone(),
                hypothesis_title: format!("{} (cost tier: {})", role_title(role), tier_label(spec)),
                focus_directive: format!(
                    "Task: {}. Target: {}. {}",
                    req.task,
                    target_hint(&req.target_files),
                    role_directive(role)
                ),
                reasoning_variant: role_variant(role).to_string(),
                tool_categories: role_tool_categories(role),
                model: model_id,
                budget_class: tier_label(spec).to_string(),
                max_steps: role_max_steps(role),
                expected_usd: expected,
            });
        }

        // Downgrade the most expensive peer until the run fits the ceiling.
        if let Some(max) = budget.max_usd {
            let mut guard = 0;
            while estimated_total(&hypotheses) > max && guard < 16 {
                guard += 1;
                if !downgrade_cheapest_win(&mut hypotheses, &eligible, &catalog, &budget) {
                    break;
                }
            }
        }

        let report = BudgetReport {
            estimated_usd: estimated_total(&hypotheses),
            estimated_saved_usd: 0.0,
            max_usd: budget.max_usd,
            over_budget: budget
                .max_usd
                .map(|max| estimated_total(&hypotheses) > max)
                .unwrap_or(false),
            by_peer: hypotheses
                .iter()
                .map(|h| PeerCost {
                    peer_id: h.peer_id,
                    model: h.model.clone().unwrap_or_default(),
                    prompt_tokens: ASSUMED_PROMPT_TOKENS as u64,
                    completion_tokens: ASSUMED_OUTPUT_TOKENS as u64,
                    estimated_usd: h.expected_usd,
                })
                .collect(),
        };

        let probe_secs = req
            .parameters
            .get("probe_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or_else(|| default_probe_secs(count));

        SwarmPlanResponse {
            strategy: STRATEGY_ID.to_string(),
            hypotheses,
            probe_secs,
            prune_after_round: MIN_PRUNE_ROUND,
            estimated_usd: report.estimated_usd,
            budget_report: report,
        }
    }

    /// Score candidates and (during a mid-flight call) select peers to prune.
    pub fn evaluate(req: &SwarmEvaluateRequest) -> SwarmRunResponse {
        let catalog = resolve_catalog(&req.available_models);
        let budget = normalize_budget(&req.budget);

        let mut peers: Vec<PeerCandidateEvaluation> = req
            .candidates
            .iter()
            .map(|c| {
                let mut peer = c.clone();
                let spec = peer
                    .telemetry
                    .model
                    .as_deref()
                    .and_then(|id| find_spec(&catalog, id));
                let (prompt, completion) = token_pair(&peer.telemetry);
                // Recompute cost from the catalog when we can identify the model
                // and have real token telemetry; otherwise trust the caller's
                // estimate rather than zeroing a peer's known spend.
                if let Some(spec) = spec {
                    if prompt > 0 || completion > 0 {
                        peer.estimated_usd = estimate_usd(spec, prompt, completion, &budget);
                    }
                }
                peer
            })
            .collect();

        let max_cost = peers.iter().map(|p| p.estimated_usd).fold(0.0_f64, f64::max);
        let max_latency = peers
            .iter()
            .map(|p| p.telemetry.elapsed_ms)
            .max()
            .unwrap_or(0)
            .max(1) as f64;
        let cost_weight = cost_weight(&budget.prefer);

        for peer in peers.iter_mut() {
            peer.composite_score = score_peer(peer, max_cost, max_latency, cost_weight);
        }

        let report = budget_report(&peers, &budget);

        if req.midflight {
            let prune = select_prune_set(&peers, req.round, &budget);
            return SwarmRunResponse {
                swarm_id: req.swarm_id.clone(),
                strategy: STRATEGY_ID.to_string(),
                status: "running".to_string(),
                winner_peer_id: None,
                winner_role: None,
                winner_hypothesis: None,
                candidates_evaluated: peers.len(),
                merged_files: Vec::new(),
                composite_winner_score: 0.0,
                summary: format!(
                    "mid-flight round {}: {} active, {} prune candidate(s)",
                    req.round,
                    peers.iter().filter(|p| !p.pruned).count(),
                    prune.len()
                ),
                peers,
                prune_peer_ids: prune,
                budget_report: report,
            };
        }

        let winner = peers
            .iter()
            .filter(|p| !p.pruned && p.check_passed && p.test_passed)
            .max_by(|a, b| {
                a.composite_score
                    .partial_cmp(&b.composite_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(b.estimated_usd.partial_cmp(&a.estimated_usd).unwrap_or(std::cmp::Ordering::Equal))
                    .then(b.peer_id.cmp(&a.peer_id))
            })
            .cloned();

        match winner {
            Some(w) => {
                let alternatives = peers.iter().filter(|p| p.peer_id != w.peer_id).count();
                SwarmRunResponse {
                    swarm_id: req.swarm_id.clone(),
                    strategy: STRATEGY_ID.to_string(),
                    status: "completed".to_string(),
                    winner_peer_id: Some(w.peer_id),
                    winner_role: Some(w.role.clone()),
                    winner_hypothesis: Some(w.hypothesis.clone()),
                    candidates_evaluated: peers.len(),
                    merged_files: w.modified_files.clone(),
                    composite_winner_score: w.composite_score,
                    summary: format!(
                        "Peer #{} ({}, {}) won on score {:.1} at ~${:.4} estimated cost; {} alternative(s) evaluated, {} pruned mid-flight.",
                        w.peer_id,
                        w.role,
                        w.telemetry.model.as_deref().unwrap_or("default model"),
                        w.composite_score,
                        w.estimated_usd,
                        alternatives,
                        peers.iter().filter(|p| p.pruned).count()
                    ),
                    peers,
                    prune_peer_ids: Vec::new(),
                    budget_report: report,
                }
            }
            None => SwarmRunResponse {
                swarm_id: req.swarm_id.clone(),
                strategy: STRATEGY_ID.to_string(),
                status: "failed".to_string(),
                winner_peer_id: None,
                winner_role: None,
                winner_hypothesis: None,
                candidates_evaluated: peers.len(),
                merged_files: Vec::new(),
                composite_winner_score: 0.0,
                summary: format!(
                    "No peer passed verification. Estimated spend ~${:.4}{}.",
                    report.estimated_usd,
                    if report.over_budget { " (over budget)" } else { "" }
                ),
                peers,
                prune_peer_ids: Vec::new(),
                budget_report: report,
            },
        }
    }
}

/// Strategy metadata payload (mirrors the SDK shape used by the host UI).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmStrategyMetadata {
    pub strategy: String,
    pub display_name: String,
    pub description: String,
    pub default_concurrency: usize,
    pub max_concurrency: usize,
    pub supported_roles: Vec<String>,
    pub telemetry_features: Vec<String>,
    pub supports_dynamic_steering: bool,
}

// ── Policy helpers ────────────────────────────────────────────────────────────

fn normalize_budget(budget: &BudgetSpec) -> BudgetSpec {
    let mut b = budget.clone();
    if b.prefer.is_empty() {
        b.prefer = default_prefer();
    }
    if !(0.0..=1.0).contains(&b.cached_read_ratio) {
        b.cached_read_ratio = DEFAULT_CACHED_READ_RATIO;
    }
    if b.max_prune_per_round == 0 {
        b.max_prune_per_round = 1;
    }
    b
}

fn cost_weight(prefer: &str) -> f64 {
    match prefer {
        "cost" => 60.0,
        "quality" => 5.0,
        _ => 25.0,
    }
}

fn token_pair(t: &CandidateTelemetry) -> (u64, u64) {
    let prompt = if t.prompt_tokens > 0 {
        t.prompt_tokens
    } else {
        0
    };
    let completion = if t.completion_tokens > 0 {
        t.completion_tokens
    } else {
        t.total_tokens.saturating_sub(prompt)
    };
    (prompt, completion)
}

/// Quality score minus normalised cost/latency penalties.
///
/// The cost term is scaled down to `0.5` at worst so it can break ties without
/// overriding a real verification difference (worth tens of points).
pub fn score_peer(
    peer: &PeerCandidateEvaluation,
    max_cost: f64,
    max_latency: f64,
    cost_weight: f64,
) -> f64 {
    let mut score = 0.0;
    if peer.check_passed {
        score += 50.0;
    }
    if peer.test_passed {
        score += 100.0;
    }
    let fuzz = if peer.pbt_fuzz.target_iterations > 0 {
        peer.pbt_fuzz.passed_iterations as f64 / peer.pbt_fuzz.target_iterations as f64
    } else {
        0.0
    };
    score += fuzz.clamp(0.0, 1.0) * 20.0;
    score -= peer.ast_convergence.overlap_risk.clamp(0.0, 1.0) * 10.0;
    score -= peer.intent_drift.drift_score.clamp(0.0, 1.0) * 40.0;
    if !peer.intent_drift.within_safe_boundary && !peer.status.is_empty() {
        score -= 25.0;
    }
    if !peer.modified_files.is_empty() {
        score += 10.0;
    }
    if peer.pruned {
        score -= 500.0;
    }

    let cost_norm = if max_cost > 0.0 {
        (peer.estimated_usd / max_cost).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let latency_norm = (peer.telemetry.elapsed_ms as f64 / max_latency).clamp(0.0, 1.0);

    score - cost_norm * cost_weight * 0.02 - latency_norm * 0.25
}

fn budget_report(
    peers: &[PeerCandidateEvaluation],
    budget: &BudgetSpec,
) -> BudgetReport {
    let estimated: f64 = peers.iter().map(|p| p.estimated_usd).sum();
    let saved: f64 = peers
        .iter()
        .filter(|p| p.pruned)
        .map(|p| p.estimated_usd)
        .sum();
    BudgetReport {
        estimated_usd: estimated,
        estimated_saved_usd: saved,
        max_usd: budget.max_usd,
        over_budget: budget.max_usd.map(|m| estimated > m).unwrap_or(false),
        by_peer: peers
            .iter()
            .map(|p| PeerCost {
                peer_id: p.peer_id,
                model: p.telemetry.model.clone().unwrap_or_default(),
                prompt_tokens: p.telemetry.prompt_tokens,
                completion_tokens: p.telemetry.completion_tokens,
                estimated_usd: p.estimated_usd,
            })
            .collect(),
    }
}

/// Successive-halving prune policy.
///
/// Deterministic and deliberately conservative:
/// - never prunes before `MIN_PRUNE_ROUND`
/// - never prunes below one remaining active peer
/// - never prunes a peer that has passed verification while another has not
/// - prunes at most `budget.max_prune_per_round` peers per round
/// - prefers candidates with no file progress, then the lowest score
pub fn select_prune_set(
    peers: &[PeerCandidateEvaluation],
    round: u32,
    budget: &BudgetSpec,
) -> Vec<u32> {
    if round < MIN_PRUNE_ROUND {
        return Vec::new();
    }
    let running: Vec<&PeerCandidateEvaluation> = peers
        .iter()
        .filter(|p| !p.pruned && p.status != "passed" && p.status != "failed")
        .collect();
    if running.len() <= 1 {
        return Vec::new();
    }

    // A verified peer is never pruned while unverified peers are still running.
    let any_verified = running.iter().any(|p| p.check_passed);
    let mut candidates: Vec<&PeerCandidateEvaluation> = running
        .iter()
        .copied()
        .filter(|p| !(any_verified && p.check_passed))
        .filter(|p| p.modified_files.is_empty() || p.composite_score < 0.0)
        .collect();

    if candidates.is_empty() {
        return Vec::new();
    }

    candidates.sort_by(|a, b| {
        a.composite_score
            .partial_cmp(&b.composite_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.estimated_usd.partial_cmp(&a.estimated_usd).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.peer_id.cmp(&b.peer_id))
    });

    // Keep at least one running peer alive.
    let max_by_survival = running.len().saturating_sub(1);
    let limit = budget
        .max_prune_per_round
        .min(max_by_survival)
        .min(candidates.len());

    candidates.iter().take(limit).map(|p| p.peer_id).collect()
}

fn normalize_roles(roles: &[String], count: usize) -> Vec<String> {
    let default = ["implementer", "optimizer", "adversarial_fuzzer", "architect"];
    if roles.is_empty() {
        (0..count).map(|i| default[i % default.len()].to_string()).collect()
    } else {
        (0..count)
            .map(|i| roles[i % roles.len()].clone())
            .collect()
    }
}

fn target_hint(target_files: &[String]) -> String {
    if target_files.is_empty() {
        "workspace".to_string()
    } else {
        target_files.join(", ")
    }
}

fn role_title(role: &str) -> &'static str {
    match role {
        "architect" => "Modular Decoupled",
        "optimizer" => "High-Throughput Lock-Free",
        "adversarial_fuzzer" => "Defensive & Invariant-Hardened",
        "reviewer" => "Correctness Review",
        "explorer" => "Read-Only Recon",
        _ => "Minimal & Surgical",
    }
}

fn role_directive(role: &str) -> &'static str {
    match role {
        "architect" => "Focus on architectural separation of concerns and extensible abstractions. Do not chase micro-optimisations.",
        "optimizer" => "Focus on zero-allocation, lock-free concurrency and algorithmic throughput without changing public interfaces.",
        "adversarial_fuzzer" => "Focus on invariant safety, boundary checks, and failure modes. Prefer finding breakage over adding features.",
        "reviewer" => "Review only. Report findings with file and line references. Do not edit files.",
        "explorer" => "Search and read only. Return a compact map of the relevant code, not file dumps.",
        _ => "Focus strictly on minimal edits preserving existing interfaces and types.",
    }
}

fn role_variant(role: &str) -> &'static str {
    match role {
        "architect" | "reviewer" => "high",
        "optimizer" | "implementer" => "medium",
        _ => "low",
    }
}

fn role_tool_categories(role: &str) -> Vec<String> {
    match role {
        "reviewer" | "explorer" => vec!["read".to_string(), "verify".to_string()],
        "architect" => vec!["read".to_string(), "verify".to_string(), "peer".to_string()],
        _ => vec!["read".to_string(), "write".to_string(), "verify".to_string()],
    }
}

fn role_max_steps(role: &str) -> Option<u32> {
    match role {
        "explorer" => Some(12),
        "adversarial_fuzzer" => Some(20),
        "reviewer" => Some(15),
        _ => Some(40),
    }
}

fn role_prefers(role: &str) -> &'static [&'static str] {
    match role {
        // Reasoning-heavy roles want the best model the budget allows.
        "architect" | "reviewer" => &["premium", "workhorse", "bulk", "free"],
        // Cheapest useful peer for grunt work.
        "adversarial_fuzzer" | "explorer" => &["free", "bulk", "workhorse", "premium"],
        _ => &["bulk", "free", "workhorse", "premium"],
    }
}

fn tier_label(spec: Option<&ModelSpec>) -> &'static str {
    match spec {
        Some(s) => match tier_rank(s) {
            0 => "free",
            1 => "bulk",
            2 => "workhorse",
            _ => "premium",
        },
        None => "default",
    }
}

/// Pick a model for a role, preferring the role's tier and rotating within a
/// tier so parallel peers do not all land on the same model.
fn pick_model<'a>(
    eligible: &[&'a ModelSpec],
    _catalog: &'a [ModelSpec],
    role: &str,
    idx: usize,
    forced: Option<&str>,
    budget: &BudgetSpec,
) -> Option<&'a ModelSpec> {
    if let Some(forced) = forced {
        return eligible.iter().copied().find(|m| m.id == forced);
    }
    if eligible.is_empty() {
        return None;
    }

    for tier in role_prefers(role) {
        let mut pool: Vec<&ModelSpec> = eligible
            .iter()
            .copied()
            .filter(|m| m.tier_or_default() == *tier)
            .collect();
        if pool.is_empty() {
            continue;
        }
        pool.sort_by(|a, b| {
            a.effective_input_rate()
                .partial_cmp(&b.effective_input_rate())
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.id.cmp(&b.id))
        });
        // With a hard ceiling, never start a role above the cheapest tier that fits.
        let chosen = pool[idx % pool.len()];
        if let Some(max) = budget.max_usd {
            let est = estimate_usd(
                chosen,
                ASSUMED_PROMPT_TOKENS as u64,
                ASSUMED_OUTPUT_TOKENS as u64,
                budget,
            );
            if est > max && *tier != "free" {
                continue;
            }
        }
        return Some(chosen);
    }

    eligible
        .iter()
        .copied()
        .min_by(|a, b| {
            a.effective_input_rate()
                .partial_cmp(&b.effective_input_rate())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

fn estimated_total(hypotheses: &[PeerVariantHypothesis]) -> f64 {
    hypotheses.iter().map(|h| h.expected_usd).sum()
}

/// Replace the most expensive hypothesis with a cheaper eligible model.
/// Returns `false` when nothing can be downgraded further.
fn downgrade_cheapest_win(
    hypotheses: &mut [PeerVariantHypothesis],
    eligible: &[&ModelSpec],
    _catalog: &[ModelSpec],
    budget: &BudgetSpec,
) -> bool {
    let mut order: Vec<usize> = (0..hypotheses.len()).collect();
    order.sort_by(|&a, &b| {
        hypotheses[b]
            .expected_usd
            .partial_cmp(&hypotheses[a].expected_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    for i in order {
        let current_tier = hypotheses[i].budget_class.clone();
        let current_cost = hypotheses[i].expected_usd;
        let cheaper = eligible
            .iter()
            .copied()
            .filter(|m| {
                let est = estimate_usd(
                    m,
                    ASSUMED_PROMPT_TOKENS as u64,
                    ASSUMED_OUTPUT_TOKENS as u64,
                    budget,
                );
                est < current_cost && tier_label(Some(m)) != current_tier
            })
            .min_by(|a, b| {
                let ea = estimate_usd(a, ASSUMED_PROMPT_TOKENS as u64, ASSUMED_OUTPUT_TOKENS as u64, budget);
                let eb = estimate_usd(b, ASSUMED_PROMPT_TOKENS as u64, ASSUMED_OUTPUT_TOKENS as u64, budget);
                ea.partial_cmp(&eb).unwrap_or(std::cmp::Ordering::Equal)
            });
        if let Some(spec) = cheaper {
            hypotheses[i].model = Some(spec.id.clone());
            hypotheses[i].budget_class = tier_label(Some(spec)).to_string();
            hypotheses[i].expected_usd = estimate_usd(
                spec,
                ASSUMED_PROMPT_TOKENS as u64,
                ASSUMED_OUTPUT_TOKENS as u64,
                budget,
            );
            return true;
        }
    }
    false
}

fn default_probe_secs(count: usize) -> u64 {
    // Small swarms can afford a longer probe; large swarms benefit from earlier cuts.
    match count {
        0..=2 => 20,
        3..=4 => 15,
        _ => 10,
    }
}

// ── WASM Hook Exports ─────────────────────────────────────────────────────────

/// Hook: `basalt_swarm_metadata() -> u64`
#[no_mangle]
pub extern "C" fn basalt_swarm_metadata() -> u64 {
    let bytes = serde_json::to_vec(&BudgetAwareEngine::metadata()).unwrap_or_default();
    pack_output(bytes)
}

/// Hook: `basalt_swarm_plan(req_ptr, req_len) -> u64`
#[no_mangle]
pub extern "C" fn basalt_swarm_plan(req_ptr: *const u8, req_len: usize) -> u64 {
    if req_ptr.is_null() || req_len == 0 {
        return 0;
    }
    let slice = unsafe { core::slice::from_raw_parts(req_ptr, req_len) };
    let Ok(req) = serde_json::from_slice::<SwarmPlanRequest>(slice) else {
        return 0;
    };
    let bytes = match serde_json::to_vec(&BudgetAwareEngine::plan(&req)) {
        Ok(b) => b,
        Err(_) => return 0,
    };
    pack_output(bytes)
}

/// Hook: `basalt_swarm_steer`. Pure policy: given the current active set, return
/// which peers a caller may prune. The host is responsible for cancellation.
#[no_mangle]
pub extern "C" fn basalt_swarm_steer(req_ptr: *const u8, req_len: usize) -> u64 {
    if req_ptr.is_null() || req_len == 0 {
        return 0;
    }
    let slice = unsafe { core::slice::from_raw_parts(req_ptr, req_len) };
    let Ok(req) = serde_json::from_slice::<SwarmEvaluateRequest>(slice) else {
        return 0;
    };
    let resp = BudgetAwareEngine::evaluate(&SwarmEvaluateRequest {
        midflight: true,
        ..req
    });
    let bytes = serde_json::to_vec(&resp).unwrap_or_default();
    pack_output(bytes)
}

/// Hook: `basalt_swarm_evaluate(req_ptr, req_len) -> u64`
#[no_mangle]
pub extern "C" fn basalt_swarm_evaluate(req_ptr: *const u8, req_len: usize) -> u64 {
    if req_ptr.is_null() || req_len == 0 {
        return 0;
    }
    let slice = unsafe { core::slice::from_raw_parts(req_ptr, req_len) };
    let Ok(req) = serde_json::from_slice::<SwarmEvaluateRequest>(slice) else {
        return 0;
    };
    let bytes = match serde_json::to_vec(&BudgetAwareEngine::evaluate(&req)) {
        Ok(b) => b,
        Err(_) => return 0,
    };
    pack_output(bytes)
}

/// Hook: `basalt_capability_handle(cap_ptr, cap_len, req_ptr, req_len) -> i64`
#[no_mangle]
pub extern "C" fn basalt_capability_handle(
    cap_ptr: *const u8,
    cap_len: usize,
    req_ptr: *const u8,
    req_len: usize,
) -> i64 {
    if cap_ptr.is_null() || cap_len == 0 {
        return pack_error(-1001);
    }
    let cap_slice = unsafe { core::slice::from_raw_parts(cap_ptr, cap_len) };
    let Ok(cap_str) = core::str::from_utf8(cap_slice) else {
        return pack_error(-1002);
    };

    let req_bytes = if !req_ptr.is_null() && req_len > 0 {
        unsafe { core::slice::from_raw_parts(req_ptr, req_len) }
    } else {
        &[]
    };

    match cap_str.trim() {
        "swarm-strategy:metadata" | "swarm:metadata" => {
            let bytes = serde_json::to_vec(&BudgetAwareEngine::metadata()).unwrap_or_default();
            pack_success(bytes)
        }
        "swarm-strategy:plan" | "swarm:plan" => {
            let Ok(req) = serde_json::from_slice::<SwarmPlanRequest>(req_bytes) else {
                return pack_error(-1003);
            };
            let bytes = match serde_json::to_vec(&BudgetAwareEngine::plan(&req)) {
                Ok(b) => b,
                Err(_) => return pack_error(-1003),
            };
            pack_success(bytes)
        }
        "swarm-strategy:evaluate" | "swarm:evaluate" | "swarm-strategy:synthesize" | "swarm:synthesize" => {
            let Ok(req) = serde_json::from_slice::<SwarmEvaluateRequest>(req_bytes) else {
                return pack_error(-1005);
            };
            let bytes = match serde_json::to_vec(&BudgetAwareEngine::evaluate(&req)) {
                Ok(b) => b,
                Err(_) => return pack_error(-1005),
            };
            pack_success(bytes)
        }
        "swarm-strategy:steer" | "swarm:steer" => {
            let Ok(req) = serde_json::from_slice::<SwarmEvaluateRequest>(req_bytes) else {
                return pack_error(-1004);
            };
            let resp = BudgetAwareEngine::evaluate(&SwarmEvaluateRequest {
                midflight: true,
                ..req
            });
            let bytes = serde_json::to_vec(&resp).unwrap_or_default();
            pack_success(bytes)
        }
        _ => pack_error(-1000),
    }
}

// ── Unit Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_req(concurrency: usize) -> SwarmPlanRequest {
        SwarmPlanRequest {
            task: "Implement a rate limiter".to_string(),
            concurrency,
            target_files: vec!["src/limiter.rs".to_string()],
            roles: Vec::new(),
            plugin: Some("opencode".to_string()),
            model: None,
            available_models: Vec::new(),
            budget: BudgetSpec::default(),
            parameters: HashMap::new(),
        }
    }

    fn candidate(peer_id: u32, check: bool, test: bool, cost: f64, files: &[&str]) -> PeerCandidateEvaluation {
        PeerCandidateEvaluation {
            peer_id,
            role: "implementer".to_string(),
            hypothesis: format!("peer-{peer_id}"),
            status: if check && test { "passed".to_string() } else { "running".to_string() },
            modified_files: files.iter().map(|s| s.to_string()).collect(),
            check_passed: check,
            test_passed: test,
            estimated_usd: cost,
            telemetry: CandidateTelemetry {
                prompt_tokens: 1_000,
                completion_tokens: 100,
                total_tokens: 1_100,
                elapsed_ms: 1_000,
                model: None,
            },
            ..PeerCandidateEvaluation::default()
        }
    }

    #[test]
    fn test_metadata_contract() {
        let meta = BudgetAwareEngine::metadata();
        assert_eq!(meta.strategy, "budget_aware");
        assert!(meta.supports_dynamic_steering);
        assert!(meta.supported_roles.contains(&"architect".to_string()));
        assert!(meta.telemetry_features.contains(&"successive_halving".to_string()));
    }

    #[test]
    fn test_default_catalog_is_sane() {
        let catalog = default_catalog();
        assert!(catalog.len() >= 20);
        let deepseek = find_spec(&catalog, "opencode-go/deepseek-v4.1-flash").expect("deepseek present");
        assert_eq!(deepseek.tier_or_default(), "bulk");
        assert_eq!(deepseek.bucket_usd, 80.0);
        let haiku = find_spec(&catalog, "opencode-go/claude-haiku-5-5").expect("haiku present");
        assert_eq!(haiku.tier_or_default(), "premium");
        assert_eq!(haiku.price_cliff_tokens, Some(100_000));
        assert_eq!(haiku.cliff_multiplier, 5.0);
    }

    #[test]
    fn test_unknown_priced_model_is_not_treated_as_free() {
        let catalog = resolve_catalog(&[ModelEntry::Id("custom/unpriced-model".to_string())]);
        assert_eq!(catalog.len(), 1);
        assert!(!catalog[0].is_free(), "unknown pricing must not count as free");
        assert!(!catalog[0].has_pricing());

        let budget = BudgetSpec::default();
        let unknown_cost = estimate_usd(&catalog[0], 1_000_000, 0, &budget);
        assert!(
            unknown_cost > 0.05,
            "unknown pricing must not estimate near zero: {unknown_cost}"
        );

        let bundled = default_catalog();
        let known = find_spec(&bundled, "opencode-go/deepseek-v4.1-flash").unwrap();
        let known_cost = estimate_usd(known, 1_000_000, 0, &budget);
        assert!(
            unknown_cost > known_cost,
            "unpriced model must not rank as cheaper than a known bulk model"
        );
    }

    #[test]
    fn test_discovered_ids_resolve_to_bundled_pricing() {
        // This is the shape the host forwards from `opencode models`: bare ids.
        let ids = [
            "opencode-go/deepseek-v4.1-flash",
            "opencode-go/glm-5.3",
            "opencode-go/beam-free",
            "opencode-go/a-model-added-after-release",
        ];
        let entries: Vec<ModelEntry> = ids.iter().map(|s| ModelEntry::Id(s.to_string())).collect();
        let catalog = resolve_catalog(&entries);
        assert_eq!(catalog.len(), 4);
        // Known ids keep their bundled pricing and tiers.
        assert_eq!(catalog[0].tier_or_default(), "bulk");
        assert_eq!(catalog[1].tier_or_default(), "premium");
        assert!(catalog[2].is_free());
        // Unknown ids stay eligible but are priced conservatively.
        assert!(!catalog[3].is_free());
        assert!(!catalog[3].has_pricing());

        let mut req = plan_req(4);
        req.available_models = entries;
        let resp = BudgetAwareEngine::plan(&req);
        assert_eq!(resp.hypotheses.len(), 4);
        assert!(resp.hypotheses.iter().all(|h| h.model.is_some()));
    }

    #[test]
    fn test_plan_routes_by_cost_tier() {
        let resp = BudgetAwareEngine::plan(&plan_req(4));
        assert_eq!(resp.strategy, "budget_aware");
        assert_eq!(resp.hypotheses.len(), 4);
        let tiers: Vec<&str> = resp.hypotheses.iter().map(|h| h.budget_class.as_str()).collect();
        assert_eq!(tiers, vec!["bulk", "bulk", "free", "premium"]);
        // Every peer got a concrete model.
        assert!(resp.hypotheses.iter().all(|h| h.model.is_some()));
        // Parallel peers do not all land on the same model.
        let distinct: std::collections::HashSet<&str> =
            resp.hypotheses.iter().filter_map(|h| h.model.as_deref()).collect();
        assert!(distinct.len() >= 3);
        assert!(resp.probe_secs > 0);
        assert_eq!(resp.prune_after_round, MIN_PRUNE_ROUND);
    }

    #[test]
    fn test_plan_excludes_training_models_unless_allowed() {
        let mut req = plan_req(3);
        req.roles = vec!["adversarial_fuzzer".to_string()];
        let resp = BudgetAwareEngine::plan(&req);
        assert!(resp
            .hypotheses
            .iter()
            .all(|h| h.model.as_deref() != Some("opencode-go/iceman-contributor-free")));

        req.budget.allow_training_models = true;
        let allowed = BudgetAwareEngine::plan(&req);
        assert!(allowed.hypotheses.iter().all(|h| h.model.is_some()));
    }

    #[test]
    fn test_plan_honours_forced_model() {
        let mut req = plan_req(3);
        req.model = Some("opencode-go/glm-5.3".to_string());
        let resp = BudgetAwareEngine::plan(&req);
        assert!(resp
            .hypotheses
            .iter()
            .all(|h| h.model.as_deref() == Some("opencode-go/glm-5.3")));
        assert!(resp.hypotheses.iter().all(|h| h.budget_class == "premium"));
    }

    #[test]
    fn test_plan_respects_budget_ceiling() {
        let mut req = plan_req(4);
        req.budget.max_usd = Some(0.01);
        let resp = BudgetAwareEngine::plan(&req);
        assert!(!resp.budget_report.over_budget, "downgrade should fit the ceiling");
        assert!(resp.estimated_usd <= 0.01 + 1e-9);
    }

    #[test]
    fn test_plan_accepts_bare_model_ids() {
        let mut req = plan_req(2);
        req.available_models = vec![ModelEntry::Id("opencode-go/deepseek-v4.1-flash".to_string())];
        let resp = BudgetAwareEngine::plan(&req);
        assert!(resp
            .hypotheses
            .iter()
            .all(|h| h.model.as_deref() == Some("opencode-go/deepseek-v4.1-flash")));
        assert!(resp.estimated_usd > 0.0);
    }

    #[test]
    fn test_cost_model_applies_cache_and_peak() {
        let catalog = default_catalog();
        let ds = find_spec(&catalog, "opencode-go/deepseek-v4.1-flash").unwrap();
        let budget = BudgetSpec::default();
        let off_peak = estimate_usd(ds, 1_000_000, 0, &budget);
        // 150k fresh @ 0.15 + 850k cached @ 0.003
        assert!((off_peak - 0.02505).abs() < 1e-9, "got {off_peak}");

        let mut peak = BudgetSpec::default();
        peak.peak_pricing = true;
        let peak_cost = estimate_usd(ds, 1_000_000, 0, &peak);
        assert!((peak_cost - 0.0501).abs() < 1e-9, "got {peak_cost}");
    }

    #[test]
    fn test_cost_model_applies_context_cliff() {
        let catalog = default_catalog();
        let haiku = find_spec(&catalog, "opencode-go/claude-haiku-5-5").unwrap();
        let budget = BudgetSpec::default();
        let small = estimate_usd(haiku, 50_000, 0, &budget);
        let large = estimate_usd(haiku, 150_000, 0, &budget);
        // Same token volume scaled 3x, but the large call crosses the 100k cliff
        // and is 5x rated, so it must cost far more than 3x.
        assert!(large > small * 10.0, "small={small} large={large}");
    }

    #[test]
    fn test_cost_breaks_quality_ties() {
        let cheap = candidate(1, true, true, 0.001, &["a.rs"]);
        let pricey = candidate(2, true, true, 1.0, &["a.rs"]);
        let cheap_score = score_peer(&cheap, 1.0, 1_000.0, cost_weight("balanced"));
        let pricey_score = score_peer(&pricey, 1.0, 1_000.0, cost_weight("balanced"));
        assert!(cheap_score > pricey_score);
    }

    #[test]
    fn test_evaluate_prefers_cheaper_winner_on_tie() {
        let req = SwarmEvaluateRequest {
            task: "t".to_string(),
            swarm_id: "swm-test".to_string(),
            candidates: vec![
                candidate(1, true, true, 1.0, &["a.rs"]),
                candidate(2, true, true, 0.001, &["a.rs"]),
            ],
            available_models: Vec::new(),
            budget: BudgetSpec::default(),
            midflight: false,
            round: 0,
        };
        let resp = BudgetAwareEngine::evaluate(&req);
        assert_eq!(resp.status, "completed");
        assert_eq!(resp.winner_peer_id, Some(2));
    }

    #[test]
    fn test_evaluate_fails_without_verified_peer() {
        let req = SwarmEvaluateRequest {
            task: "t".to_string(),
            swarm_id: "swm-test".to_string(),
            candidates: vec![candidate(1, false, false, 0.1, &[]), candidate(2, false, true, 0.1, &[])],
            available_models: Vec::new(),
            budget: BudgetSpec::default(),
            midflight: false,
            round: 0,
        };
        let resp = BudgetAwareEngine::evaluate(&req);
        assert_eq!(resp.status, "failed");
        assert!(resp.winner_peer_id.is_none());
    }

    #[test]
    fn test_no_pruning_before_min_round() {
        let peers = vec![
            candidate(1, false, false, 0.01, &["a.rs"]),
            candidate(2, false, false, 0.01, &[]),
        ];
        assert!(select_prune_set(&peers, 1, &BudgetSpec::default()).is_empty());
        assert!(!select_prune_set(&peers, 2, &BudgetSpec::default()).is_empty());
    }

    #[test]
    fn test_pruning_keeps_at_least_one_peer() {
        let peers = vec![candidate(1, false, false, 0.01, &[])];
        assert!(select_prune_set(&peers, 3, &BudgetSpec::default()).is_empty());
    }

    #[test]
    fn test_pruning_respects_per_round_cap() {
        let peers = vec![
            candidate(1, false, false, 0.01, &[]),
            candidate(2, false, false, 0.01, &[]),
            candidate(3, false, false, 0.01, &[]),
            candidate(4, false, false, 0.01, &[]),
        ];
        let pruned = select_prune_set(&peers, 3, &BudgetSpec::default());
        assert_eq!(pruned.len(), 1);

        let mut budget = BudgetSpec::default();
        budget.max_prune_per_round = 2;
        let pruned = select_prune_set(&peers, 3, &budget);
        assert_eq!(pruned.len(), 2);
    }

    #[test]
    fn test_pruning_spares_verified_peer() {
        let mut verified = candidate(1, true, false, 0.01, &["a.rs"]);
        verified.status = "running".to_string();
        let mut idle = candidate(2, false, false, 0.01, &[]);
        idle.status = "running".to_string();
        let peers = vec![verified, idle];
        let pruned = select_prune_set(&peers, 3, &BudgetSpec::default());
        assert_eq!(pruned, vec![2]);
    }

    #[test]
    fn test_midflight_returns_prunes_not_winner() {
        let req = SwarmEvaluateRequest {
            task: "t".to_string(),
            swarm_id: "swm-test".to_string(),
            candidates: vec![
                candidate(1, false, false, 0.01, &[]),
                candidate(2, false, false, 0.01, &[]),
            ],
            available_models: Vec::new(),
            budget: BudgetSpec::default(),
            midflight: true,
            round: 3,
        };
        let resp = BudgetAwareEngine::evaluate(&req);
        assert_eq!(resp.status, "running");
        assert!(resp.winner_peer_id.is_none());
        assert_eq!(resp.prune_peer_ids.len(), 1);
    }

    #[test]
    fn test_plan_response_serializes_as_envelope() {
        let resp = BudgetAwareEngine::plan(&plan_req(3));
        let value = serde_json::to_value(&resp).unwrap();
        assert!(value.get("hypotheses").is_some());
        assert!(value.get("probe_secs").is_some());
        assert!(value.get("budget_report").is_some());
        assert!(value["hypotheses"][0].get("model").is_some());
    }

    #[test]
    fn test_plan_request_tolerates_unknown_and_missing_fields() {
        let raw = serde_json::json!({
            "task": "x",
            "available_models": ["opencode-go/glm-5.3-flash"],
            "unknown_future_field": 42
        });
        let req: SwarmPlanRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.concurrency, 3);
        assert_eq!(req.available_models.len(), 1);
        assert!(req.budget.max_usd.is_none());
    }
}

