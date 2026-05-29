# Planner Model-Call Mechanism

Task: `planner-model-mechanism` (task 18)
Status: Normative — decision finalised

---

## 1. The Decision

**The Planner uses the one-shot-agent backend mechanism (`PlannerMechanism::OneShotAgent`).**

Implementation: `ModelInterpreter` in `crates/makina-core/src/interpreter.rs`.

`PlannerMechanism::DirectApi` is documented as a future option but is **not implemented** in the MVP. Selecting it returns `InterpretError::MechanismNotSupported` with a clear message — no silent fallback.

---

## 2. Rationale

The project principle is: **"agents are external processes; Makina coordinates, it does not embed model clients."**

Two mechanisms were considered:

| | `OneShotAgent` (chosen) | `DirectApi` (deferred) |
|-|-------------------------|------------------------|
| **Implementation** | Reuses `AgentBackend` / `AcpBackend` — the same trait used for Developer and Reviewer agents | Would require a new HTTP client + API-key management inside `makina-core` |
| **Auth** | Zed model: CLI is pre-authenticated by the user; Makina inherits the session via environment — **no credential handling in Makina** | Requires Makina to obtain, store, and inject an API key — the one narrow exception the architecture explicitly defers |
| **Code reuse** | Zero new infrastructure: `ModelInterpreter::new(Arc<dyn AgentBackend>)` | Net-new HTTP client, retry logic, token refresh, credential storage |
| **Testing** | `NoopBackend::with_responses(...)` provides deterministic tests with no real CLI | Would require mocking an HTTP endpoint |
| **Consistency** | Planner uses the same "spawn a pre-authenticated CLI subprocess" model as Developer and Reviewer | Diverges from the architectural seam |

The decisive factor is credential-handling: a direct API would require Makina to manage a model API key. This is explicitly deferred by the architecture, documented in `docs/spec/acp-auth.md` §6. Reusing `AgentBackend` means the Planner inherits the Zed auth model at no extra cost.

---

## 3. Credential / Auth Path

The auth path for `OneShotAgent`:

```
user → `gemini auth login` (or equivalent CLI sign-in)
         │
         │  credentials stored in ~/.config/gemini/ (CLI-owned)
         │
         ▼
makina spawns `gemini --acp` subprocess
         │
         │  subprocess inherits parent environment (Zed auth model)
         │  CLI reads its own credentials — Makina never touches them
         │
         ▼
AcpBackend → AgentSession → ModelInterpreter
         │
         │  one-shot: spawn → prompt → collect → terminate
         │
         ▼
TaskGraph (JSON)
```

Key properties:
- Makina holds **no credentials** — no API keys, no tokens, no secrets.
- The ACP subprocess inherits the parent environment without `env_clear()` (see `crates/makina-acp/src/client.rs`).
- Only non-secret operational variables (e.g. `MAKINA_LOG=debug`) may be layered on top.
- Full audit in `docs/spec/acp-auth.md` §3.

---

## 4. Implementation: `ModelInterpreter`

Location: `crates/makina-core/src/interpreter.rs`

```
ModelInterpreter::new(Arc<dyn AgentBackend>)
    │
    │  interpret(slug, source_text)
    │
    ├─ 1. spawn(SessionConfig { system_prompt: PLANNER_SYSTEM_PROMPT, … })
    ├─ 2. prompt("Interpret … slug `{slug}` … output ONLY the JSON task graph …\n\n{source_text}")
    ├─ 3. collect TextChunk events until TurnComplete → raw_response
    ├─ 4. extract_json_object(raw_response)  // strips fences + prose
    ├─ 5. serde_json::from_str(json_str)     // deserialize into TaskGraph
    ├─ 6. graph.validate()                    // structural validation
    └─ 7. terminate session
```

### 4.1 Planner System Prompt

The Planner system prompt (`PLANNER_SYSTEM_PROMPT` constant) is the Planner's "role prompt":
- Identifies the agent as the task-graph-extraction function of Makina.
- Instructs it to emit **only** the JSON object — no prose, no fences.
- Includes the schema inline so the model knows the exact structure.

This is distinct from Developer and Reviewer role prompts (task 19).

The system prompt is a `pub const` so it can be overridden via `ModelInterpreter::with_system_prompt(...)` for testing or custom Planner roles.

