# Seam Security Model

Seam's security model is based on **authority separation and information separation**. The architecture should not rely on a model following prose instructions when a boundary can be enforced in code or infrastructure.

## Trust Boundaries

```text
USER / CLIENT
      │
      ▼
ORCHESTRATION KERNEL
      │   no operational credentials/tools
      ▼
WORKER SANDBOX
      │   local workspace authority only
      ▼
CAPABILITY BROKER
      │   explicit ticket authority + policy
      ▼
EXTERNAL PROVIDERS / SYSTEMS
```

## Orchestrator

The orchestrator should have:

- access to its permitted conversation/memory context;
- worker control-plane functions;
- no shell;
- no direct browser/web tool;
- no direct MCP server access;
- no cloud/API credentials;
- no direct external-system authority.

Operational tools should be absent from the model-visible schema, not merely discouraged by prompt text.

## Workers

Workers should execute in real OS-level isolation.

Default environment:

```text
workspace filesystem:  read/write
host filesystem:       denied
network:                denied
ambient credentials:   none
cross-worker IPC:       denied
orchestrator memory:    denied
durable memory:         denied
external access:        Capability Broker only
```

The Rust path restrictions in the prototype are convenience checks, not a security sandbox.

## Capability Broker

The Capability Broker is a privileged boundary and should enforce:

- caller/ticket identity;
- explicit capability authority;
- argument validation;
- provider policy;
- cost/rate/quota ceilings;
- credential scoping;
- audit events;
- typed failures;
- safe retry behavior.

The broker's SLM output must be treated as untrusted structured input and validated before execution.

## Credentials

Workers and the orchestrator should not receive raw durable credentials unless an explicit future design requires it.

Provider credentials should be resolved/brokered at the capability layer and scoped to the smallest possible operation.

## Memory Isolation

Memory access is compartmentalized:

- orchestrator memory is unavailable to workers and broker by default;
- workers retain only task-local state;
- broker memory contains operational/provider state only.

Recording worker activity does not grant workers historical read access.

## Network Policy

The default worker network policy should be deny-all.

All external network access should flow through brokered capabilities where identity, policy, quota, provenance and audit controls can be applied.

## Failure Handling

Infrastructure details should remain below the semantic boundary when they are not useful above it.

Example:

```text
provider failure:
  github-mcp socket error / HTTP 503

worker-facing failure:
  CAPABILITY_TEMPORARILY_UNAVAILABLE

orchestrator-facing result:
  Unable to inspect remote CI state; task blocked.
```

This reduces accidental leakage of provider topology and credentials while keeping failures actionable.

## Non-Goals

The current prototype does not yet provide a production sandbox, secret broker, complete authorization engine, hardened network proxy, or production audit ledger. Those are required before treating Seam as a secure execution environment.
