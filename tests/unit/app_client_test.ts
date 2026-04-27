/**
 * Unit tests for `src/github/app-client.ts`.
 *
 * The AppClient is the App-level companion to the installation-scoped
 * `GitHubClientImpl`. These tests cover:
 *
 *   1. **`listAppInstallations` happy path** — sends a GET to
 *      `/app/installations` with the JWT minted by the injected auth
 *      strategy and projects the response into the typed
 *      `AppInstallation[]` shape.
 *   2. **`listInstallationRepositories` happy path + pagination** —
 *      mints an installation token, GETs `/installation/repositories`,
 *      and walks pages until a short page is returned.
 *   3. **Auth failure propagation** — anything thrown by the strategy
 *      (factory or hook) surfaces as a `GitHubAppClientError`.
 *   4. **Projection failure propagation** — malformed payloads (non-array
 *      response, missing fields) surface as `GitHubAppClientError`.
 *   5. **HTTP failure propagation** — a 5xx from GitHub propagates as
 *      `GitHubAppClientError` with the original Octokit error on `cause`.
 *
 * No real network or JWT signing happens: the tests inject a scripted
 * `fetch` (Octokit's `request: { fetch }` test seam) and a stub auth
 * strategy that returns deterministic tokens.
 */

import { assertEquals, assertRejects } from "@std/assert";

import {
  type AppClientAuthStrategy,
  createAppClient,
  GitHubAppClientError,
} from "../../src/github/app-client.ts";

interface RecordedRequest {
  readonly url: string;
  readonly method: string;
  readonly headers: Record<string, string>;
  readonly body: string | null;
}

/**
 * Per-test scripted fetch harness. Mirrors the shape used by the
 * installation-scoped client tests (`tests/unit/github_client_test.ts`)
 * but stays local to this module — the AppClient does not need the
 * sleep/now hooks the rate-limit retry harness exposes.
 */
class FakeFetchHarness {
  readonly recordedRequests: RecordedRequest[] = [];
  private readonly responseQueue: Array<() => Promise<Response>> = [];

  enqueueResponse(
    status: number,
    body: unknown,
    options: { headers?: Record<string, string> } = {},
  ): void {
    this.responseQueue.push(() => {
      const headers = new Headers({
        "content-type": "application/json",
        ...(options.headers ?? {}),
      });
      return Promise.resolve(
        new Response(JSON.stringify(body), { status, headers }),
      );
    });
  }

  fetch: typeof fetch = (
    input: string | URL | Request,
    init?: RequestInit,
  ): Promise<Response> => {
    const url = typeof input === "string"
      ? input
      : input instanceof URL
      ? input.toString()
      : input.url;
    const method = init?.method ??
      (typeof input !== "string" && !(input instanceof URL) ? input.method : "GET");
    const headersIn = new Headers(init?.headers ?? {});
    const headers: Record<string, string> = {};
    headersIn.forEach((value, key) => {
      headers[key.toLowerCase()] = value;
    });
    const bodyRaw = init?.body;
    const body: string | null = typeof bodyRaw === "string" ? bodyRaw : null;
    this.recordedRequests.push({ url, method: method ?? "GET", headers, body });
    const next = this.responseQueue.shift();
    if (next === undefined) {
      throw new Error(`FakeFetchHarness: no scripted response for ${method} ${url}`);
    }
    return next();
  };
}

/**
 * Build a deterministic stub strategy that mints `{ token: "jwt-..." }`
 * for App-mode and `{ token: "install-<id>" }` for installation-mode.
 *
 * `authCalls` records every invocation so the assertion can verify the
 * AppClient demanded the right token type at the right boundary.
 */
