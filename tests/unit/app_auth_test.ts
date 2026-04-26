/**
 * Unit tests for `src/github/app-auth.ts`. Covers the cache lifecycle
 * (hit, miss, refresh on near-expiry), error propagation from the
 * underlying `@octokit/auth-app` strategy, distinct-installation
 * isolation, and concurrent access.
 *
 * Tests inject a stub `createAppAuthStrategy` and an injected clock so
 * that no real network or `Date.now()` reads happen — every expiry path
 * is deterministic.
 */

import { assertEquals, assertNotEquals, assertRejects, assertStrictEquals } from "@std/assert";

import {
  type CreateAppAuthStrategy,
  createGitHubAppAuth,
  GitHubAppAuthError,
  type InstallationAuthHook,
  type InstallationAuthResult,
} from "../../src/github/app-auth.ts";
import { INSTALLATION_TOKEN_REFRESH_LEAD_MILLISECONDS } from "../../src/constants.ts";
import { type InstallationId, makeInstallationId } from "../../src/types.ts";

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

const ONE_HOUR_MILLISECONDS = 60 * 60 * 1_000;

/**
 * Adjustable clock used by every test that exercises expiry behavior.
 *
 * The setter lets a test "advance" virtual time between cache reads.
 */
function createClock(initialMilliseconds: number): {
  now: () => number;
  set: (milliseconds: number) => void;
  advance: (deltaMilliseconds: number) => void;
} {
  let current = initialMilliseconds;
  return {
    now: () => current,
    set: (milliseconds: number) => {
      current = milliseconds;
    },
    advance: (deltaMilliseconds: number) => {
      current += deltaMilliseconds;
    },
  };
}

/**
 * Build a stub strategy that mints deterministic tokens whose `expiresAt`
 * is `nowMilliseconds + ttl` at mint time. Returns the hook plus a counter
 * tracking how many times the auth strategy was *invoked* for tokens.
 */
function createStubStrategy(args: {
  /** The synthetic clock used to compute `expiresAt`. */
  readonly clock: { now: () => number };
  /**
   * TTL applied to each minted token (mirrors GitHub's one-hour install
   * tokens unless overridden).
   */
  readonly tokenTtlMilliseconds?: number;
  /**
   * Optional pre-baked replies. If supplied, the strategy serves these in
   * order until exhausted, then falls back to deterministic minting. Each
   * entry is either a value to return or an error to throw.
   */
  readonly scriptedReplies?: Array<
    | { readonly kind: "value"; readonly value: InstallationAuthResult }
    | { readonly kind: "error"; readonly error: unknown }
  >;
  /** Throw from the strategy *factory* itself (private-key parse failure). */
  readonly factoryError?: unknown;
}): {
  strategy: CreateAppAuthStrategy;
  authCalls: Array<{ installationId: number }>;
  factoryCalls: Array<{ appId: number; privateKey: string; installationId?: number }>;
} {
  const ttl = args.tokenTtlMilliseconds ?? ONE_HOUR_MILLISECONDS;
  const scripted = args.scriptedReplies ? [...args.scriptedReplies] : [];
  const authCalls: Array<{ installationId: number }> = [];
  const factoryCalls: Array<{ appId: number; privateKey: string; installationId?: number }> = [];
  let mintCounter = 0;

  const strategy: CreateAppAuthStrategy = (factoryOpts) => {
    if (factoryOpts.installationId === undefined) {
      factoryCalls.push({
        appId: factoryOpts.appId,
        privateKey: factoryOpts.privateKey,
      });
    } else {
      factoryCalls.push({
        appId: factoryOpts.appId,
        privateKey: factoryOpts.privateKey,
        installationId: factoryOpts.installationId,
      });
    }
    if (args.factoryError !== undefined) {
      throw args.factoryError;
    }

    const hook: InstallationAuthHook = (request) => {
      authCalls.push({ installationId: request.installationId });
      const next = scripted.shift();
      if (next !== undefined) {
        if (next.kind === "error") {
          return Promise.reject(next.error);
        }
        return Promise.resolve(next.value);
      }
      mintCounter += 1;
      const expiresAtMs = args.clock.now() + ttl;
      return Promise.resolve({
        token: `stub-token-${request.installationId}-${mintCounter}`,
        expiresAt: new Date(expiresAtMs).toISOString(),
      });
    };
    return hook;
  };

  return { strategy, authCalls, factoryCalls };
}

