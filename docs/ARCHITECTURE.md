# Seam Architecture

This document describes the current architectural contract for Seam. It distinguishes responsibilities, information boundaries, and replaceable infrastructure. Implementation details may change without changing these boundaries.

## 1. System Overview

```text
                                  USER
                                   │
                                   ▼
                    ┌───────────────────────────┐
                    │ PRESENTATION CLIENT       │
                    │ web / CLI / desktop/etc. │
                    └─────────────┬─────────────┘
                                  │
                       commands / events / reads
                                  │
                                  ▼
                    ┌───────────────────────────┐
                    │ ORCHESTRATION API         │
                    │ versioned semantic seam   │
                    └─────────────┬─────────────┘
                                  │
                                  ▼
                    ┌───────────────────────────┐
                    │ ORCHESTRATION KERNEL      │
                    │ context / ticket / memory │
                    │ policy                    │
                    └─────────────┬─────────────┘
                                  │
                    model gateway │ memory service
                         ┌────────┼────────┐
                         │        ▼        │
                         │ ┌─────────────┐ │
                         └▶│ORCHESTRATOR │◀┘
                           │ MODEL       │
                           │ frontier    │
                           └──────┬──────┘
                                  │
                               tickets
                                  │
                     durable work controller
                                  │
              ┌───────────────────┼───────────────────┐
              ▼                   ▼                   ▼
       ┌─────────────┐     ┌─────────────┐     ┌─────────────┐
       │ WORKER A    │     │ WORKER B    │     │ WORKER C    │
       │ tiny loop   │     │ tiny loop   │     │ tiny loop   │
       │ task state  │     │ task state  │     │ task state  │
       │ workbench   │     │ workbench   │     │ workbench   │
       └──────┬──────┘     └──────┬──────┘     └──────┬──────┘
              │                   │                   │
              └───────────────────┼───────────────────┘
                                  │
                     external capability requests
                                  │
                                  ▼
                    ┌───────────────────────────┐
                    │ CAPABILITY BROKER         │
                    │ deterministic policy      │
                    │ + narrow SLM translator   │
                    │ + operational memory      │
                    └─────────────┬─────────────┘
                                  │
                    ┌─────────────┼─────────────┐
                    ▼             ▼             ▼
                   MCP           APIs        Services
```

## 2. Responsibility Model

```text
Orchestration Kernel: durable coordination and policy
Orchestrator Model:   WHY / WHAT
Worker Model:         HOW TO SOLVE THIS TICKET
Broker SLM:           HOW TO EXPRESS THIS EXTERNAL OPERATION
Provider:             EXECUTE
```

No layer should absorb another layer's responsibility merely because a model is capable of doing so.

## 3. Orchestration Kernel

The Orchestration Kernel owns the durable semantics around the primary model.

Responsibilities:

- receive user turns;
- persist/reconstruct conversation state;
- retrieve permitted durable memory;
- assemble orchestrator context;
- invoke the selected frontier model through a model gateway;
- interpret orchestration-control outputs;
- create/update/cancel work tickets;
- receive worker completion/failure events;
- resume the correct orchestration flow;
- decide what information is eligible for durable memory;
- emit semantic client events.

The kernel should not become a generic tool runner, workflow engine, memory database, or model-provider SDK layer.

## 4. Orchestrator Model

The orchestrator is the only component expected to maintain the system's broad understanding of the user and ongoing work.

It may reason about:

- user intent;
- goals;
- project context;
- plans;
- decomposition;
- worker results;
- contradictions;
- what deserves durable retention.

It must not receive operational tools such as shell, browser, GitHub, databases, MCP, or cloud APIs.

Its model-visible control surface should remain small, e.g.:

```text
worker.submit
worker.status
worker.update
worker.cancel

memory.retrieve
memory.propose_write
```

Exact functions may evolve; the invariant is that the orchestrator has **control-plane authority, not operational authority**.

## 5. Tickets

Tickets are the sole default information-transfer mechanism from orchestrator to worker.

A ticket SHOULD include:

- objective;
- relevant facts/context;
- inputs/artifacts;
- constraints;
- expected output;
- authorization/capability envelope;
- resource/budget limits when relevant.

A ticket SHOULD NOT contain unrelated durable memory or conversation history.

Workers must be able to complete their task from the ticket and their assigned workspace. If they cannot, the task should become blocked or request additional scoped information rather than independently browsing global memory.

## 6. Worker Runtime

The generic worker runtime is intentionally small.

Required responsibilities:

```text
accept an assigned worker identity and ticket
construct task-local model context within a bounded observation budget
expose sandbox workbench primitives with bounded output
accept broker capability results
maintain task-local state
loop until completion/block/failure
return bounded final result
```

Worker identity is assigned by the dispatcher, not chosen by the worker, because the
broker binds authority to it.

A failed *task* is a report with status `failed`. A runtime error is a separate outcome:
the two must not be conflated, or the orchestrator cannot tell "the work could not be
done" from "the harness broke". Unparseable model output is corrected and retried a
bounded number of times before the task is failed.

The runtime does not require persistent identity or durable memory.

### 6.1 Local workbench

Workers may directly operate inside their assigned isolation boundary:

- read/write files;
- search/grep;
- edit/patch;
- execute sandboxed processes;
- compile/run code;
- run tests;
- use local Git;
- create artifacts;
- maintain task-local scratch state.

These actions should not require an SLM round trip through the Capability Broker.

### 6.2 External boundary

