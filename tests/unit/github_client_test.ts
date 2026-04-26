/**
 * Unit tests for `src/github/client.ts`.
 *
 * Each public method is covered by three tests:
 *
 * 1. **Happy path** — verifies the request shape (URL, method, body,
 *    `Authorization` header from the auth double) and the projection of
 *    GitHub's payload into the typed return value.
 * 2. **429 rate-limit retry** — the first response is a `429` with
 *    `Retry-After`; the client sleeps the indicated duration and
 *    retries once. The injected sleep records its argument so the test
 *    asserts the delay deterministically.
 * 3. **5xx propagation** — a `500` propagates immediately (no retry).
 *
 * The tests inject a scripted `fetch` so no real network is touched.
 * The injected `nowMilliseconds`/`sleepMilliseconds` hooks make the
 * rate-limit math deterministic.
 */

import { assertEquals, assertGreaterOrEqual, assertRejects } from "@std/assert";

import {
  type CheckRunSummary,
  DEFAULT_FALLBACK_RETRY_SLEEP_MILLISECONDS,
  DEFAULT_MAX_RETRY_SLEEP_MILLISECONDS,
  GitHubClientImpl,
  type GitHubClientOptions,
  type PullRequestReview,
  type PullRequestReviewComment,
} from "../../src/github/client.ts";
import { makeInstallationId, makeIssueNumber, makeRepoFullName } from "../../src/types.ts";

import { InMemoryGitHubAuth } from "../helpers/in_memory_github_auth.ts";

interface RecordedRequest {
  readonly url: string;
  readonly method: string;
  readonly headers: Record<string, string>;
  readonly body: string | null;
}

/**
 * Per-test scripted fetch + sleep harness. Each call to `enqueue` adds
 * one fake response (or thrown network error) and the harness records
 * the inbound request so assertions can verify URL/method/body.
 */
class FakeFetchHarness {
  readonly recordedRequests: RecordedRequest[] = [];
  readonly recordedSleeps: number[] = [];
  private readonly responseQueue: Array<() => Promise<Response>> = [];
  private currentTimeMilliseconds = 1_700_000_000_000;

  enqueueResponse(
    status: number,
    body: unknown,
    options: { headers?: Record<string, string>; binary?: Uint8Array } = {},
  ): void {
    this.responseQueue.push(() => {
      const headers = new Headers({
        "content-type": options.binary !== undefined ? "application/zip" : "application/json",
        ...(options.headers ?? {}),
      });
      const payload: BodyInit = options.binary !== undefined
        ? (options.binary as unknown as BodyInit)
        : JSON.stringify(body);
      return Promise.resolve(new Response(payload, { status, headers }));
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

  sleep = (milliseconds: number): Promise<void> => {
    this.recordedSleeps.push(milliseconds);
    this.currentTimeMilliseconds += milliseconds;
    return Promise.resolve();
  };

  now = (): number => this.currentTimeMilliseconds;

  setTime(milliseconds: number): void {
    this.currentTimeMilliseconds = milliseconds;
  }
}

interface BuildOptions {
  readonly maxRetrySleepMilliseconds?: number;
}

function buildClient(
  harness: FakeFetchHarness,
  buildOptions: BuildOptions = {},
): { client: GitHubClientImpl; auth: InMemoryGitHubAuth } {
  const auth = new InMemoryGitHubAuth();
  const options: GitHubClientOptions = {
    auth,
    installationId: makeInstallationId(123),
    fetch: harness.fetch,
    sleepMilliseconds: harness.sleep,
    nowMilliseconds: harness.now,
    ...(buildOptions.maxRetrySleepMilliseconds !== undefined
      ? { maxRetrySleepMilliseconds: buildOptions.maxRetrySleepMilliseconds }
      : {}),
  };
  return { client: new GitHubClientImpl(options), auth };
}

const REPO = makeRepoFullName("octocat/hello-world");

// ---------------------------------------------------------------------------
// getIssue
// ---------------------------------------------------------------------------

Deno.test("getIssue: happy path projects GitHub payload into IssueDetails", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(200, {
    number: 42,
    title: "Bug: thing broken",
    body: "Steps to reproduce.",
    state: "open",
  });
  const { client } = buildClient(harness);

  const issue = await client.getIssue(REPO, makeIssueNumber(42));

  assertEquals(issue.number, 42);
  assertEquals(issue.title, "Bug: thing broken");
  assertEquals(issue.body, "Steps to reproduce.");
  assertEquals(issue.state, "open");

  assertEquals(harness.recordedRequests.length, 1);
  const recorded = harness.recordedRequests[0];
  assertEquals(recorded?.method, "GET");
  assertEquals(
    recorded?.url,
    "https://api.github.com/repos/octocat/hello-world/issues/42",
  );
  // Auth header present and non-empty.
  assertEquals(
    recorded?.headers["authorization"]?.startsWith("token "),
    true,
  );
});

Deno.test("getIssue: happy path tolerates a null issue body", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(200, {
    number: 7,
    title: "no body issue",
    body: null,
    state: "closed",
  });
  const { client } = buildClient(harness);
  const issue = await client.getIssue(REPO, makeIssueNumber(7));
  assertEquals(issue.body, "");
  assertEquals(issue.state, "closed");
});

Deno.test("getIssue: 429 with Retry-After backs off and retries once", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(429, { message: "Rate limited" }, {
    headers: { "retry-after": "2" },
  });
  harness.enqueueResponse(200, {
    number: 1,
    title: "ok",
    body: "",
    state: "open",
  });
  const { client } = buildClient(harness);

  const issue = await client.getIssue(REPO, makeIssueNumber(1));

  assertEquals(issue.number, 1);
  assertEquals(harness.recordedRequests.length, 2);
  assertEquals(harness.recordedSleeps, [2_000]);
});