### 4.2 Robust JSON Extraction

Real models frequently wrap output in ` ```json ``` ` code fences or add explanatory prose. The extraction pipeline:

1. `extract_json_object(text)`: scans byte-by-byte, tracks brace depth and string boundaries, returns the first outermost `{ … }` substring.
2. `serde_json::from_str`: standard deserialization into `TaskGraph`.
3. `TaskGraph::validate()`: structural checks (unique ids, no dangling edges).

On any failure in steps 1–3: `InterpretError::ModelResponseInvalid` or `InterpretError::ValidationFailed` — never a panic.

### 4.3 Composition with `EdgeInferrer`

`ModelInterpreter` composes *under* `EdgeInferrer` (task 17):

```rust
let interpreter = EdgeInferrer::new(Arc::new(ModelInterpreter::new(backend)));
```

Cross-cutting edge inference remains a separate concern. The `ModelInterpreter` only parses the model's explicit `depends_on` declarations; `EdgeInferrer` adds inferred edges on top.

---

## 5. Mechanism Builder: `build_planner_interpreter`

Location: `crates/makina-core/src/interpreter.rs`

```rust
pub fn build_planner_interpreter(
    mechanism: &PlannerMechanism,
    backend: Option<Arc<dyn AgentBackend>>,
) -> Result<Arc<dyn TaskListInterpreter>, InterpretError>
```

| `mechanism` | `backend` | Result |
|-------------|-----------|--------|
| `OneShotAgent` | `Some(b)` | `ModelInterpreter::new(b)` (production path) |
| `OneShotAgent` | `None` | `StructuredTextInterpreter::new()` (offline / test fallback) |
| `DirectApi` | any | `Err(InterpretError::MechanismNotSupported { mechanism: "direct-api" })` |

The caller wraps the result in `EdgeInferrer` if cross-cutting edge inference is desired.

---

## 6. Deferred: `DirectApi`

`PlannerMechanism::DirectApi` is retained in the enum for forward-compatibility but is **not implemented**. Selecting it produces:

```
InterpretError::MechanismNotSupported { mechanism: "direct-api" }
```

Implementing `DirectApi` would require:
1. An HTTP client (e.g. `reqwest`) added to `makina-core`.
2. A credential source: environment variable (`ANTHROPIC_API_KEY`, etc.) or a secrets store.
3. Rate-limit handling, retry logic, and streaming response parsing.
4. Documentation of the credential-handling approach, kept separate from the ACP subprocess path.

This is the one credential exception explicitly deferred by the architecture. It must be tackled in a dedicated task and must not bleed into the ACP subprocess auth model.

---

## 7. Acceptance Criterion: Real Model Call

The acceptance test is in `crates/makina-acp/tests/model_interpreter_real.rs`:

```bash
# Google Gemini CLI (must be signed in via `gemini auth login`):
MAKINA_ACP_CMD=gemini MAKINA_ACP_ARGS=--acp \
    cargo test -p makina-acp --test model_interpreter_real -- --ignored --nocapture
```

**Verified on 2026-05-29** against `gemini --acp`:
- `interpret()` returned `Ok(graph)`.
- `graph.slug == "sample-project"`.
- `graph.tasks` = 2 tasks: `scaffold-repo` (no deps) and `add-readme` (depends on `scaffold-repo`).
- `validate()` passed.
- Elapsed: ~27 s.

---

## 8. Files Changed (Task 18)

| File | Change |
|------|--------|
| `crates/makina-core/src/interpreter.rs` | Added `InterpretError::{BackendError, ModelResponseInvalid, MechanismNotSupported}`; `ModelInterpreter`; `PLANNER_SYSTEM_PROMPT`; `extract_json_object`; `parse_model_response`; `build_planner_interpreter`; unit tests for all new code. |
| `crates/makina-core/src/config.rs` | Finalised `PlannerConfig` and `PlannerMechanism` doc comments (removed "provisional" caveat; noted `DirectApi` as deferred-not-implemented). |
| `crates/makina-core/Cargo.toml` | Moved `serde_json` from `dev-dependencies` to `dependencies` (needed by `ModelInterpreter`). |
| `crates/makina-acp/tests/model_interpreter_real.rs` | New `#[ignore]`d integration test for the real model call. |
| `docs/spec/planner-model-mechanism.md` | This document. |
