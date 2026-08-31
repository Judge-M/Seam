# Seam Roadmap

The near-term goal is not to accumulate features. It is to validate whether Seam's minimal separations actually produce better agent behavior, cost, context discipline and security.

## Phase 0 — Make the Prototype Executable — **done**

- [x] install/use a Rust toolchain in CI;
- [x] `cargo fmt --check`;
- [x] `cargo check --workspace`;
- [x] `cargo clippy --workspace --all-targets -- -D warnings`;
- [x] `cargo test --workspace`;
- [x] fix the current prototype until the workspace is compiler-verified;
- [x] add CI for every pull request.

Still open from this phase, deliberately deferred to the phases that own them: broker
cost/quota/rate ceilings (Phase 2) and a real isolation boundary (Phase 3).

## Phase 0.5 — Pre-Benchmark Hardening — **done**

Prerequisites for Phase 1 measurements being meaningful:

- [x] bounded worker context: windowed file reads, capped command output, capped
      per-observation and per-window budgets;
- [x] worker failure semantics: task failure is a `failed` report, not a runtime error;
- [x] broker authority resolved from a trusted source rather than supplied by the caller;
- [x] active-work context for the orchestrator's own control plane;
- [x] ticket/conversation ownership enforced at the API seam;
- [x] provider identity removed from the worker-visible result.

## Phase 1 — Worker Runtime Baseline

Implement and benchmark the minimal generic worker:

- ticket loading;
- task-local context;
- read/write/search/patch workspace operations;
- sandboxed command execution;
- local Git;
- artifacts;
- `broker.request()`;
- structured completion/block/failure result.

Measure:

- prompt/context size;
- turns per task;
- completion rate;
- latency;
- token cost;
- failure modes.

## Phase 2 — Capability Broker Baseline

Implement a minimal end-to-end capability path:

```text
worker natural-language request
→ SLM logical capability translation
→ validation
→ authorization
→ deterministic provider selection
→ provider invocation
→ normalized result
```

Start with a deliberately small capability catalog.

Measure:

- capability-selection accuracy;
- argument-construction accuracy;
- SLM size vs. reliability;
- unnecessary SLM invocation rate;
- cost and latency;
- recovery behavior.

## Phase 3 — Real Sandbox

Replace prototype path/process restrictions with a real isolation boundary.

Required defaults:

- no external network;
- no host filesystem;
- no ambient credentials;
- resource limits;
- disposable workspace;
- broker-only external access.

## Phase 4 — Orchestration Kernel

Connect:

- conversation persistence;
- memory retrieval/admission policy;
- model gateway;
- ticket lifecycle;
- worker completion events;
- bounded result injection;
- context compaction/assembly.

The orchestrator must remain tool-blind.

## Phase 5 — Durable Execution

Integrate a durable workflow/runtime rather than recreating one.

Evaluate at minimum:

- Restate;
- DBOS;
- Temporal only if the additional complexity is justified.

Requirements:

- crash-safe ticket dispatch;
- retry semantics;
- wait/resume;
- cancellation;
- external completion events;
- durable worker status.

## Phase 6 — Memory Backend Evaluation

Benchmark memory services independently of Seam policy.

Questions:

- retrieval quality under strict token budgets;
- temporal/project memory behavior;
- self-hosting requirements;
- privacy characteristics;
- cost;
- deterministic namespace controls.

Seam should retain control of memory admission and context-injection policy regardless of backend.

## Phase 7 — UI Seam

Add a thin transport layer over `orchestration-api`:

- REST commands;
- SSE or WebSocket events;
- durable read projection;
- minimal chat UI;
- expandable ticket/work view;
- artifacts;
- approval surface when approval semantics exist.

Avoid operational dashboards in the primary user experience unless evidence shows they are needed.

## Phase 8 — Hypothesis Benchmarks

Compare Seam against heavier harness baselines on matched tasks.

### H1 — Tool-blind orchestrator

Does preventing direct tool use reduce context growth and improve long-horizon reasoning?

### H2 — Tiny workers

Can the minimal worker runtime match or outperform full agent harnesses on bounded coding/research/analysis tasks?

### H3 — Memory-blind workers

Does limiting workers to ticket-supplied information improve focus/reliability without materially reducing completion rate?

### H4 — Natural-language broker interface

Is natural language sufficient between worker and SLM without model-specific adapters?

### H5 — Small broker model

What is the smallest model that achieves acceptable capability/argument accuracy?

### H6 — Local freedom vs. fully brokered operations

How much token/latency cost is avoided by allowing safe local workbench operations directly?

### H7 — Provider independence

Can capability/provider changes occur without changing worker behavior or prompts?

## Explicitly Deferred

Do not add these until a measured requirement exists:

- native Claude Code/Codex worker lanes;
- shared worker memory;
- general worker-to-worker messaging;
- model-specific broker adapters;
- autonomous capability generation;
- large skill/plugin marketplaces;
- complex multi-agent topology editors;
- rich operational UI;
- a custom model gateway;
- a custom durable workflow engine.