const COMMON_OPTS = {
  appId: 1234,
  privateKey: "-----BEGIN RSA PRIVATE KEY-----\nstub\n-----END RSA PRIVATE KEY-----",
  installationId: 9876543,
} as const;

// ---------------------------------------------------------------------------
// Cache hit / miss / refresh
// ---------------------------------------------------------------------------

Deno.test("createGitHubAppAuth: returns the cached token on consecutive hits", async () => {
  const clock = createClock(1_700_000_000_000);
  const { strategy, authCalls } = createStubStrategy({ clock });

  const auth = createGitHubAppAuth({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    nowMilliseconds: clock.now,
  });

  const installation = makeInstallationId(9876543);
  const first = await auth.getInstallationToken(installation);
  const second = await auth.getInstallationToken(installation);
  const third = await auth.getInstallationToken(installation);

  assertEquals(first, second);
  assertEquals(second, third);
  assertEquals(authCalls.length, 1, "underlying strategy should mint once");
});

Deno.test("createGitHubAppAuth: cache miss mints a fresh token on first call", async () => {
  const clock = createClock(0);
  const { strategy, authCalls } = createStubStrategy({ clock });

  const auth = createGitHubAppAuth({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    nowMilliseconds: clock.now,
  });

  const token = await auth.getInstallationToken(makeInstallationId(9876543));
  assertEquals(token.startsWith("stub-token-9876543-"), true);
  assertEquals(authCalls.length, 1);
  assertEquals(authCalls[0]?.installationId, 9876543);
});

Deno.test("createGitHubAppAuth: refreshes when within the lead-time window", async () => {
  const clock = createClock(1_700_000_000_000);
  const { strategy, authCalls } = createStubStrategy({ clock });

  const auth = createGitHubAppAuth({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    nowMilliseconds: clock.now,
  });

  const installation = makeInstallationId(9876543);
  const first = await auth.getInstallationToken(installation);
  // Advance past TTL minus the refresh lead time. The cache should
  // consider the token expired and mint a new one.
  clock.advance(ONE_HOUR_MILLISECONDS - INSTALLATION_TOKEN_REFRESH_LEAD_MILLISECONDS);
  const second = await auth.getInstallationToken(installation);

  assertNotEquals(first, second);
  assertEquals(authCalls.length, 2);
});

Deno.test("createGitHubAppAuth: keeps the cached token just before the lead-time boundary", async () => {
  const clock = createClock(1_700_000_000_000);
  const { strategy, authCalls } = createStubStrategy({ clock });

  const auth = createGitHubAppAuth({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    nowMilliseconds: clock.now,
  });

  const installation = makeInstallationId(9876543);
  const first = await auth.getInstallationToken(installation);
  // Advance to *one millisecond before* the refresh boundary. The cached
  // token's `expiresAt` is `t0 + ONE_HOUR`; the refresh threshold under
  // strict-greater semantics is `t0 + ONE_HOUR - LEAD - 1` ms.
  clock.advance(ONE_HOUR_MILLISECONDS - INSTALLATION_TOKEN_REFRESH_LEAD_MILLISECONDS - 1);
  const second = await auth.getInstallationToken(installation);

  assertEquals(first, second);
  assertEquals(authCalls.length, 1);
});

Deno.test("createGitHubAppAuth: refreshes once the token has fully expired", async () => {
  const clock = createClock(1_700_000_000_000);
  const { strategy, authCalls } = createStubStrategy({ clock });

  const auth = createGitHubAppAuth({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    nowMilliseconds: clock.now,
  });

  const installation = makeInstallationId(9876543);
  await auth.getInstallationToken(installation);
  clock.advance(ONE_HOUR_MILLISECONDS * 2);
  const second = await auth.getInstallationToken(installation);

  assertEquals(second.endsWith("-2"), true);
  assertEquals(authCalls.length, 2);
});