Deno.test("getIssue: 500 propagates immediately without retry", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(500, { message: "boom" });
  const { client } = buildClient(harness);

  await assertRejects(
    () => client.getIssue(REPO, makeIssueNumber(1)),
    Error,
  );
  assertEquals(harness.recordedRequests.length, 1);
  assertEquals(harness.recordedSleeps.length, 0);
});

// ---------------------------------------------------------------------------
// createPullRequest
// ---------------------------------------------------------------------------

Deno.test("createPullRequest: happy path posts to /pulls and projects PR", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(201, {
    number: 9,
    head: { sha: "deadbeef", ref: "feature/x" },
    base: { ref: "main" },
    state: "open",
  });
  const { client } = buildClient(harness);

  const pr = await client.createPullRequest(REPO, {
    headRef: "feature/x",
    baseRef: "main",
    title: "Add x",
    body: "Implements #1.",
  });

  assertEquals(pr.number, 9);
  assertEquals(pr.headSha, "deadbeef");
  assertEquals(pr.headRef, "feature/x");
  assertEquals(pr.baseRef, "main");
  assertEquals(pr.state, "open");

  const recorded = harness.recordedRequests[0];
  assertEquals(recorded?.method, "POST");
  assertEquals(recorded?.url, "https://api.github.com/repos/octocat/hello-world/pulls");
  const parsedBody = recorded?.body !== null && recorded?.body !== undefined
    ? JSON.parse(recorded.body) as Record<string, unknown>
    : {};
  assertEquals(parsedBody.head, "feature/x");
  assertEquals(parsedBody.base, "main");
  assertEquals(parsedBody.title, "Add x");
  assertEquals(parsedBody.body, "Implements #1.");
});

Deno.test("createPullRequest: 429 with X-RateLimit-Reset waits until reset and retries", async () => {
  const harness = new FakeFetchHarness();
  // Set 'now' to t = 1000 seconds; reset = 1003 -> sleep ~3000ms.
  harness.setTime(1_000_000);
  harness.enqueueResponse(429, { message: "primary limit" }, {
    headers: {
      "x-ratelimit-remaining": "0",
      "x-ratelimit-reset": "1003",
    },
  });
  harness.enqueueResponse(201, {
    number: 1,
    head: { sha: "s", ref: "h" },
    base: { ref: "b" },
    state: "open",
  });
  const { client } = buildClient(harness);

  await client.createPullRequest(REPO, {
    headRef: "h",
    baseRef: "b",
    title: "t",
    body: "b",
  });

  assertEquals(harness.recordedRequests.length, 2);
  assertEquals(harness.recordedSleeps.length, 1);
  // 1003s = 1_003_000ms, less now=1_000_000ms = 3_000ms.
  assertEquals(harness.recordedSleeps[0], 3_000);
});

