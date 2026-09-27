# Terminology

| Term | Meaning |
| --- | --- |
| Ticket | One bounded task assigned to an SDK worker, with relevant context, constraints, deliverable, and local authority. |
| Worker | The SDK's task-scoped loop. Its identity is assigned by trusted dispatch code. |
| Reasoner | SDK port that chooses the worker's next typed action. |
| ExternalRuntime | SDK port for an operation outside the worker's local isolation boundary. |
| LocalWorkbench | Replaceable SDK interface for permitted local filesystem and process work. |
| Subject | Generic caller identity presented to Engine; it may represent an SDK worker or another runtime. |
| AuthoritySource | Trusted Engine source that resolves a subject/grant binding. |
| Capability | Provider-neutral semantic operation sanctioned for a caller. |
| DecisionEngine | Generic System 1 contract for choice, boolean, and score decisions. |
| Gateway | Replaceable downstream execution boundary for semantic caller requests and Engine's internal decision requests. |
| Control route | Preconfigured `seam.decision.*` gateway route used by Engine itself, separate from caller authority. |
| Escalation | Replaceable response when a semantic choice falls below the configured confidence threshold. |