// ---------------------------------------------------------------------------
// Per-installation isolation
// ---------------------------------------------------------------------------

Deno.test("createGitHubAppAuth: distinct installations get distinct cache entries", async () => {
  const clock = createClock(1_700_000_000_000);
  const { strategy, authCalls } = createStubStrategy({ clock });

  const auth = createGitHubAppAuth({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    nowMilliseconds: clock.now,
  });

  const a = await auth.getInstallationToken(makeInstallationId(1));
  const b = await auth.getInstallationToken(makeInstallationId(2));
  const aAgain = await auth.getInstallationToken(makeInstallationId(1));

  assertNotEquals(a, b);
  assertEquals(a, aAgain);
  assertEquals(authCalls.length, 2);
  assertEquals(authCalls[0]?.installationId, 1);
  assertEquals(authCalls[1]?.installationId, 2);
});

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

Deno.test("createGitHubAppAuth: concurrent first calls share one in-flight refresh", async () => {
  const clock = createClock(1_700_000_000_000);
  let resolveAuth: ((value: InstallationAuthResult) => void) | undefined;
  let invocations = 0;
  const strategy: CreateAppAuthStrategy = () => {
    return () => {
      invocations += 1;
      return new Promise((resolve) => {
        resolveAuth = resolve;
      });
    };
  };

  const auth = createGitHubAppAuth({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    nowMilliseconds: clock.now,
  });

  const installation = makeInstallationId(9876543);
  const pendingA = auth.getInstallationToken(installation);
  const pendingB = auth.getInstallationToken(installation);
  // Both calls should be waiting on the same single underlying request.
  if (resolveAuth === undefined) {
    throw new Error("strategy should have been invoked exactly once before resolving");
  }
  resolveAuth({
    token: "concurrent-token",
    expiresAt: new Date(clock.now() + ONE_HOUR_MILLISECONDS).toISOString(),
  });
  const [a, b] = await Promise.all([pendingA, pendingB]);
  assertEquals(a, b);
  assertEquals(a, "concurrent-token");
  assertEquals(invocations, 1);
});

Deno.test("createGitHubAppAuth: in-flight refresh failure does not leak across calls", async () => {
  const clock = createClock(1_700_000_000_000);
  const { strategy } = createStubStrategy({
    clock,
    scriptedReplies: [
      { kind: "error", error: new Error("transient 503") },
      {
        kind: "value",
        value: {
          token: "recovered",
          expiresAt: new Date(clock.now() + ONE_HOUR_MILLISECONDS).toISOString(),
        },
      },
    ],
  });

  const auth = createGitHubAppAuth({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    nowMilliseconds: clock.now,
  });

  const installation = makeInstallationId(9876543);
  await assertRejects(
    () => auth.getInstallationToken(installation),
    GitHubAppAuthError,
    "transient 503",
  );
  // Second call should re-attempt rather than serve a poisoned cache entry.
  const second = await auth.getInstallationToken(installation);
  assertEquals(second, "recovered");
});

// ---------------------------------------------------------------------------
// Error propagation
// ---------------------------------------------------------------------------

Deno.test("createGitHubAppAuth: wraps install-token errors as GitHubAppAuthError", async () => {
  const clock = createClock(0);
  const { strategy } = createStubStrategy({
    clock,
    scriptedReplies: [{
      kind: "error",
      error: new Error("Bad credentials"),
    }],
  });

  const auth = createGitHubAppAuth({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    nowMilliseconds: clock.now,
  });

  const error = await assertRejects(
    () => auth.getInstallationToken(makeInstallationId(9876543)),
    GitHubAppAuthError,
  );
  assertEquals(error.operation, "getInstallationToken");
  assertEquals(
    error.message.startsWith("GitHub App auth failed during getInstallationToken:"),
    true,
  );
  assertEquals(error.message.includes("Bad credentials"), true);
  // Cause is preserved.
  assertStrictEquals((error.cause as Error).message, "Bad credentials");
});