function createStubStrategy(): {
  strategy: AppClientAuthStrategy;
  authCalls: Array<
    { type: "app" } | { type: "installation"; installationId: number; refresh: true }
  >;
  factoryCalls: Array<{ appId: number; privateKey: string }>;
} {
  const authCalls: Array<
    { type: "app" } | { type: "installation"; installationId: number; refresh: true }
  > = [];
  const factoryCalls: Array<{ appId: number; privateKey: string }> = [];
  const strategy: AppClientAuthStrategy = (factoryOpts) => {
    factoryCalls.push({
      appId: factoryOpts.appId,
      privateKey: factoryOpts.privateKey,
    });
    function hook(options: { readonly type: "app" }): Promise<{ token: string }>;
    function hook(options: {
      readonly type: "installation";
      readonly installationId: number;
      readonly refresh: true;
    }): Promise<{ token: string }>;
    function hook(
      options:
        | { readonly type: "app" }
        | {
          readonly type: "installation";
          readonly installationId: number;
          readonly refresh: true;
        },
    ): Promise<{ token: string }> {
      if (options.type === "app") {
        authCalls.push({ type: "app" });
        return Promise.resolve({ token: "jwt-token" });
      }
      authCalls.push({
        type: "installation",
        installationId: options.installationId,
        refresh: options.refresh,
      });
      return Promise.resolve({ token: `install-${options.installationId}` });
    }
    return hook;
  };
  return { strategy, authCalls, factoryCalls };
}

const COMMON_OPTS = {
  appId: 1234,
  privateKey: "-----BEGIN RSA PRIVATE KEY-----\nstub\n-----END RSA PRIVATE KEY-----",
} as const;

// ---------------------------------------------------------------------------
// listAppInstallations
// ---------------------------------------------------------------------------

Deno.test("listAppInstallations: happy path projects /app/installations payload", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(200, [
    { id: 9876543, account: { login: "octocat" } },
    { id: 12345, account: { login: "myorg" } },
  ]);
  const { strategy, authCalls, factoryCalls } = createStubStrategy();
  const client = createAppClient({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    fetch: harness.fetch,
  });

  const installations = await client.listAppInstallations();

  assertEquals(installations.length, 2);
  assertEquals(installations[0]?.id, 9876543);
  assertEquals(installations[0]?.accountLogin, "octocat");
  assertEquals(installations[1]?.id, 12345);
  assertEquals(installations[1]?.accountLogin, "myorg");

  // The factory was called once with the App credentials.
  assertEquals(factoryCalls.length, 1);
  assertEquals(factoryCalls[0]?.appId, 1234);

  // The hook was asked for an App JWT exactly once.
  assertEquals(authCalls.length, 1);
  assertEquals(authCalls[0]?.type, "app");

  // The request hit `/app/installations` with the JWT as a Bearer token.
  assertEquals(harness.recordedRequests.length, 1);
  const recorded = harness.recordedRequests[0];
  assertEquals(recorded?.method, "GET");
  assertEquals(recorded?.url, "https://api.github.com/app/installations");
  assertEquals(recorded?.headers["authorization"], "Bearer jwt-token");
});

Deno.test("listAppInstallations: tolerates installations with missing account login", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(200, [
    { id: 1, account: null },
    { id: 2 },
    { id: 3, account: { login: "named" } },
  ]);
  const { strategy } = createStubStrategy();
  const client = createAppClient({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    fetch: harness.fetch,
  });

  const installations = await client.listAppInstallations();
  assertEquals(installations.length, 3);
  assertEquals(installations[0]?.id, 1);
  assertEquals(installations[0]?.accountLogin, undefined);
  assertEquals(installations[1]?.id, 2);
  assertEquals(installations[1]?.accountLogin, undefined);
  assertEquals(installations[2]?.accountLogin, "named");
});

Deno.test("listAppInstallations: rejects a non-array payload as projection error", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(200, { not: "an array" });
  const { strategy } = createStubStrategy();
  const client = createAppClient({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    fetch: harness.fetch,
  });

  const error = await assertRejects(
    () => client.listAppInstallations(),
    GitHubAppClientError,
  );
  assertEquals(error.operation, "projectInstallations");
});

Deno.test("listAppInstallations: non-object installation entry rejects with projection error", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(200, [42]);
  const { strategy } = createStubStrategy();
  const client = createAppClient({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    fetch: harness.fetch,
  });

  const error = await assertRejects(
    () => client.listAppInstallations(),
    GitHubAppClientError,
  );
  assertEquals(error.operation, "projectInstallations");
});

Deno.test("listAppInstallations: missing numeric id rejects with descriptive error", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(200, [{ account: { login: "octocat" } }]);
  const { strategy } = createStubStrategy();
  const client = createAppClient({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    fetch: harness.fetch,
  });

  const error = await assertRejects(
    () => client.listAppInstallations(),
    GitHubAppClientError,
  );
  assertEquals(error.operation, "projectInstallations");
});