Deno.test("createPullRequest: 502 propagates without retry", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(502, { message: "bad gateway" });
  const { client } = buildClient(harness);
  await assertRejects(() =>
    client.createPullRequest(REPO, {
      headRef: "h",
      baseRef: "b",
      title: "t",
      body: "b",
    })
  );
  assertEquals(harness.recordedRequests.length, 1);
});

// ---------------------------------------------------------------------------
// requestReviewers
// ---------------------------------------------------------------------------

Deno.test("requestReviewers: happy path posts the reviewer list", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(201, {});
  const { client } = buildClient(harness);

  await client.requestReviewers(REPO, makeIssueNumber(7), ["Copilot", "alice"]);

  const recorded = harness.recordedRequests[0];
  assertEquals(recorded?.method, "POST");
  assertEquals(
    recorded?.url,
    "https://api.github.com/repos/octocat/hello-world/pulls/7/requested_reviewers",
  );
  const parsed = recorded?.body !== null && recorded?.body !== undefined
    ? JSON.parse(recorded.body) as Record<string, unknown>
    : {};
  assertEquals(parsed.reviewers, ["Copilot", "alice"]);
});

Deno.test("requestReviewers: 429 retries once and then succeeds", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(429, {}, { headers: { "retry-after": "1" } });
  harness.enqueueResponse(201, {});
  const { client } = buildClient(harness);
  await client.requestReviewers(REPO, makeIssueNumber(1), ["Copilot"]);
  assertEquals(harness.recordedSleeps, [1_000]);
  assertEquals(harness.recordedRequests.length, 2);
});

Deno.test("requestReviewers: 503 propagates", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(503, {});
  const { client } = buildClient(harness);
  await assertRejects(() => client.requestReviewers(REPO, makeIssueNumber(1), ["Copilot"]));
  assertEquals(harness.recordedRequests.length, 1);
});

// ---------------------------------------------------------------------------
// getCombinedStatus
// ---------------------------------------------------------------------------

Deno.test("getCombinedStatus: happy path returns the aggregate state", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(200, { state: "success", sha: "abc" });
  const { client } = buildClient(harness);
  const status = await client.getCombinedStatus(REPO, "abc");
  assertEquals(status.state, "success");
  assertEquals(status.sha, "abc");
  const recorded = harness.recordedRequests[0];
  assertEquals(
    recorded?.url,
    "https://api.github.com/repos/octocat/hello-world/commits/abc/status",
  );
  assertEquals(recorded?.method, "GET");
});

Deno.test("getCombinedStatus: 429 with no headers falls back to constant sleep", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(429, { message: "rate limited" });
  harness.enqueueResponse(200, { state: "pending", sha: "abc" });
  const { client } = buildClient(harness);
  await client.getCombinedStatus(REPO, "abc");
  assertEquals(harness.recordedRequests.length, 2);
  assertEquals(harness.recordedSleeps, [DEFAULT_FALLBACK_RETRY_SLEEP_MILLISECONDS]);
});

Deno.test("getCombinedStatus: 500 propagates", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(500, {});
  const { client } = buildClient(harness);
  await assertRejects(() => client.getCombinedStatus(REPO, "abc"));
  assertEquals(harness.recordedRequests.length, 1);
});

// ---------------------------------------------------------------------------
// listCheckRuns
// ---------------------------------------------------------------------------

