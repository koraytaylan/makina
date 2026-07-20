# Plan authoring model mechanism

Status: Normative

Makina coordinates external, already-authenticated ACP agents; it does not embed a model client or manage model credentials. Model-assisted plan authoring uses the configured one-shot agent backend.

The agent receives repository context and collision-free number reservations and returns only a typed plan blueprint. It does not select plan numbers, write Markdown, update status, run Git commands, or register a plan. The Rust authoring coordinator validates the closed blueprint, renders the canonical `SCOPE.md`, `ARCHITECTURE.md`, `STATUS.md`, and `tasks/*.md` bundle atomically, and optionally registers the exact committed source.

Every host-level author-agent promise is enclosed by the plan-contract worker lifecycle so the canonical repository lease remains held until that exact call terminates. Invalid or incomplete model output fails closed at the typed boundary; there is no free-form task-list parser or model-output runtime schema.

Direct model APIs remain unsupported. The same ACP authentication and process-isolation rules used by Developer and Reviewer agents apply to authoring agents.
