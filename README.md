# Seam

**Seam is a minimal, compartmentalized agent runtime built around delegation, bounded workers, and brokered external capabilities.**

Most agent frameworks put the primary model in the middle of everything: conversation history, memory, tools, browser state, shell output, APIs, subagents, and provider-specific plumbing. Seam takes the opposite approach.

The primary model keeps the big picture and delegates work. Small task-scoped workers solve individual tickets. Workers can freely operate inside isolated workspaces, but anything outside that boundary goes through a shared Capability Broker. A narrow SLM inside the broker translates natural-language operational intent into logical capability calls; deterministic code handles authorization and provider selection.

The result is an architecture designed around **shorter context, informational least privilege, replaceable infrastructure, and explicit seams between responsibilities**.

> **Status:** experimental architecture prototype. The current Rust workspace demonstrates the core contracts and boundaries. It builds clean and is formatted, linted (`clippy -D warnings`) and tested in CI on every pull request. It is not yet production-ready — see [Security Model](docs/SECURITY.md) for what the prototype does and does not enforce.

---

## Why Seam?

Agent systems tend to accumulate responsibilities in the same runtime:

- persistent conversation state;
- long-term memory;
- tool catalogs;
- shell and filesystem access;
- web browsing;
- MCP servers;
- credentials;
- subagent state;
- model-provider integrations;
- workflow recovery;
- UI state.

That makes the primary model expensive, difficult to secure, difficult to reason about, and increasingly dependent on a particular harness.

Seam instead asks a simpler question:

> **What is the minimum information and authority each layer actually needs?**

```text
                                  USER
                                   │
                                   ▼
                        ┌────────────────────┐
                        │ UI / CLIENT SEAM   │
                        └─────────┬──────────┘
                                  │
                                  ▼
                        ┌────────────────────┐
                        │ ORCHESTRATION      │
                        │ KERNEL             │
                        └─────────┬──────────┘
                                  │
                          frontier model
                                  │
                               tickets
                                  │
                ┌─────────────────┼─────────────────┐
                ▼                 ▼                 ▼
             Worker            Worker            Worker
                │                 │                 │
                └─────────────────┼─────────────────┘
                                  │
                       external capability requests
                                  │
                                  ▼
                        ┌────────────────────┐
                        │ CAPABILITY BROKER  │
                        │ deterministic core │
                        │ + narrow SLM       │
                        └─────────┬──────────┘
                                  │
                         MCP / API / services
```

---

## Core Model

Seam separates three kinds of reasoning.

### Orchestrator — **Why / What**

The persistent frontier model understands the user, maintains the big picture, plans, delegates, evaluates worker results, and decides what matters enough to retain.

It does **not** have operational tools.

### Worker — **How to solve this ticket**

A worker receives only the task, relevant context, constraints, and workspace required for a single piece of work.

It has a small local workbench—files, edits, sandboxed processes, local Git, scratch state—but no durable memory and no direct external tools.

### Capability Broker SLM — **How to express this external operation**

The broker's small language model has a deliberately narrow job:

1. understand the worker's immediate operational request;
2. select the logical capability;
3. construct valid arguments.

It does not plan the worker's task, access user/project memory, or write general-purpose code.

Provider selection, policy, authorization, quotas, retries, and telemetry remain deterministic wherever possible.

---

## The Boundary That Matters

Seam does **not** divide the world into “tools” and “no tools.”

It divides it into **local work** and **external authority**.

Workers may directly:

```text
read/write files inside their workspace
search/grep workspace content
patch/edit files
run sandboxed commands
compile and execute code
run tests
use local Git
create artifacts
maintain task-local scratch state
```

Workers must use the Capability Broker for:

```text
web/search/browsing
MCP
remote Git/GitHub
SaaS APIs
email/chat
remote databases
cloud/infrastructure
SSH to external systems
credentials/secrets
anything outside the sandbox boundary
```

This avoids wasting model calls on trivial local operations while preserving a clear privilege boundary around the outside world.

---

## Information Compartments

There is intentionally **no global shared memory plane**.

