# ACP Authentication: the Zed Inheritance Model

This document describes how Makina handles authentication when speaking to an
ACP-compatible agent CLI. The short answer: **Makina holds no credentials and
never drives an authentication flow.** The CLI must be signed in before Makina
spawns it.

---

## 1. The Zed model

Makina follows the authentication model pioneered by Zed Editor's agent-client
integrations:

> The **user** signs in to the agent CLI **once**, via the CLI's own flow.
> Makina then spawns the pre-authenticated CLI as a subprocess and **inherits
> its session** via the inherited environment. Makina never sees, stores,
> transmits, or touches model credentials.

This is the same pattern used when you run a terminal tool that reads
credentials from `~/.config/<agent>/credentials` or an environment variable
set by the CLI's `auth login` command — the sub-process simply inherits the
parent's environment.

---

## 2. Signing in — the user's responsibility

Before running Makina, the operator must authenticate the agent CLI through its
own flow. Examples:

| Agent CLI | Sign-in command |
|-----------|-----------------|
| Google Gemini | `gemini auth login` |
| Claude Code (Anthropic) | `claude` (interactive) |
| Zed claude-code-acp | handled by the Zed IDE login |

After sign-in the CLI typically writes a token or sets up a session that it
reads back on subsequent invocations. That token lives in the CLI's own storage
(e.g. `~/.config/gemini/`, `~/.claude/`), not in Makina's configuration.

---

## 3. How Makina spawns the CLI

`AcpClient::connect(AcpCommand)` (`crates/makina-acp/src/client.rs`) spawns
the agent as a subprocess. The `Command` is constructed without calling
`std::process::Command::env_clear()` or otherwise restricting the environment:

```rust
cmd.args(&command.args)
    .current_dir(&command.working_dir)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
// Zed auth model: inherit the parent environment; only *add* extras.
for (key, value) in &command.env {
    cmd.env(key, value);
}
```

The subprocess **inherits the full parent environment** by default (Tokio's
`Command` inherits the calling process's env unless overridden). The `env`
field on `AcpCommand`/`AcpBackend` only **layers** additional non-secret
operational variables on top (e.g. `MAKINA_LOG=debug`). Makina never:

- sets `*_API_KEY`, `*_TOKEN`, `*_SECRET`, or any credential-flavoured var;
- reads `~/.config/<agent>/credentials` or any agent-specific token store;
- calls the ACP `authenticate` RPC method.

### Audit conclusion (Task 16)

A grep-based audit of `crates/makina-acp` and `crates/makina-core` for all
credential-related patterns (`API_KEY`, `TOKEN`, `SECRET`, `PASSWORD`,
`CREDENTIAL`, `authenticate`, `oauth`, `env::var`, home-dir reads) found:

- All occurrences are in **comments and documentation** explaining the Zed
  model, in `#[ignore]`d test boilerplate reading `MAKINA_ACP_CMD` (not a
  credential), and in `std::env::temp_dir()` / `std::env::current_dir()` for
  test working directories.
- **No production code** reads, stores, sets, or injects model credentials.
- The only `home_dir()` call in the codebase is in `makina-core/src/config.rs`
  for resolving `~/.makina/config.toml` — the Makina *operator* config, which
  contains the agent binary path and arguments, not credentials.

---

## 4. The `authMethods` field — observability, not a flow

The ACP `initialize` response includes an optional `authMethods` array. Each
entry is an object with a `type` field (e.g. `"oauth"`, `"apiKey"`) plus
agent-specific metadata.

Makina **retains** these after the handshake and exposes them via:

```rust
pub fn auth_methods(&self) -> &[crate::protocol::AuthMethod]
```

This is an **observability accessor only**. Makina never calls the ACP
`authenticate` method. The accessor exists to:

1. **Log** what auth mechanism the agent uses (visible at `tracing::debug!`
   level): "ACP initialize: agent advertises authMethods [oauth]".
2. **Give operators a signal** if an agent is misconfigured — an agent that is
   not signed in will typically fail with `AcpError::Rpc` (a JSON-RPC error
   response to `initialize`) or `AcpError::AgentExited` (process exit before
   completing the handshake). The agent's stderr (forwarded line-by-line as
   `[acp-agent] …` to this process's stderr) usually contains the
   human-readable authentication error.

An empty `authMethods` list means the agent requires no explicit auth, or is
already authenticated and considers auth transparent. A non-empty list is
purely diagnostic.

### If an agent is not authenticated

The failure looks like one of these:

- `AcpError::Rpc` — the agent answered `initialize` with a JSON-RPC error
  object (e.g. `{ "code": -32000, "message": "Not authenticated" }`).
- `AcpError::AgentExited` — the agent process exited before completing the
  handshake (usually because it printed an error to stderr and exited 1).

**Resolution**: sign in via the CLI's own command (e.g. `gemini auth login`,
`claude` interactive), then retry. Do **not** try to pass credentials to
Makina; the model is that the CLI is already signed in.

---

## 5. Running the live acceptance test

The `#[ignore]`d tests in `crates/makina-acp/tests/real_cli.rs` are the live
proof that "a run works against a pre-authenticated CLI". Run them on a machine
where the CLI is already signed in:

```bash
# Google Gemini CLI (must be signed in via `gemini auth login`):
MAKINA_ACP_CMD=gemini MAKINA_ACP_ARGS=--acp \
    cargo test -p makina-acp --test real_cli -- --ignored --nocapture

# Zed's Claude Code ACP adapter via npx:
MAKINA_ACP_CMD=npx \
MAKINA_ACP_ARGS='-y,@zed-industries/claude-code-acp@latest' \
    cargo test -p makina-acp --test real_cli -- --ignored --nocapture
```

`MAKINA_ACP_CMD` is the program; `MAKINA_ACP_ARGS` is a comma-separated arg
list. The test asserts that the turn completes and returns non-empty text; it
does not validate the model response content (non-deterministic).

---

## 6. The one future exception (deferred)

The Planner actor may eventually call the model API directly rather than via an
ACP subprocess (the `PlannerMechanism::DirectApi` variant in `config.rs`). In
that case a credential — an Anthropic API key, etc. — would need to reach the
Planner. That path is:

- **Not yet implemented** (task 18: `planner-model-mechanism`).
- **Out of scope** for the Zed-model auth guarantee, which covers only the
  ACP subprocess path.

When task 18 is tackled it must document its credential-handling approach
separately and ensure it does not bleed into the ACP subprocess path.

---

## 7. Files changed in Task 16

| File | Change |
|------|--------|
| `crates/makina-acp/src/protocol.rs` | Added `auth_methods: Vec<AuthMethod>` to `InitializeResult`; new `AuthMethod` type |
| `crates/makina-acp/src/client.rs` | Retain `auth_methods` from `initialize`; `AcpClient::auth_methods()` accessor; tracing log |
| `crates/makina-acp/src/lib.rs` | Re-export `AuthMethod` |
| `crates/makina-acp/tests/common/mod.rs` | `MockBehavior::auth_methods` field; default = `[{ "type": "oauth" }]` |
| `crates/makina-acp/tests/acp_auth_verify.rs` | New integration tests (auth observability, no-credential injection, env inheritance) |
| `crates/makina-acp/tests/mock_exchange.rs` | Added `..MockBehavior::default()` to existing struct literals |
| `crates/makina-acp/tests/backend_trait.rs` | Added `..MockBehavior::default()` to existing struct literals |
| `Cargo.toml` (workspace) | Added `tracing = "0.1"` |
| `crates/makina-acp/Cargo.toml` | Added `tracing = { workspace = true }` |
| `docs/spec/acp-auth.md` | This document |