Deno.test("listCheckRuns: happy path projects the check_runs array", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(200, {
    check_runs: [
      {
        id: 1,
        name: "build",
        status: "completed",
        conclusion: "success",
        html_url: "https://github.com/check/1",
      },
      {
        id: 2,
        name: "lint",
        status: "in_progress",
        conclusion: null,
        html_url: "https://github.com/check/2",
      },
    ],
  });
  const { client } = buildClient(harness);

  const runs: readonly CheckRunSummary[] = await client.listCheckRuns(REPO, "abc");
  assertEquals(runs.length, 2);
  assertEquals(runs[0]?.name, "build");
  assertEquals(runs[0]?.conclusion, "success");
  assertEquals(runs[1]?.status, "in_progress");
  assertEquals(runs[1]?.conclusion, null);

  const recorded = harness.recordedRequests[0];
  assertEquals(
    recorded?.url,
    "https://api.github.com/repos/octocat/hello-world/commits/abc/check-runs",
  );
});

Deno.test("listCheckRuns: 429 retries with Retry-After", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(429, {}, { headers: { "retry-after": "0.5" } });
  harness.enqueueResponse(200, { check_runs: [] });
  const { client } = buildClient(harness);
  await client.listCheckRuns(REPO, "abc");
  assertEquals(harness.recordedSleeps, [500]);
  assertEquals(harness.recordedRequests.length, 2);
});

Deno.test("listCheckRuns: 500 propagates", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(500, {});
  const { client } = buildClient(harness);
  await assertRejects(() => client.listCheckRuns(REPO, "abc"));
  assertEquals(harness.recordedRequests.length, 1);
});

// ---------------------------------------------------------------------------
// getCheckRunLogs
// ---------------------------------------------------------------------------

Deno.test("getCheckRunLogs: happy path returns the raw bytes", async () => {
  const harness = new FakeFetchHarness();
  const fakeZip = new Uint8Array([0x50, 0x4b, 0x03, 0x04, 0xde, 0xad]);
  harness.enqueueResponse(200, undefined, { binary: fakeZip });
  const { client } = buildClient(harness);

  const bytes = await client.getCheckRunLogs(REPO, 99);
  assertEquals(Array.from(bytes), Array.from(fakeZip));

  const recorded = harness.recordedRequests[0];
  assertEquals(
    recorded?.url,
    "https://api.github.com/repos/octocat/hello-world/check-runs/99/logs",
  );
});

Deno.test("getCheckRunLogs: 429 retries once", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(429, {}, { headers: { "retry-after": "1" } });
  const fakeZip = new Uint8Array([1, 2, 3]);
  harness.enqueueResponse(200, undefined, { binary: fakeZip });
  const { client } = buildClient(harness);
  const bytes = await client.getCheckRunLogs(REPO, 1);
  assertEquals(Array.from(bytes), [1, 2, 3]);
  assertEquals(harness.recordedSleeps, [1_000]);
});

Deno.test("getCheckRunLogs: 500 propagates", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(500, {});
  const { client } = buildClient(harness);
  await assertRejects(() => client.getCheckRunLogs(REPO, 1));
  assertEquals(harness.recordedRequests.length, 1);
});

// ---------------------------------------------------------------------------
// listReviews
// ---------------------------------------------------------------------------

Deno.test("listReviews: happy path projects review entries", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(200, [
    {
      id: 10,
      user: { login: "Copilot" },
      state: "COMMENTED",
      body: "looks ok",
      submitted_at: "2026-04-26T12:00:00Z",
    },
    {
      id: 11,
      user: null,
      state: "APPROVED",
      body: null,
    },
  ]);
  const { client } = buildClient(harness);

  const reviews: readonly PullRequestReview[] = await client.listReviews(
    REPO,
    makeIssueNumber(7),
  );
  assertEquals(reviews.length, 2);
  assertEquals(reviews[0]?.user, "Copilot");
  assertEquals(reviews[0]?.state, "COMMENTED");
  assertEquals(reviews[0]?.submittedAtIso, "2026-04-26T12:00:00Z");
  assertEquals(reviews[1]?.user, "");
  assertEquals(reviews[1]?.body, "");
  assertEquals(reviews[1]?.submittedAtIso, undefined);

  const recorded = harness.recordedRequests[0];
  assertEquals(
    recorded?.url,
    "https://api.github.com/repos/octocat/hello-world/pulls/7/reviews",
  );
});