```text
┌─────────────────────────┐
│ ORCHESTRATOR            │
│                         │
│ conversation            │
│ durable memory          │
│ plans / decisions       │
│ project understanding   │
│ worker summaries        │
└─────────────────────────┘

            │ tickets / results
            │
            ▼

┌─────────────────────────┐
│ WORKER                  │
│                         │
│ assigned ticket         │
│ supplied context        │
│ local workspace         │
│ task scratch            │
│ NO durable memory       │
│ NO big picture          │
└─────────────────────────┘

            │ capability requests
            │
            ▼

┌─────────────────────────┐
│ CAPABILITY BROKER       │
│                         │
│ capability schemas      │
│ provider state          │
│ quotas / costs          │
│ health / telemetry      │
│ operational memory      │
│ NO user/project memory  │
└─────────────────────────┘
```

Workers are intentionally **memory-blind by default**. If a worker needs information, the orchestrator should put it in the ticket.

Worker activity may be recorded for auditability without granting workers the ability to browse historical records.

---

## Tickets Are the Delegation Contract

Workers are created around work, not personalities.

A ticket contains the information required to perform one bounded task:

```yaml
objective: determine why the API integration tests fail

context:
  repository: example
  branch: feature/foo

constraints:
  - do not modify main
  - do not change the public API

expected_output:
  - root cause
  - proposed fix
  - validation result

capabilities:
  external:
    - development.issue.search
```

A worker should not need the orchestrator's full conversation or durable memory to complete the ticket.

The orchestrator receives the bounded result, not the worker's entire working context or raw tool exhaust.

---

## Capability Broker

The Capability Broker is a shared external-access boundary for all workers.

```text
Worker request
     │
     ▼
Narrow SLM
intent → logical capability + arguments
     │
     ▼
Deterministic broker core
authorization / provider choice / quota / health
     │
     ▼
Capability provider
MCP / REST / native service / CLI adapter / other
```

### Logical capabilities, not provider names

A worker asks for an outcome such as:

```text
development.issue.search
web.search
web.fetch
communication.email.send
infrastructure.host.inspect
```

It should not need to know whether the implementation is MCP, REST, a native SDK, or another transport.

MCP is therefore a **provider/integration surface**, not Seam's capability ontology.

### Operational memory only

The broker may remember information such as:

- provider health;
- latency;
- quotas;
- historical failures;
- capability schemas;
- observed site behavior;
- cost;
- successful fallback paths.

It should not have access to the user's durable memory or project narrative.

---

## UI Is a Seam Too

The presentation layer is outside the agent runtime.

```text
Web UI / Desktop / CLI / Mobile / Chat bridge
                     │
              commands / events
                     │
                     ▼
              orchestration-api
                     │
                     ▼
             orchestration-kernel
```

Clients see semantic objects such as:

- conversations;
- messages;
- tickets;
- ticket status;
- approvals;
- artifacts;
- notifications.

They do not need to know about:

- worker processes;
- SLM prompts;
- MCP topology;
- provider routing;
- model-gateway internals;
- durable-workflow invocation IDs.

This allows a web app, terminal UI, desktop client, mobile client, or chat integration to use the same orchestration system without embedding agent logic in the presentation layer.

---

## Commodity Infrastructure Stays Replaceable

Seam is intentionally not trying to reimplement generic infrastructure.

Expected seams include:

```text
Model gateway          LiteLLM or equivalent
Memory mechanics       external/self-hosted memory service
Durable execution      Restate / DBOS / Temporal-class runtime
Conversation storage   durable database / event log
Capability providers   MCP / APIs / native services
Presentation           any client over orchestration-api
```

Seam should own the semantics that make Seam distinct:

- ticket semantics;
- information boundaries;
- context assembly policy;
- memory admission policy;
- worker contract;
- capability ontology;
- capability authorization;
- broker behavior;
- result/failure propagation.

Everything else should remain replaceable where practical.

---

## Repository Layout

