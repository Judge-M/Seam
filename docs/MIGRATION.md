# Migration from the monolithic prototype

The previous `Judge-M/Seam` workspace combined worker runtime, ticket protocol, capability broker, model gateway, orchestration kernel, API, and demo. Its historical Git commits remain available in this repository. The [migration map](MIGRATION_MAP.md) records where each significant crate, test group, and document belongs.

For worker integrations, replace `ModelGateway` with `seam_sdk::Reasoner` and `CapabilityClient` with `seam_sdk::ExternalRuntime`. Move worker-specific tickets, reports, and local authority to the SDK. The external port receives assigned identity and intent; resolve external grants in trusted infrastructure.

For broker integrations, replace `TicketAuthorityStore` with Engine's generic `AuthoritySource`, and replace the generative `SlmTranslator` with a `DecisionEngine`. Use `GatewayDecisionEngine` when a preconfigured unified gateway serves System 1 requests. Move provider selection, credentials, generic retries, health, and load balancing to the existing gateway. Register only logical capabilities and their argument schema in Engine.

The old orchestration kernel and client API are outside the two implementation products. Agent platforms may retain their own orchestration and connect directly to SDK, Engine, or both. The chat-completions transport adapter is source material for optional downstream adapters; it does not define either core API.

For the combined path, implement the SDK ports in a separate adapter that calls Engine. The fixture at `examples/combined` demonstrates this boundary without either core crate depending on the other.
