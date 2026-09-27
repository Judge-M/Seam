# Seam repository migration map

This map was made from the current `Judge-M/Seam` `main` source before extraction. The original repository remains the historical source for the prototype; the new target repositories have independent core dependency graphs.

| Current path | Destination | Treatment |
| --- | --- | --- |
| `crates/worker-runtime/src/lib.rs` | Seam SDK | Preserve the bounded loop, local workbench, task statuses, observation and output limits, traversal and symlink checks, child environment clearing, and safety tests. Replace `ModelGateway` and `CapabilityClient` with SDK-owned `Reasoner` and `ExternalRuntime` ports. |
| `crates/protocol/src/lib.rs` | Shared concept requiring redesign | Move ticket, worker identity, local authority, action/report types into the SDK. Keep Engine requests generic; do not move ticket IDs, worker lifecycle, or `CapabilityClient` into Engine. |
| `crates/capability-broker/src/lib.rs` | Seam Engine, selectively | Preserve trusted authority lookup, pre-routing allowlist filtering, bounded failure codes, audit and schema checks. Replace the SLM translator with a generic decision contract and thresholded hierarchical router. Replace provider registry, scoring, and execution with a single gateway port. |
| `crates/capability-broker/src/schema.rs` | Seam Engine | Reuse bounded deterministic JSON schema validation after route selection and before gateway execution. |
| `crates/model-gateway/src/lib.rs` | Shared concept requiring redesign | The provider-neutral idea is useful, but Engine decisions use its gateway port and SDK reasoning uses its own reasoner port. No shared core crate or concrete model choice. |
| `crates/model-gateway-chat-completions` | Optional adapter, later | Transport-specific code does not belong in either core. Keep as reference for a future SDK-side direct gateway adapter if needed. |
| `crates/orchestration-kernel` | Obsolete for these two products | Durable conversation, memory, planning, dispatch, and workflow ownership are outside SDK and Engine. Preserve only the invariant that trusted dispatch grants authority and workers cannot mint it. |
| `crates/orchestration-api` | Obsolete for these two products | Client UI/projection and orchestration surface are outside scope. |
| `apps/demo` | Shared concept requiring redesign | Replace the coupled monolith demo with independent SDK and Engine fixtures and an optional combined adapter example. |
| `docs/SECURITY.md` | Umbrella Seam; adapted details in both products | Preserve trust boundaries and caveats, update the topology and ownership. |
| `docs/ARCHITECTURE.md`, root `ARCHITECTURE.md`, `docs/DESIGN_PRINCIPLES.md`, `docs/ROADMAP.md`, `README.md` | Umbrella Seam | Rewrite outdated broker SLM, provider-selection, and monolithic-runtime descriptions for the three-layer target. |
| `.github/workflows/ci.yml`, `.gitignore`, `LICENSE` | All three repositories | Use Apache-2.0 and independent formatting, lint, and test gates. |
| `Cargo.toml`, `Cargo.lock` | Split per product | Build each implementation independently. Umbrella becomes documentation and integration guidance, not a core Rust workspace. |

## Coupling and security findings

- `worker-runtime` depends on `model-gateway` and `agent-protocol`, and its tests depend on `capability-broker`. Its prompt names the broker directly. These ties must be removed from the SDK.
- `capability-broker` depends on the ticket protocol and model gateway, translates intents with a generative SLM, and selects concrete providers by score. These are replaced by generic caller authority, bounded System 1 decisions via the user's gateway, and semantic gateway requests.
- The old ticket authority store correctly binds a dispatch-assigned worker to a grant. Engine will resolve a generic subject/grant from a trusted authority source. SDK will pass only worker identity and intent to its external port.
- The filesystem workbench currently canonicalizes paths including final symlinks, rejects traversal, gates local operations by deployment and ticket ceilings, clears ambient process environment, and bounds observations and output. Preserve those properties. The workbench is an interface plus a reference implementation; it is not an OS sandbox.
- Engine's internal `seam.decision.*` gateway requests are control-plane operations. Caller grants must never authorize them, and the decision route must bypass Engine's own semantic routing to avoid recursion.