Deno.test("listReviews: 429 retries once", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(429, {}, { headers: { "retry-after": "1" } });
  harness.enqueueResponse(200, []);
  const { client } = buildClient(harness);
  await client.listReviews(REPO, makeIssueNumber(7));
  assertEquals(harness.recordedSleeps, [1_000]);
  assertEquals(harness.recordedRequests.length, 2);
});

Deno.test("listReviews: 500 propagates", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(500, {});
  const { client } = buildClient(harness);
  await assertRejects(() => client.listReviews(REPO, makeIssueNumber(7)));
  assertEquals(harness.recordedRequests.length, 1);
});

// ---------------------------------------------------------------------------
// listReviewComments
// ---------------------------------------------------------------------------

Deno.test("listReviewComments: happy path projects comment entries", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(200, [
    {
      id: 100,
      pull_request_review_id: 10,
      user: { login: "Copilot" },
      body: "Consider renaming.",
      path: "src/x.ts",
      line: 12,
      in_reply_to_id: null,
      created_at: "2026-04-26T12:00:00Z",
    },
    {
      id: 101,
      pull_request_review_id: 10,
      user: null,
      body: "Replying.",
      path: "src/x.ts",
      line: null,
      in_reply_to_id: 100,
      created_at: "2026-04-26T12:01:00Z",
    },
  ]);
  const { client } = buildClient(harness);

  const comments: readonly PullRequestReviewComment[] = await client
    .listReviewComments(REPO, makeIssueNumber(7));
  assertEquals(comments.length, 2);
  assertEquals(comments[0]?.user, "Copilot");
  assertEquals(comments[0]?.line, 12);
  assertEquals(comments[0]?.inReplyToId, null);
  assertEquals(comments[1]?.user, "");
  assertEquals(comments[1]?.inReplyToId, 100);
  assertEquals(comments[1]?.line, null);

  const recorded = harness.recordedRequests[0];
  assertEquals(
    recorded?.url,
    "https://api.github.com/repos/octocat/hello-world/pulls/7/comments",
  );
});

Deno.test("listReviewComments: 429 retries once", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(429, {}, { headers: { "retry-after": "1" } });
  harness.enqueueResponse(200, []);
  const { client } = buildClient(harness);
  await client.listReviewComments(REPO, makeIssueNumber(7));
  assertEquals(harness.recordedSleeps, [1_000]);
  assertEquals(harness.recordedRequests.length, 2);
});

Deno.test("listReviewComments: 500 propagates", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(500, {});
  const { client } = buildClient(harness);
  await assertRejects(() => client.listReviewComments(REPO, makeIssueNumber(7)));
  assertEquals(harness.recordedRequests.length, 1);
});

// ---------------------------------------------------------------------------
// mergePullRequest
// ---------------------------------------------------------------------------

Deno.test("mergePullRequest: squash mode PUTs to /merge with merge_method", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(200, { merged: true, sha: "abc" });
  const { client } = buildClient(harness);
  await client.mergePullRequest(REPO, makeIssueNumber(7), "squash");
  const recorded = harness.recordedRequests[0];
  assertEquals(recorded?.method, "PUT");
  assertEquals(
    recorded?.url,
    "https://api.github.com/repos/octocat/hello-world/pulls/7/merge",
  );
  const parsed = recorded?.body !== null && recorded?.body !== undefined
    ? JSON.parse(recorded.body) as Record<string, unknown>
    : {};
  assertEquals(parsed.merge_method, "squash");
});

Deno.test("mergePullRequest: rebase mode sends merge_method=rebase", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(200, { merged: true, sha: "abc" });
  const { client } = buildClient(harness);
  await client.mergePullRequest(REPO, makeIssueNumber(7), "rebase");
  const recorded = harness.recordedRequests[0];
  const parsed = recorded?.body !== null && recorded?.body !== undefined
    ? JSON.parse(recorded.body) as Record<string, unknown>
    : {};
  assertEquals(parsed.merge_method, "rebase");
});