Anything that crosses the sandbox boundary must be brokered.

Examples:

- network access;
- web search/fetch/render;
- MCP;
- remote Git/GitHub;
- SaaS APIs;
- databases outside the sandbox;
- cloud control planes;
- credentials/secrets;
- external messaging;
- remote hosts.

## 7. Capability Broker

The Capability Broker is a logical shared service, regardless of physical replica count.

Responsibilities:

- maintain logical capability definitions;
- authorize requests against ticket authority;
- invoke the SLM only where semantic translation is required;
- validate SLM output;
- choose providers deterministically;
- enforce cost/quota/rate constraints;
- normalize provider results/failures;
- record operational telemetry;
- retain operational memory.

### 7.0 Authority resolution

The broker authorizes against a grant it resolves itself, keyed by ticket and worker
identity. Callers present identity and intent; they do not present permissions. In the
prototype this is an in-memory store written by the dispatcher; in a distributed
deployment it is the point at which a signed capability token would be verified.

### 7.1 SLM responsibility

The SLM's charter is deliberately narrow:

```text
input:
  immediate worker request
  relevant logical capability definitions
  execution constraints

output:
  selected logical capability
  validated arguments
```

It must not:

- solve the worker's task;
- make strategic decisions;
- access user/project durable memory;
- rewrite the ticket objective;
- write general-purpose application code;
- choose infrastructure providers when deterministic policy can do so.

### 7.2 Capability gaps

If no existing logical capability can safely express the requested operation, the broker should report a typed capability gap rather than asking the SLM to improvise general-purpose code.

Repeated gaps are product signals: they may justify adding a reusable capability.

## 8. Provider Layer

Providers implement logical capabilities.

Possible transports include:

- MCP;
- REST APIs;
- GraphQL;
- native SDKs;
- local services;
- controlled CLI adapters;
- other RPC mechanisms.

MCP is a provider interface, not a semantic category. Workers request `web.search` or `development.issue.search`, not “use MCP.”

## 9. Information Compartments

```text
ORCHESTRATOR              WORKER                 BROKER
-----------               ------                 ------
conversation              ticket                 capability schemas
user/project memory       supplied context       provider state
plans/decisions           task scratch           quotas/cost
worker summaries          workspace              health/telemetry

NO operational catalog    NO durable memory      NO user/project memory
NO raw tool exhaust       NO global context      NO orchestrator memory
```

There is no default shared knowledge bus.

## 10. Memory

Memory mechanics may be delegated to a dedicated service. Memory policy belongs to Seam.

The Orchestration Kernel determines:

- which namespaces may be queried;
- retrieval token budget;
- admissible memory types;
- what stays session-only;
- what worker output may be promoted;
- what should be discarded;
- what may be exposed back to the orchestrator.

Workers do not automatically receive retrieval access.

The Capability Broker's memory is separate and operational only.

## 11. UI / Presentation Seam

The UI is a client of Seam, not part of the agent runtime.

The API contract should expose semantic concepts:

```text
commands:
  send_message
  cancel_ticket
  update_ticket
  provide_input
  approve / reject (when introduced)

events:
  message_created
  ticket_created
  ticket_updated
  ticket_state_changed
  artifact_created
  approval_required
```

Read projections should provide current conversation/ticket state after reload/reconnect.

The UI must not perform orchestration logic, model routing, provider selection, worker creation, or authorization decisions.

Transport is replaceable. REST + SSE/WebSocket is a reasonable implementation, but not part of the semantic contract.

## 12. Durable Execution

Seam should not implement a general durable workflow engine unless required by evidence.

A dedicated durable runtime may own:

- retries;
- queues;
- crash recovery;
- wait/resume;
- external events;
- cancellation;
- timers;
- worker lifecycle durability.

Seam owns the semantics of tickets and completion, not the generic mechanics of reliable distributed execution.

## 13. External Information Flow

### Downward

```text
User intent
    ↓
Orchestrator understanding
    ↓
Ticket
    ↓
Worker reasoning
    ↓
External operational request
    ↓
Broker SLM translation
    ↓
Logical capability
    ↓
Provider invocation
```

### Upward

```text
Provider result
    ↓
Worker working context
    ↓
Bounded worker result/artifacts
    ↓
Orchestrator
    ↓
User and/or orchestrator durable memory
```

Each boundary should compress or narrow information for the layer above it.

## 14. Replaceable Infrastructure

The architecture should not depend on specific vendors for generic mechanisms.

```text
model access         → gateway/proxy
memory mechanics     → memory service
conversation store   → durable data store
workflow durability  → durable runtime
capability transport → MCP/API/etc.
presentation         → any client
```

The seam matters more than the implementation behind it.

## 15. Non-Goals

Seam does not aim to be:

- an all-purpose agent harness;
- a model-specific coding runtime;
- a swarm with shared memory;
- a workflow engine;
- an MCP registry product;
- a memory database;
- a browser automation framework;
- a UI framework.

## 16. Current Architectural Hypotheses

These require benchmarking:

1. A tool-blind primary retains cleaner useful context.
2. Small ticket-scoped workers can outperform heavier harnesses on bounded work.
3. Workers benefit from not having global durable-memory retrieval.
4. Natural language is sufficient for the worker → broker semantic boundary.
5. A small model can reliably map operational intent to logical capabilities.
6. Local worker freedom plus brokered external authority is efficient and secure.
7. Model-specific adapters are unnecessary unless empirical results prove otherwise.
