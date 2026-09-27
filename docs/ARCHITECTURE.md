# Architecture

## Responsibility split

```text
┌──────────────────────────┐
│ Seam SDK                 │  What do I need done?
│ bounded worker loop      │
│ task-local observations  │
│ local workbench          │
└────────────┬─────────────┘
             │ optional integration
             ▼
┌──────────────────────────┐
│ Seam Engine              │  Which sanctioned semantic capability fits?
│ trusted identity/grants  │
│ layered System 1 routing │
│ deterministic policy     │
└────────────┬─────────────┘
             ▼
┌──────────────────────────┐
│ Existing unified gateway │  Which concrete model/tool/provider executes it?
│ models, tools, MCP, APIs │
└──────────────────────────┘
```

The SDK also works directly with existing inference and external infrastructure. Engine also accepts any other agent runtime. Neither implementation crate depends on the other; the optional adapter in `examples/combined` depends on both.

## SDK contract

A dispatcher assigns the worker identity and ticket. The worker receives task context, local authority, constraints, and a deliverable, with no durable memory. `Reasoner` chooses the next worker action. `ExternalRuntime` handles an intent outside the local boundary. The worker loop caps steps and observations and returns completed, blocked, or failed reports. Runtime failures remain separate from task failures. Local workbench methods enforce deployment ceilings and ticket local authority. External grants are resolved by the external implementation from trusted state; the ticket cannot confer them.

The two ports may target one unified downstream gateway without merging their meanings. The worker's reasoner asks for a next action; its external port asks for an operation.

## Engine contract

Engine accepts generic `Subject`, grant identifier, intent, and arguments. `AuthoritySource` resolves the grant using trusted state. Registered logical capabilities are filtered against that grant before any decision request. The router narrows candidates by namespace segment across as many layers as required. The generic `DecisionEngine` chooses from the supplied set; deterministic code rejects invented candidates and invalid confidence values. Low confidence invokes a replaceable escalation policy. A selected capability's arguments are schema checked before a replaceable `Gateway` executes it.

Engine's gateway-backed decision adapter sends `seam.decision.choice`, `.boolean`, or `.score` directly to a preconfigured gateway control route. The control route has no caller subject or caller grant and must not call Engine again. The gateway chooses the actual System 1 model and infrastructure fallback. Engine handles semantic escalation when confidence is low.

Capabilities such as `inference.code.reason` and `development.issue.search` are semantic names. Engine does not choose concrete model IDs, providers, MCP servers, or endpoints. The gateway owns those mappings and execution. Caller grants do not authorize `seam.decision.*`; those names are reserved for Engine control requests.

## Integration

The adapter in `examples/combined` implements both SDK ports through Engine. Its trusted dispatcher binds an SDK worker identity and ticket ID to an Engine grant. The SDK sends only assigned identity, intent, and inputs; Engine resolves and enforces the grant before routing. The adapter maps Engine errors into bounded SDK errors and parses a gateway-produced worker action for `Reasoner`.

Clone all three repositories as siblings to run the fixture:

```sh
cargo run --manifest-path Seam/examples/combined/Cargo.toml
```

The relative paths in that example are only for a local demonstration. A deployed adapter can use versioned crate dependencies and its own transport.