Deno.test("createGitHubAppAuth: rethrows strategy-factory errors as GitHubAppAuthError", () => {
  const { strategy } = createStubStrategy({
    clock: { now: () => 0 },
    factoryError: new Error("invalid PEM"),
  });

  let caught: unknown;
  try {
    createGitHubAppAuth({
      ...COMMON_OPTS,
      createAppAuthStrategy: strategy,
      nowMilliseconds: () => 0,
    });
  } catch (error) {
    caught = error;
  }
  if (!(caught instanceof GitHubAppAuthError)) {
    throw new Error("expected GitHubAppAuthError");
  }
  assertEquals(caught.operation, "createAppAuth");
  assertEquals(caught.message.includes("invalid PEM"), true);
  assertStrictEquals((caught.cause as Error).message, "invalid PEM");
});

Deno.test("createGitHubAppAuth: surfaces non-Error rejection values via stringification", async () => {
  const clock = createClock(0);
  const { strategy } = createStubStrategy({
    clock,
    scriptedReplies: [{ kind: "error", error: { code: "ENETUNREACH" } }],
  });

  const auth = createGitHubAppAuth({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    nowMilliseconds: clock.now,
  });

  const error = await assertRejects(
    () => auth.getInstallationToken(makeInstallationId(9876543)),
    GitHubAppAuthError,
  );
  assertEquals(error.message.includes("ENETUNREACH"), true);
});

Deno.test("createGitHubAppAuth: rejects an unparseable expiresAt timestamp", async () => {
  const clock = createClock(0);
  const { strategy } = createStubStrategy({
    clock,
    scriptedReplies: [{
      kind: "value",
      value: { token: "garbled", expiresAt: "not-a-date" },
    }],
  });

  const auth = createGitHubAppAuth({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    nowMilliseconds: clock.now,
  });

  const error = await assertRejects(
    () => auth.getInstallationToken(makeInstallationId(9876543)),
    GitHubAppAuthError,
  );
  assertEquals(error.operation, "parseExpiresAt");
});

// ---------------------------------------------------------------------------
// Strategy wiring
// ---------------------------------------------------------------------------

Deno.test("createGitHubAppAuth: forwards appId/privateKey/installationId to the strategy", () => {
  const clock = createClock(0);
  const { strategy, factoryCalls } = createStubStrategy({ clock });

  createGitHubAppAuth({
    appId: 4321,
    privateKey: "PEM",
    installationId: 99,
    createAppAuthStrategy: strategy,
    nowMilliseconds: clock.now,
  });

  assertEquals(factoryCalls.length, 1);
  assertEquals(factoryCalls[0]?.appId, 4321);
  assertEquals(factoryCalls[0]?.privateKey, "PEM");
  assertEquals(factoryCalls[0]?.installationId, 99);
});

Deno.test("createGitHubAppAuth: passes the requested installationId on each token call", async () => {
  const clock = createClock(0);
  const { strategy, authCalls } = createStubStrategy({ clock });

  const auth = createGitHubAppAuth({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    nowMilliseconds: clock.now,
  });

  await auth.getInstallationToken(makeInstallationId(11));
  await auth.getInstallationToken(makeInstallationId(22));

  assertEquals(authCalls.map((c) => c.installationId), [11, 22]);
});

// ---------------------------------------------------------------------------
// Type sanity
// ---------------------------------------------------------------------------

Deno.test("createGitHubAppAuth: returned object satisfies the W1 GitHubAuth interface", async () => {
  const clock = createClock(0);
  const { strategy } = createStubStrategy({ clock });
  const auth = createGitHubAppAuth({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    nowMilliseconds: clock.now,
  });
  const installation: InstallationId = makeInstallationId(9876543);
  const result: string = await auth.getInstallationToken(installation);
  assertEquals(typeof result, "string");
});