Deno.test("mergePullRequest: manual mode is a no-op (no HTTP call)", async () => {
  const harness = new FakeFetchHarness();
  const { client } = buildClient(harness);
  await client.mergePullRequest(REPO, makeIssueNumber(7), "manual");
  assertEquals(harness.recordedRequests.length, 0);
});

Deno.test("mergePullRequest: 429 retries once", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(429, {}, { headers: { "retry-after": "1" } });
  harness.enqueueResponse(200, { merged: true, sha: "abc" });
  const { client } = buildClient(harness);
  await client.mergePullRequest(REPO, makeIssueNumber(7), "squash");
  assertEquals(harness.recordedSleeps, [1_000]);
  assertEquals(harness.recordedRequests.length, 2);
});

Deno.test("mergePullRequest: 500 propagates without retry", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(500, {});
  const { client } = buildClient(harness);
  await assertRejects(() => client.mergePullRequest(REPO, makeIssueNumber(7), "squash"));
  assertEquals(harness.recordedRequests.length, 1);
});

// ---------------------------------------------------------------------------
// Cross-cutting: rate-limit math and edge cases
// ---------------------------------------------------------------------------

Deno.test("retry: a second 429 propagates (only one retry per call)", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(429, {}, { headers: { "retry-after": "1" } });
  harness.enqueueResponse(429, {}, { headers: { "retry-after": "1" } });
  const { client } = buildClient(harness);
  await assertRejects(() => client.getIssue(REPO, makeIssueNumber(1)));
  // Exactly one retry attempted.
  assertEquals(harness.recordedRequests.length, 2);
  assertEquals(harness.recordedSleeps.length, 1);
});

Deno.test("retry: an excessive Retry-After is clamped by maxRetrySleepMilliseconds", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(429, {}, { headers: { "retry-after": "9999999" } });
  harness.enqueueResponse(200, {
    number: 1,
    title: "ok",
    body: "",
    state: "open",
  });
  const { client } = buildClient(harness, { maxRetrySleepMilliseconds: 5_000 });
  await client.getIssue(REPO, makeIssueNumber(1));
  assertEquals(harness.recordedSleeps, [5_000]);
});

Deno.test("retry: defaults clamp to DEFAULT_MAX_RETRY_SLEEP_MILLISECONDS", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(429, {}, { headers: { "retry-after": "9999999" } });
  harness.enqueueResponse(200, {
    number: 1,
    title: "ok",
    body: "",
    state: "open",
  });
  const { client } = buildClient(harness);
  await client.getIssue(REPO, makeIssueNumber(1));
  assertEquals(harness.recordedSleeps[0], DEFAULT_MAX_RETRY_SLEEP_MILLISECONDS);
});

Deno.test("retry: X-RateLimit-Reset that is already in the past sleeps zero", async () => {
  const harness = new FakeFetchHarness();
  // now=2_000_000ms, reset=1000s=1_000_000ms is in the past.
  harness.setTime(2_000_000);
  harness.enqueueResponse(403, { message: "rate limited" }, {
    headers: {
      "x-ratelimit-remaining": "0",
      "x-ratelimit-reset": "1000",
    },
  });
  harness.enqueueResponse(200, {
    number: 1,
    title: "ok",
    body: "",
    state: "open",
  });
  const { client } = buildClient(harness);
  await client.getIssue(REPO, makeIssueNumber(1));
  assertEquals(harness.recordedSleeps, [0]);
});

Deno.test(
  "retry: 403 with X-RateLimit-Remaining > 0 propagates without retry",
  async () => {
    const harness = new FakeFetchHarness();
    harness.enqueueResponse(403, { message: "Forbidden" }, {
      headers: {
        "x-ratelimit-remaining": "100",
        "x-ratelimit-reset": "1000",
      },
    });
    // 403 with quota left is a permission/scope error, not a rate-limit
    // event — see ADR-010. The client must not sleep or retry.
    const { client } = buildClient(harness);
    await assertRejects(() => client.getIssue(REPO, makeIssueNumber(1)));
    assertEquals(harness.recordedRequests.length, 1);
    assertEquals(harness.recordedSleeps.length, 0);
  },
);

