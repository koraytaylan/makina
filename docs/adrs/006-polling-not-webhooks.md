# ADR-006: Poll for CI and review changes; no webhooks

## Status

Accepted (2026-04-26).

## Context

The stabilize loop reacts to two kinds of remote changes: CI status updates and new PR review
comments. Two ways to learn about them:

1. **Webhooks** — GitHub POSTs events to an HTTPS endpoint we host.
2. **Polling** — we ask GitHub at a fixed cadence.

Webhooks are the lower-latency option in principle — but a local CLI does not have a public HTTPS
endpoint. Standing one up via tunneling (ngrok, Cloudflare Tunnel) drags in deployment complexity,
an external dependency, and a network attack surface, all for a few seconds of latency.

## Decision

Poll. The Poller runs one timer per task in any `STABILIZING` sub-phase, default cadence 30 s,
honoring `Retry-After` and `X-RateLimit-Reset` headers from GitHub.

## Consequences

**Positive:**

- Zero deployment surface — the CLI is fully local.
- No webhook signature verification, secret rotation, or retry handling.
- Bounded by GitHub's rate limits, which the App auth (ADR-003) gives us plenty of headroom on.

**Negative:**

- Latency: feedback lags by up to one poll interval. Acceptable since the human cycle (waiting for
  CI / Copilot review) is on the order of minutes.
- Burns API calls when many tasks idle. Mitigated by exponential backoff during steady-state and by
  collapsing per-task pollers to a single ticker if the count grows.
