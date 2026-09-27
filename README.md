# Seam

Seam is a modular architecture for bounded AI workers and semantic decision routing.

```text
Seam
├── Seam SDK      bounded worker runtime
└── Seam Engine   semantic decision plane
```

- [Seam SDK](https://github.com/Judge-M/Seam-SDK) runs task-scoped workers with bounded context, local workbench operations, and replaceable reasoning and external-operation ports.
- [Seam Engine](https://github.com/Judge-M/Seam-Engine) applies identity, trusted authority, semantic capability routing, System 1 decisions, and policy before requests reach an existing gateway.

```text
Seam SDK ── optional integration ──> Seam Engine ──> existing unified gateway
    │
    └──────────────────────────────────────────────> existing infrastructure

another agent runtime ──> Seam Engine ──> existing unified gateway
```

The user's gateway remains responsible for concrete model/tool/provider execution, MCP connectivity, credentials, health, load balancing, retries, and protocol conversion. Engine chooses a sanctioned semantic capability; the gateway decides how to execute it. SDK and Engine have no core dependency on each other.

This repository holds the [architecture](docs/ARCHITECTURE.md), [principles](docs/DESIGN_PRINCIPLES.md), [terminology](docs/TERMINOLOGY.md), [rationale](docs/RATIONALE.md), [trust boundaries](docs/SECURITY.md), [migration map](docs/MIGRATION_MAP.md), and [optional combined example](examples/combined). It is the umbrella specification, rather than a monolithic Rust runtime.

Licensed under Apache-2.0. See [LICENSE](LICENSE).