```text
crates/
├── protocol/               shared tickets, authority and result types
├── model-gateway/          model abstraction + LiteLLM-compatible client
├── orchestration-kernel/   primary context, ticket and memory policy seam
├── orchestration-api/      versioned client command/event/read seam
├── worker-runtime/         tiny task-scoped worker loop + local workbench
└── capability-broker/      SLM translation, policy and provider routing

apps/
└── demo/                   minimal wiring example

docs/
├── ARCHITECTURE.md         detailed component and data-flow design
├── DESIGN_PRINCIPLES.md    normative architectural principles
├── SECURITY.md             trust boundaries and security assumptions
└── ROADMAP.md              prototype milestones and hypotheses to test
```

---

## Current Workspace

The prototype currently includes:

- shared protocol types for tickets, authority, capabilities and worker reports;
- a model-gateway abstraction with a LiteLLM-compatible implementation;
- a tool-blind orchestration kernel;
- a tiny worker runtime;
- workspace-local file/process primitives;
- a Capability Broker with a narrow SLM translation boundary;
- deterministic capability authorization/provider selection;
- deterministic validation of SLM-produced arguments before any provider is invoked;
- typed, provider-opaque failure codes at the worker boundary;
- a deterministic ticket-authority policy the orchestrator cannot widen;
- broker-side authority resolution: workers present identity, not permissions;
- bounded worker context — windowed reads, capped output, capped observation budget;
- task failure reported as a result rather than a runtime error;
- ticket/conversation ownership enforced at the client seam;
- initial broker operational telemetry/state;
- a transport-agnostic orchestration UI API;
- an in-memory read projection for development/testing.

### Important limitation

Filesystem path checks in the worker runtime are **not an OS sandbox**.

They refuse absolute paths, parent traversal, and symlinks that resolve outside the
workspace, and every operation is gated on the ticket's authority as well as the
workbench's own ceiling. That is defence in depth, not isolation: a worker that can
execute processes can still reach the host.

Production workers must run inside a real isolation boundary such as a container, microVM, sandbox service, or equivalent environment with:

- network denied by default;
- no ambient credentials;
- no unrestricted host mounts;
- bounded CPU/memory/process access;
- external access only through the Capability Broker.

---

## Design Principles

Seam is built around a few strict defaults:

1. **The orchestrator reasons; it does not operate.**
2. **Workers receive tickets, not the entire world.**
3. **Workers are free inside their sandbox and brokered outside it.**
4. **The Capability Broker knows tools, not users.**
5. **The SLM translates; deterministic code governs.**
6. **Memory is compartmentalized rather than globally shared.**
7. **Record work without automatically granting historical read access.**
8. **Prefer replaceable commodity infrastructure over framework ownership.**
9. **Add model-specific behavior only when measurements justify it.**
10. **Every major boundary should remain a seam.**

See [Design Principles](docs/DESIGN_PRINCIPLES.md) for the normative version.

---

## What Seam Is Not

Seam is not intended to be:

- an all-in-one agent framework;
- a model-specific coding harness;
- a shared-memory swarm;
- an MCP wrapper;
- a workflow engine;
- a vector database;
- a browser automation product;
- a replacement for provider gateways;
- a UI framework.

It is the thin coordination architecture between those pieces.

---

## Hypotheses We Intend to Test

Several important design choices are deliberately treated as hypotheses rather than facts:

- a tool-blind frontier orchestrator will retain cleaner context and reason more effectively;
- tiny ticket-scoped workers can match or outperform heavier agent harnesses on bounded tasks;
- workers benefit from not having access to durable/global memory;
- natural language is sufficient as the worker → Capability Broker interface;
- a small model can reliably translate immediate operational intent into capability invocations;
- local worker freedom plus brokered external access provides a better cost/security balance than brokering every action;
- the architecture can remain largely model-neutral without model-specific adapters.

The prototype exists to measure these claims rather than assume them.

---

## Documentation

- [Architecture](docs/ARCHITECTURE.md)
- [Design Principles](docs/DESIGN_PRINCIPLES.md)
- [Security Model](docs/SECURITY.md)
- [Roadmap](docs/ROADMAP.md)

---

## License

Dual licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. This matches the `MIT OR Apache-2.0` declaration in the workspace manifest
and the usual Rust convention.

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in this work by you shall be dual licensed as above, without any additional
terms or conditions.
