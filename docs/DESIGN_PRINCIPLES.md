# Design principles

1. Give workers bounded tasks and task-local context instead of global history.
2. Keep durable/global memory out of workers by default.
3. Let local sandbox work stay local; cross an explicit seam for outside operations.
4. Separate semantic routing from infrastructure execution.
5. Use System 1 for classification and routing, while deterministic code enforces authority.
6. Keep the existing gateway replaceable and avoid rebuilding commodity gateway behavior.
7. Make SDK and Engine independently useful, with optional integration in a separate adapter.
8. Keep model, tool, provider, and vendor details behind gateway adapters.
9. A unified gateway may serve both reasoning and external operations while the worker keeps them conceptually separate.
10. Engine selects a sanctioned semantic capability; the downstream gateway selects concrete execution.