Deno.test("listAppInstallations: forwards a custom baseUrl to Octokit", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(200, []);
  const { strategy } = createStubStrategy();
  const client = createAppClient({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    fetch: harness.fetch,
    baseUrl: "https://ghe.example.com/api/v3",
  });

  await client.listAppInstallations();
  assertEquals(harness.recordedRequests.length, 1);
  const recorded = harness.recordedRequests[0];
  assertEquals(
    recorded?.url,
    "https://ghe.example.com/api/v3/app/installations",
  );
});

Deno.test("listAppInstallations: 500 propagates as GitHubAppClientError(listAppInstallations)", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(500, { message: "boom" });
  const { strategy } = createStubStrategy();
  const client = createAppClient({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    fetch: harness.fetch,
  });

  const error = await assertRejects(
    () => client.listAppInstallations(),
    GitHubAppClientError,
  );
  assertEquals(error.operation, "listAppInstallations");
});

Deno.test("listAppInstallations: auth-strategy factory failure propagates as createAppAuth error", () => {
  const failingStrategy: AppClientAuthStrategy = () => {
    throw new Error("private key parse failed");
  };
  let caught: unknown;
  try {
    createAppClient({
      ...COMMON_OPTS,
      createAppAuthStrategy: failingStrategy,
    });
  } catch (error) {
    caught = error;
  }
  if (!(caught instanceof GitHubAppClientError)) {
    throw new Error(
      `expected GitHubAppClientError, got ${caught instanceof Error ? caught.message : caught}`,
    );
  }
  assertEquals(caught.operation, "createAppAuth");
});

Deno.test("listAppInstallations: auth-hook rejection propagates as mintAppJwt error", async () => {
  const harness = new FakeFetchHarness();
  // Don't enqueue a fetch response — the call should fail at JWT minting.
  const failingStrategy: AppClientAuthStrategy = () => {
    return () => Promise.reject(new Error("clock skew exceeded"));
  };
  const client = createAppClient({
    ...COMMON_OPTS,
    createAppAuthStrategy: failingStrategy,
    fetch: harness.fetch,
  });

  const error = await assertRejects(
    () => client.listAppInstallations(),
    GitHubAppClientError,
  );
  assertEquals(error.operation, "mintAppJwt");
  assertEquals(harness.recordedRequests.length, 0);
});

// ---------------------------------------------------------------------------
// listInstallationRepositories
// ---------------------------------------------------------------------------

Deno.test("listInstallationRepositories: happy path projects single-page payload", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(200, {
    total_count: 2,
    repositories: [
      { name: "makina", owner: { login: "koraytaylan" } },
      { name: "other", owner: { login: "koraytaylan" } },
    ],
  });
  const { strategy, authCalls } = createStubStrategy();
  const client = createAppClient({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    fetch: harness.fetch,
  });

  const repos = await client.listInstallationRepositories(9876543);
  assertEquals(repos.length, 2);
  assertEquals(repos[0]?.owner, "koraytaylan");
  assertEquals(repos[0]?.name, "makina");
  assertEquals(repos[1]?.name, "other");

  // The hook was asked for an installation token (with `refresh: true`).
  assertEquals(authCalls.length, 1);
  assertEquals(authCalls[0], {
    type: "installation",
    installationId: 9876543,
    refresh: true,
  });

  assertEquals(harness.recordedRequests.length, 1);
  const recorded = harness.recordedRequests[0];
  assertEquals(recorded?.method, "GET");
  // Octokit serialises `per_page` and `page` as query params on a GET.
  assertEquals(
    recorded?.url,
    "https://api.github.com/installation/repositories?per_page=100&page=1",
  );
  assertEquals(recorded?.headers["authorization"], "token install-9876543");
});

Deno.test("listInstallationRepositories: paginates until a short page", async () => {
  const harness = new FakeFetchHarness();
  // Build a first page of exactly 100 repos, then a short second page.
  const fullPage = Array.from({ length: 100 }, (_value, index) => ({
    name: `repo-${index}`,
    owner: { login: "org" },
  }));
  harness.enqueueResponse(200, { total_count: 102, repositories: fullPage });
  harness.enqueueResponse(200, {
    total_count: 102,
    repositories: [
      { name: "tail-1", owner: { login: "org" } },
      { name: "tail-2", owner: { login: "org" } },
    ],
  });
  const { strategy } = createStubStrategy();
  const client = createAppClient({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    fetch: harness.fetch,
  });

  const repos = await client.listInstallationRepositories(42);
  assertEquals(repos.length, 102);
  assertEquals(repos[0]?.name, "repo-0");
  assertEquals(repos[99]?.name, "repo-99");
  assertEquals(repos[100]?.name, "tail-1");
  assertEquals(repos[101]?.name, "tail-2");

  assertEquals(harness.recordedRequests.length, 2);
  assertEquals(
    harness.recordedRequests[0]?.url,
    "https://api.github.com/installation/repositories?per_page=100&page=1",
  );
  assertEquals(
    harness.recordedRequests[1]?.url,
    "https://api.github.com/installation/repositories?per_page=100&page=2",
  );
});

