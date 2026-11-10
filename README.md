# basalt-plugin-swarm-budget

Budget-aware Swarm Strategy Plugin for [Basalt](https://github.com/adevcorn/basalt): **successive-halving orchestration with per-peer model routing and cost telemetry**.

Where `swarm-competitive` races N peers to completion on one model, this strategy treats a swarm as a cost-aware search.

---

## 🚀 Capabilities & Metadata

- **Plugin Name**: `swarm-budget`
- **Hook Flags**: `CAP_SWARM_STRATEGY` (`1 << 23`, `0x00800000`) | `CAP_CAPABILITY_HANDLE` (`1 << 17`)
- **Provides**: `swarm-strategy:budget_aware`, `swarm-strategy:budget-aware`, `swarm:successive-halving`
- **Optional requires**: `agent-launcher@opencode/v1` (soft — see [Model discovery](#model-discovery); it never gates activation)
- **ABI Version**: `1`
- **Target**: `wasm32-unknown-unknown`

---

## ✨ What it does

### 1. Per-peer model routing

Each peer gets its own `model`, chosen from a cost-tier catalog:

| Tier | Typical roles | Rationale |
| --- | --- | --- |
| `free` | `adversarial_fuzzer`, `explorer` | Zero marginal cost; a failing fuzzer is still informative |
| `bulk` | `implementer`, `optimizer` | Cheap, large allowance, no context-price cliff |
| `workhorse` | orchestration, cross-cutting edits | Enough headroom to daily-drive |
| `premium` | `architect`, `reviewer` | Scarce allowance; reserve for reasoning-heavy work |

Peers rotate within a tier, so parallel workers do not all land on the same model. When `budget.max_usd` is set, the planner downgrades the most expensive peer until the run fits the ceiling.

`BudgetSpec.allow_training_models` defaults to `false`, so free models whose provider trains on request data are excluded unless you opt in.

### 2. Successive halving

Mid-flight evaluation returns `prune_peer_ids`. The host cancels those peers instead of paying for a full race. The policy is deliberately conservative:

- never prunes before round **2**;
- never prunes every remaining peer;
- never prunes a peer that has passed verification while another has not;
- prunes at most `budget.max_prune_per_round` peers (default `1`) per round.

Pruning only happens when the host calls `swarm-strategy:evaluate` with `"midflight": true` during its poll loop. Hosts without that support still get correct final scoring — they simply race every peer to completion.

### 3. Cost scoring

`composite_score = quality - normalised_cost_penalty - latency_penalty` where quality is built from verification, PBT fuzz ratio, AST overlap and intent drift. The cost term is bounded at `0.5` points so it can break ties without overriding a real verification difference (worth tens of points).

Cost is estimated per peer from token telemetry:

```text
fresh   = prompt × (1 - cached_read_ratio)
cached  = prompt × cached_read_ratio
usd     = (fresh × input_rate + cached × cached_rate
           + fresh × cache_write_rate + completion × output_rate) / 1e6
```

`cached_read_ratio` defaults to `0.85` (agentic loops resend a stable prefix). Peak-hour and context-cliff multipliers are applied when configured.

---

## 📥 Inputs

### Model discovery

Model enumeration belongs to the **agent launcher**, not to this plugin: a WASM strategy cannot spawn `opencode models`, and hard-wiring the strategy to one launcher would break `codex` / `copilot` peers. So the chain is:

```
swarm_run(available_models?)        host honours explicit catalog
        │
        ▼  when omitted
fetch_agent_models(launcher)        e.g. `opencode models` → 53 ids
        │
        ▼  available_models (bare ids)
strategy plan                        ids matched against the bundled catalog,
                                     unknown ids priced conservatively
```

Consequences:

- **Known ids keep their published pricing and tier.** `opencode-go/glm-5.3` is still rated `premium` with a $15 bucket.
- **Unknown ids stay eligible but are never treated as free.** A model the bundled catalog has never heard of is estimated at `$1.00/M` input / `$4.00/M` output, so it cannot win on cost by accident. Set `free: true` on a spec if a model really is unmetered.
- **Discovery failure is invisible.** If the launcher cannot enumerate models, the host forwards an empty catalog and the strategy falls back to its bundled table. No swarm fails because `models` was slow.
- **`discover_models: false`** skips the subprocess and uses the bundled catalog directly.

The `optional_requires: agent-launcher@opencode/v1` declaration is documentation, not control flow: `requires` would gate activation and break non-opencode peers, while `optional_requires` keeps the pairing visible without disabling the plugin.

### `available_models`

The host forwards this catalog verbatim. Items may be a model id string or a spec object:

```jsonc
{
  "available_models": [
    "opencode-go/deepseek-v4.1-flash",
    {
      "id": "opencode-go/glm-5.3",
      "provider": "opencode",
      "input_per_mtok": 1.40,
      "output_per_mtok": 4.40,
      "cached_read_per_mtok": 0.26,
      "cache_write_per_mtok": 0.0,
      "price_cliff_tokens": null,
      "cliff_multiplier": 1.0,
      "bucket_usd": 15.0,
      "peak_multiplier": 1.0,
      "tier": "premium",
      "free": false,
      "trains_on_data": false
    }
  ]
}
```

Omit it to use the bundled OpenCode Go catalog (prices as published for the Go plan). A bare id string resolves its pricing from that catalog; an id the catalog does not know is kept but priced conservatively rather than treated as free.

### `budget`

```jsonc
{
  "budget": {
    "max_usd": 0.50,
    "prefer": "balanced",
    "allow_training_models": false,
    "peak_pricing": false,
    "cached_read_ratio": 0.85,
    "max_prune_per_round": 1
  }
}
```

`prefer` is `cost` | `balanced` | `quality` and controls the cost-penalty weight.

---

## 🧪 Example

```jsonc
{
  "tool": "swarm_run",
  "task": "Add a token bucket rate limiter",
  "strategy": "budget_aware",
  "concurrency": 4,
  "target_files": ["src/limiter.rs"],
  "available_models": [
    "opencode-go/deepseek-v4.1-flash",
    "opencode-go/glm-5.3-flash",
    "opencode-go/beam-free",
    "opencode-go/glm-5.3"
  ],
  "budget": { "max_usd": 1.0, "prefer": "balanced" }
}
```

Result includes per-peer `model`, `telemetry`, `estimated_usd`, `pruned`, plus a top-level `budget_report` and `pruned_peers`.

---

## 🏗️ Building & Installing

```bash
cargo test
cargo build --target wasm32-unknown-unknown --release
cp target/wasm32-unknown-unknown/release/basalt_plugin_swarm_budget.wasm \
   ~/.config/basalt/plugins/swarm_budget.wasm
```

Then call `swarm_run` with `"strategy": "budget_aware"` (or `"swarm-budget"`).

---

## 📌 Host requirements

- **Model discovery** requires a host that resolves `available_models` from the selected launcher (`resolve_available_models` in `core/src/session_mcp/swarm.rs`). Without it the plugin simply uses its bundled catalog.
- **Per-peer model routing** requires a host that reads `model` from each hypothesis (`resolve_peer_model`). Older hosts ignore the field and fall back to the run-level `model` argument — the plan still works, it just becomes homogeneous.
- **Mid-flight pruning** requires a host that calls `swarm-strategy:evaluate` with `midflight: true` during its poll loop and honours `prune_peer_ids`.
- **Cost telemetry** requires the host to attach `telemetry` to each candidate. Without it the plugin trusts caller-supplied `estimated_usd` values.

---

## 📄 License

Licensed under the Apache-2.0 / MIT licenses to match the Basalt ecosystem.
