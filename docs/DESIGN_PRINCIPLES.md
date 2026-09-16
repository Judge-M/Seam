# Seam Design Principles

This document records the normative principles behind Seam. They are intended to prevent implementation convenience from silently collapsing architectural boundaries.

## 1. Orchestrate, Do Not Operate

The primary/orchestrator reasons about goals, decomposition, context and results. It should not directly operate shells, browsers, APIs, repositories, databases, infrastructure, or MCP servers.

Operational access belongs below the worker boundary.

## 2. Tickets, Not Context Inheritance

Workers receive explicit tickets rather than inheriting the orchestrator's conversation or memory.

If information matters to the task, include it in the ticket.

This makes context transfer visible, bounded and auditable.

## 3. Local Freedom, External Mediation

Workers should not pay an SLM/tool round trip for ordinary operations inside their own sandbox.

They may use a small local workbench directly.

Anything crossing the sandbox boundary must use the Capability Broker.

## 4. Informational Least Privilege

Authority is not the only thing that should be minimized. Information should be minimized too.

- the orchestrator knows the big picture but not tool plumbing;
- workers know their ticket but not durable/global context;
- the broker knows operational infrastructure but not user/project memory.

## 5. No Global Memory Bus by Default

Shared knowledge planes make boundaries easy to bypass and context easy to pollute.

Memory should be compartmentalized and exposed only through explicit policy.

## 6. Record Does Not Imply Read

Worker activity may be recorded for audit, debugging and evaluation. That does not grant workers permission to retrieve historical records.

Information flow can intentionally be asymmetric.

## 7. The Broker SLM Translates; It Does Not Solve

The SLM inside the Capability Broker exists to bridge natural language and logical capabilities.

It should not become a generic fallback worker.

If a task requires coding or domain reasoning, that work belongs to the worker model.

## 8. Deterministic Before Probabilistic

If capability policy, capability routing, authorization, validation or capability-provider
selection can be expressed reliably in normal code, use normal code.

Use the SLM only where semantic interpretation is actually required.

## 9. Capability Before Provider

Workers request logical capabilities. They should not be coupled to MCP servers, vendor APIs, CLI tools, or provider names.

Provider selection belongs to the broker.

## 10. Do Not Normalize Models Prematurely

Do not maintain model-specific adapters merely because models differ.

Natural language is the default semantic boundary. Add specialized adapters only if evaluation demonstrates a material improvement.

## 11. Workers Are Disposable

A worker should be cheap to create, easy to destroy, and reconstructable from its ticket plus workspace/task state.

Long-lived identity and memory are not worker requirements.

## 12. Keep the Worker Harness Small

The worker runtime should provide only what bounded work requires:

- model loop;
- ticket context;
- local workbench;
- task scratch;
- broker request;
- completion/block/failure reporting.

Do not import a large agent framework to gain functionality Seam intentionally does not need.

## 13. Outsource Commodity Mechanics

Provider compatibility, durable workflows, memory storage/retrieval algorithms, and transport/UI frameworks are replaceable infrastructure.

Seam should own policies and semantic contracts, not duplicate mature infrastructure without evidence.

Shared ports must not expose a vendor SDK type, transport-client error, deployment product,
or adapter-specific wire field. Concrete adapters translate at the edge and are selected by
the composition root. Products named in examples or evaluations are candidates, not defaults.

Seam does not select models. Model identifiers, routing, fallback and provider policy belong
behind `ModelGateway`; Seam components provide semantic inference requests only.

## 14. UI Is a Client

The UI renders conversations, work, approvals, artifacts and events. It does not decide how
work is decomposed, how inference is routed, or which provider executes a capability.

Presentation must remain replaceable.

## 15. Seams Are the Product Surface

Every major boundary should be explicit:

```text
presentation ↔ orchestration
orchestration ↔ workers
workers ↔ capability broker
capability broker ↔ providers
orchestration ↔ memory mechanics
orchestration ↔ model gateway
kernel ↔ durable runtime
```

The architecture should optimize these contracts rather than collapse them.