Deno.test(
  "retry: 403 with no rate-limit headers propagates without retry",
  async () => {
    const harness = new FakeFetchHarness();
    harness.enqueueResponse(403, { message: "Forbidden" });
    // Bare 403 → permission error, not a rate-limit event.
    const { client } = buildClient(harness);
    await assertRejects(() => client.getIssue(REPO, makeIssueNumber(1)));
    assertEquals(harness.recordedRequests.length, 1);
    assertEquals(harness.recordedSleeps.length, 0);
  },
);

Deno.test(
  "retry: 403 with X-RateLimit-Remaining=0 sleeps until reset and retries",
  async () => {
    const harness = new FakeFetchHarness();
    // now=1_000_000ms; reset=1003 -> sleep ~3000ms.
    harness.setTime(1_000_000);
    harness.enqueueResponse(403, { message: "primary limit" }, {
      headers: {
        "x-ratelimit-remaining": "0",
        "x-ratelimit-reset": "1003",
      },
    });
    harness.enqueueResponse(200, {
      number: 1,
      title: "ok",
      body: "",
      state: "open",
    });
    const { client } = buildClient(harness);
    await client.getIssue(REPO, makeIssueNumber(1));
    assertEquals(harness.recordedRequests.length, 2);
    assertEquals(harness.recordedSleeps, [3_000]);
  },
);

Deno.test("auth: each call mints a token via the injected GitHubAuth", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(200, {
    number: 1,
    title: "ok",
    body: "",
    state: "open",
  });
  const { client, auth } = buildClient(harness);
  await client.getIssue(REPO, makeIssueNumber(1));
  // The auth double records every getInstallationToken call.
  assertEquals(auth.recordedRequests().length, 1);
  // The Authorization header carries the deterministic fake token.
  const recorded = harness.recordedRequests[0];
  assertEquals(
    recorded?.headers["authorization"],
    "token inmemory-token-123-1",
  );
});

Deno.test("retry: a sleep duration is non-negative even with a zero Retry-After", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(429, {}, { headers: { "retry-after": "0" } });
  harness.enqueueResponse(200, {
    number: 1,
    title: "ok",
    body: "",
    state: "open",
  });
  const { client } = buildClient(harness);
  await client.getIssue(REPO, makeIssueNumber(1));
  assertEquals(harness.recordedSleeps.length, 1);
  assertGreaterOrEqual(harness.recordedSleeps[0] ?? -1, 0);
});

Deno.test("retry: a non-rate-limited 4xx propagates without retry", async () => {
  const harness = new FakeFetchHarness();
  harness.enqueueResponse(404, { message: "Not Found" });
  const { client } = buildClient(harness);
  await assertRejects(() => client.getIssue(REPO, makeIssueNumber(1)));
  assertEquals(harness.recordedRequests.length, 1);
  assertEquals(harness.recordedSleeps.length, 0);
});

Deno.test(
  "constructor: custom userAgent and baseUrl are forwarded to Octokit",
  async () => {
    const harness = new FakeFetchHarness();
    harness.enqueueResponse(200, {
      number: 1,
      title: "ok",
      body: "",
      state: "open",
    });
    const auth = new InMemoryGitHubAuth();
    const client = new GitHubClientImpl({
      auth,
      installationId: makeInstallationId(123),
      fetch: harness.fetch,
      sleepMilliseconds: harness.sleep,
      nowMilliseconds: harness.now,
      userAgent: "custom-ua/1.0",
      baseUrl: "https://example.test/api/v3",
    });
    await client.getIssue(REPO, makeIssueNumber(1));
    const recorded = harness.recordedRequests[0];
    assertEquals(
      recorded?.url,
      "https://example.test/api/v3/repos/octocat/hello-world/issues/1",
    );
    // Octokit appends its own version suffix; assert our UA is the prefix.
    assertEquals(
      recorded?.headers["user-agent"]?.startsWith("custom-ua/1.0"),
      true,
    );
  },
);
