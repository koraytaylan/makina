# Plan 0044 — Agent Coverage & Compatibility — Verified ACP Agent Support — status

Task-level status lives here; the roll-up row in [../STATUS.md](../STATUS.md) must stay in sync.

**Status:** 📋 Planned.

_Last updated: 2026-07-05, against develop._

- **Goal:** Makina's ACP agent support is visible and verified: string-id agent requests are answered (no hung turns), the registry lists ten correctly-launched ACP agents, every registry-derived surface stays in sync, and the README publishes a compatibility matrix — all gates green.
- **Root cause:** ACP coverage was invisible and under-verified: `KNOWN_AGENTS` (preflight.rs:24-40) listed only three agents with two wrong default args (claude `--acp`, gemini `--yolo`), and `IncomingMessage.id: Option<u64>` (protocol.rs:81) made any agent→client request with a JSON-RPC string id fail to decode and get silently dropped, hanging the untimed prompt turn.
- **Approach:** Four workstreams: make the JSON-RPC id flexible (RequestId string-or-number) end-to-end through protocol classification and the transport so string-id permission requests are answered; rewrite KNOWN_AGENTS with ten verified launch profiles (drop --yolo, empty claude args, add seven agents, Cursor last); add regression tests keeping the validation message, Doctor scaffold, and shipped config comment in sync with the registry; and publish a README compatibility matrix whose launch column mirrors the registry verbatim.

| WS | Workstream | Tasks | State |
|---|---|---|---|
| 0001 | JSON-RPC Id Correctness | `flexible-jsonrpc-request-id` | 📋 Planned |
| 0002 | Expanded KNOWN_AGENTS Launch Profiles | `expand-known-agents` | 📋 Planned |
| 0003 | Registry-Derived Surface Sync | `sync-scaffold-and-validation`, `sync-shipped-config-comment` | 📋 Planned |
| 0004 | README Compatibility Matrix | `readme-compat-matrix` | 📋 Planned |
