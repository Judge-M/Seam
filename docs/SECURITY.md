# Trust boundaries

```text
trusted dispatcher/policy source ──> SDK worker identity and local authority
                             └─────> Engine subject/grant binding

SDK local workbench ── explicit external port ──> Engine (optional)
Engine trusted grant lookup ── authorized candidates ──> System 1
System 1 selection ── deterministic validation ──> existing gateway
```

The worker cannot assign itself a trusted identity or widen its external grant. SDK callers assign `WorkerId`. Engine resolves authority from an `AuthoritySource` keyed to the generic subject and grant identifier; requests do not carry a self-asserted allowed capability set. Unknown, revoked, expired, or mismatched grants fail before routing. A model's confidence has no role in authorization.

Engine's internal `seam.decision.*` control requests are separate from caller capabilities. They have no caller subject, do not appear in the caller catalog, and go directly to a gateway route that must not invoke Engine recursively. The gateway handles System 1 model/provider selection and infrastructure fallback. Engine validates that each choice is a supplied candidate, applies confidence thresholds, and validates arguments before caller execution. Gateway failures are returned through bounded error types; adapter internals and credentials are not exposed to callers.

SDK's reference workbench rejects absolute and parent-traversal paths and resolves final symlinks before reads and writes. Each operation checks both local task authority and deployment ceilings. Child processes start with a cleared environment and an explicit allowlist, have a timeout, and return capped output. Worker observations and their window are bounded. Task failure remains a report; failure of the runtime itself is an error.

These checks are inner guards, not an OS sandbox. Allowing process execution without an outer sandbox may expose the host or network. Filesystem canonicalization can also race with concurrent filesystem changes. Production deployments should supply operating-system isolation and avoid concurrent mutation of worker workspace paths from less trusted processes. ExternalRuntime and Gateway adapters must enforce their own credential and transport boundaries.

The schema validator implements a documented subset of JSON Schema: `type`, `required`, `properties`, `items`, `enum`, and `additionalProperties`. Adapters requiring a richer schema must validate further before execution.