Deno.test("listInstallationRepositories: empty installation returns empty array", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(200, { total_count: 0, repositories: [] });
  const { strategy } = createStubStrategy();
  const client = createAppClient({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    fetch: harness.fetch,
  });

  const repos = await client.listInstallationRepositories(42);
  assertEquals(repos.length, 0);
  assertEquals(harness.recordedRequests.length, 1);
});

Deno.test("listInstallationRepositories: malformed envelope rejects as projectRepositories error", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(200, { not_repositories: [] });
  const { strategy } = createStubStrategy();
  const client = createAppClient({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    fetch: harness.fetch,
  });

  const error = await assertRejects(
    () => client.listInstallationRepositories(42),
    GitHubAppClientError,
  );
  assertEquals(error.operation, "projectRepositories");
});

Deno.test("listInstallationRepositories: scalar repositories envelope rejects with projection error", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(200, "not an envelope");
  const { strategy } = createStubStrategy();
  const client = createAppClient({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    fetch: harness.fetch,
  });

  const error = await assertRejects(
    () => client.listInstallationRepositories(42),
    GitHubAppClientError,
  );
  assertEquals(error.operation, "projectRepositories");
});

Deno.test("listInstallationRepositories: non-object repository entry rejects with projection error", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(200, {
    total_count: 1,
    repositories: [42],
  });
  const { strategy } = createStubStrategy();
  const client = createAppClient({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    fetch: harness.fetch,
  });

  const error = await assertRejects(
    () => client.listInstallationRepositories(42),
    GitHubAppClientError,
  );
  assertEquals(error.operation, "projectRepositories");
});

Deno.test("listInstallationRepositories: missing repository name rejects with descriptive error", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(200, {
    total_count: 1,
    repositories: [{ owner: { login: "octocat" } }],
  });
  const { strategy } = createStubStrategy();
  const client = createAppClient({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    fetch: harness.fetch,
  });

  const error = await assertRejects(
    () => client.listInstallationRepositories(42),
    GitHubAppClientError,
  );
  assertEquals(error.operation, "projectRepositories");
});

Deno.test("listInstallationRepositories: missing owner.login rejects with descriptive error", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(200, {
    total_count: 1,
    repositories: [{ name: "broken", owner: null }],
  });
  const { strategy } = createStubStrategy();
  const client = createAppClient({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    fetch: harness.fetch,
  });

  const error = await assertRejects(
    () => client.listInstallationRepositories(42),
    GitHubAppClientError,
  );
  assertEquals(error.operation, "projectRepositories");
});

Deno.test("listInstallationRepositories: 500 propagates as listInstallationRepositories error", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(500, { message: "internal" });
  const { strategy } = createStubStrategy();
  const client = createAppClient({
    ...COMMON_OPTS,
    createAppAuthStrategy: strategy,
    fetch: harness.fetch,
  });

  const error = await assertRejects(
    () => client.listInstallationRepositories(42),
    GitHubAppClientError,
  );
  assertEquals(error.operation, "listInstallationRepositories");
});

Deno.test("listInstallationRepositories: hook rejection propagates as mintInstallationToken error", async () => {
  const harness = new FakeFetchHarness();
  const failingStrategy: AppClientAuthStrategy = () => {
    return () => Promise.reject(new Error("invalid installation"));
  };
  const client = createAppClient({
    ...COMMON_OPTS,
    createAppAuthStrategy: failingStrategy,
    fetch: harness.fetch,
  });

  const error = await assertRejects(
    () => client.listInstallationRepositories(42),
    GitHubAppClientError,
  );
  assertEquals(error.operation, "mintInstallationToken");
  assertEquals(harness.recordedRequests.length, 0);
});
