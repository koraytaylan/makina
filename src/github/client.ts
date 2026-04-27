/**
 * github/client.ts — high-level typed GitHub client used by the supervisor.
 *
 * Implements {@link GitHubClient} from `src/types.ts` and adds the four
 * stabilize-phase methods (`listCheckRuns`, `getCheckRunLogs`,
 * `listReviews`, `listReviewComments`) the Wave 4 conversations and CI
 * loops need. Those four extensions live behind
 * {@link StabilizeGitHubClient}, an additive interface exported from this
 * module; the Wave 1 {@link GitHubClient} contract in `src/types.ts`
 * stays frozen. Built on `@octokit/core` with a {@link GitHubAuth}
 * strategy injected: every request resolves an installation token via
 * `getInstallationToken` immediately before the call so token rotation
 * is automatic.
 *
 * **Rate-limit awareness.** The retry policy intentionally fires only
 * when GitHub gave us a clear rate-limit signal (see ADR-011):
 *
 * - `Retry-After` present (any status) — sleep that duration, retry once.
 * - `X-RateLimit-Remaining: 0` paired with `X-RateLimit-Reset` (any
 *   status) — sleep until the reset, retry once.
 * - `429` with neither header — sleep
 *   {@link DEFAULT_FALLBACK_RETRY_SLEEP_MILLISECONDS} and retry once;
 *   the `429` itself is the rate-limit signal.
 * - `403` with neither header — propagate immediately. A 403 without
 *   rate-limit headers is a permission/scope error, not a quota event,
 *   and retrying just adds latency before the same failure surfaces.
 *
 * 5xx responses propagate immediately — they indicate transient GitHub
 * trouble that the supervisor's higher-level retry policy handles.
 *
 * **Test seam.** `Octokit` exposes a `request: { fetch }` option;
 * `tests/unit/github_client_test.ts` injects a scripted fake fetch so
 * unit tests never touch the network. The {@link GitHubClientOptions}
 * surface forwards `fetch` and `nowMilliseconds`/`sleepMilliseconds`
 * hooks so the rate-limit branches are deterministic.
 *
 * @module
 */

import { Octokit } from "@octokit/core";

import {
  type CombinedStatus,
  type GitHubAuth,
  type GitHubClient,
  type InstallationId,
  type IssueDetails,
  type IssueNumber,
  makeIssueNumber,
  type MergeMode,
  type PullRequestDetails,
  type RepoFullName,
} from "../types.ts";

// ---------------------------------------------------------------------------
// Public extra payload types (the four methods not in the W1 interface)
// ---------------------------------------------------------------------------

/**
 * Aggregate result of a single GitHub check run.
 *
 * Mirrors the subset of the [Check Runs API](https://docs.github.com/en/rest/checks/runs)
 * payload the stabilize loop reads.
 */
export interface CheckRunSummary {
  /** Numeric check-run id; stable per push. */
  readonly id: number;
  /** Workflow / check name (e.g. `"build"`, `"ci/lint"`). */
  readonly name: string;
  /** Lifecycle status of the run. */
  readonly status: "queued" | "in_progress" | "completed";
  /**
   * Final outcome once `status === "completed"`; `null` while the run is
   * still queued or in progress.
   */
  readonly conclusion:
    | "success"
    | "failure"
    | "neutral"
    | "cancelled"
    | "timed_out"
    | "action_required"
    | "stale"
    | "skipped"
    | null;
  /** URL of the check-run details page on GitHub. */
  readonly htmlUrl: string;
}

/**
 * One review submitted on a pull request.
 *
 * Mirrors the subset of the [Pulls Reviews API](https://docs.github.com/en/rest/pulls/reviews)
 * the stabilize loop's conversations phase reads.
 */
export interface PullRequestReview {
  /** Review id; stable. */
  readonly id: number;
  /** Login of the reviewer (or bot). */
  readonly user: string;
  /** Verdict the reviewer left. */
  readonly state:
    | "APPROVED"
    | "CHANGES_REQUESTED"
    | "COMMENTED"
    | "DISMISSED"
    | "PENDING";
  /** Free-text review body. */
  readonly body: string;
  /** ISO-8601 timestamp the review was submitted, when present. */
  readonly submittedAtIso?: string;
}

/**
 * One inline review comment on a pull request.
 *
 * Mirrors the subset of the [Pulls Comments API](https://docs.github.com/en/rest/pulls/comments)
 * the stabilize loop reads. Threading metadata
 * (`pull_request_review_id`, `in_reply_to_id`) is preserved so the
 * conversations phase can group comments by thread; the optional
 * {@link PullRequestReviewComment.threadNodeId} carries the GraphQL
 * node id the
 * {@link StabilizeGitHubClient.resolveReviewThread} mutation requires
 * once the supervisor pushes a fix for the thread.
 */
export interface PullRequestReviewComment {
  /** Comment id; stable. */
  readonly id: number;
  /** Review id this comment belongs to, when present. */
  readonly pullRequestReviewId: number | null;
  /** Login of the commenter. */
  readonly user: string;
  /** Comment body (markdown). */
  readonly body: string;
  /** Repository file path the comment is anchored to. */
  readonly path: string;
  /** Diff line the comment targets, when present. */
  readonly line: number | null;
  /** Parent comment id when this is a reply, otherwise `null`. */
  readonly inReplyToId: number | null;
  /**
   * GraphQL node id of the review thread this comment belongs to, when
   * known. Required by
   * {@link StabilizeGitHubClient.resolveReviewThread}; the REST Pulls
   * Comments API does not expose it directly, so the conversations
   * phase treats `undefined` as "thread cannot be resolved" and skips
   * the resolve call rather than calling the mutation with a guessed id.
   */
  readonly threadNodeId?: string;
  /** ISO-8601 timestamp the comment was created. */
  readonly createdAtIso: string;
}

// ---------------------------------------------------------------------------
// Stabilize-phase additive interface
// ---------------------------------------------------------------------------

/**
 * Stabilize-phase extension of {@link GitHubClient}.
 *
 * The W1 `GitHubClient` contract in `src/types.ts` is intentionally
 * minimal — every interface there is the surface every Wave 2 branch
 * builds in parallel and consumer waves cannot cheaply reshape it.
 * The methods Wave 4's stabilize loop needs (per-check-run reads
 * for the CI phase, review/comment reads + thread resolution for the
 * conversations phase) are additive, so we expose them on a separate
 * interface that **extends** `GitHubClient` instead. Consumers (Wave
 * 4's supervisor, the in-memory double
 * `tests/helpers/in_memory_github_client.ts`) depend on this interface
 * rather than the concrete {@link GitHubClientImpl} class.
 *
 * @example
 * ```ts
 * function startStabilizeLoop(client: StabilizeGitHubClient) {
 *   // Type-checked access to both W1 + W4 methods.
 * }
 * ```
 */
export interface StabilizeGitHubClient extends GitHubClient {
  /**
   * List individual check runs (Checks API) attached to `sha`.
   *
   * @param repo Target repository.
   * @param sha Commit SHA.
   * @returns The check runs in GitHub's natural order.
   */
  listCheckRuns(
    repo: RepoFullName,
    sha: string,
  ): Promise<readonly CheckRunSummary[]>;
  /**
   * Fetch the raw logs ZIP for a single check run, as bytes.
   *
   * @param repo Target repository.
   * @param checkRunId Check-run id from {@link StabilizeGitHubClient.listCheckRuns}.
   * @returns The ZIP bytes.
   */
  getCheckRunLogs(repo: RepoFullName, checkRunId: number): Promise<Uint8Array>;
  /**
   * List submitted reviews on a pull request, in submission order.
   *
   * @param repo Target repository.
   * @param pullRequestNumber Pull-request number.
   * @returns Reviews in their submission order.
   */
  listReviews(
    repo: RepoFullName,
    pullRequestNumber: IssueNumber,
  ): Promise<readonly PullRequestReview[]>;
  /**
   * List inline review comments on a pull request, in creation order.
   *
   * @param repo Target repository.
   * @param pullRequestNumber Pull-request number.
   * @returns Inline review comments in their creation order.
   */
  listReviewComments(
    repo: RepoFullName,
    pullRequestNumber: IssueNumber,
  ): Promise<readonly PullRequestReviewComment[]>;
  /**
   * Resolve a single review thread via GitHub's GraphQL API.
   *
   * GitHub does not expose a REST endpoint for resolving review threads
   * — the action is GraphQL-only via the `resolveReviewThread` mutation.
   * The thread id is the **GraphQL node id**, not the REST review-id;
   * the conversations phase obtains it from the threads payload it
   * groups per pull request.
   *
   * See ADR-019 for the dependency-policy rationale (we route the
   * mutation through `@octokit/core`'s built-in `graphql()` rather than
   * pulling in `@octokit/graphql` as a separate dependency).
   *
   * @param threadId GraphQL node id of the review thread to resolve.
   */
  resolveReviewThread(threadId: string): Promise<void>;
}

// ---------------------------------------------------------------------------
// Options surface
// ---------------------------------------------------------------------------

/**
 * Construction options for {@link GitHubClient}.
 *
 * Every field except `auth` and `installationId` is optional; the
 * default values reproduce real-world behavior. The `now`,
 * `sleep`, and `fetch` hooks exist so the unit tests can drive the
 * rate-limit retry branches without sleeping for real or hitting the
 * network.
 */
export interface GitHubClientOptions {
  /** Installation-token mint; usually `src/github/app-auth.ts`. */
  readonly auth: GitHubAuth;
  /** Installation id this client makes requests on behalf of. */
  readonly installationId: InstallationId;
  /**
   * `User-Agent` to send. Defaults to `makina-github-client/0.1`. GitHub
   * rejects requests without a `User-Agent`.
   */
  readonly userAgent?: string;
  /**
   * Override the base URL (defaults to `https://api.github.com`). Useful
   * for testing against GitHub Enterprise stand-ins.
   */
  readonly baseUrl?: string;
  /**
   * Inject the underlying `fetch`. Tests pass a scripted fake;
   * production leaves this undefined and `@octokit/core` uses the
   * runtime `fetch`.
   */
  readonly fetch?: typeof fetch;
  /**
   * Wall-clock source used to compute `X-RateLimit-Reset` waits.
   * Defaults to `() => Date.now()`.
   */
  readonly nowMilliseconds?: () => number;
  /**
   * Sleep primitive used between a rate-limited response and the retry.
   * Defaults to `globalThis.setTimeout`-based sleep.
   */
  readonly sleepMilliseconds?: (milliseconds: number) => Promise<void>;
  /**
   * Upper bound on a single retry sleep, in milliseconds. Caps absurd
   * `Retry-After`/`X-RateLimit-Reset` values so a buggy GitHub never
   * stalls the supervisor for an hour. Defaults to
   * {@link DEFAULT_MAX_RETRY_SLEEP_MILLISECONDS}.
   */
  readonly maxRetrySleepMilliseconds?: number;
}

/**
 * Default cap on a single rate-limit retry sleep, in milliseconds.
 *
 * Five minutes is well above any realistic `Retry-After` GitHub returns
 * for secondary limits but well below the supervisor's settling window
 * upper bound. See {@link GitHubClientOptions.maxRetrySleepMilliseconds}.
 */
export const DEFAULT_MAX_RETRY_SLEEP_MILLISECONDS = 5 * 60 * 1_000;

/**
 * Default fallback sleep when GitHub answered `429` with no rate-limit
 * headers (the status itself is the signal). One second is conservative
 * — enough to clear a transient burst without serializing the supervisor
 * on every retry. `403` does not use this fallback; without rate-limit
 * headers a `403` is treated as a permission error and propagates. See
 * ADR-011.
 */
export const DEFAULT_FALLBACK_RETRY_SLEEP_MILLISECONDS = 1_000;

const HEADER_RETRY_AFTER = "retry-after";
const HEADER_RATE_LIMIT_REMAINING = "x-ratelimit-remaining";
const HEADER_RATE_LIMIT_RESET = "x-ratelimit-reset";

/**
 * GraphQL mutation used by
 * {@link GitHubClientImpl.resolveReviewThread} to mark a single review
 * thread as resolved. Centralised so the unit test asserts against the
 * exact body the production code sends, and any future amendment
 * (`unresolveReviewThread`, additional return fields) lives next to it.
 *
 * The mutation only requires the `threadId` input; the response payload
 * is read for shape (so a misconfigured GraphQL endpoint surfaces as a
 * type error rather than silent success). See ADR-019 for the
 * dependency-policy rationale (no `@octokit/graphql` dependency added).
 */
const RESOLVE_REVIEW_THREAD_MUTATION = `mutation ResolveReviewThread($threadId: ID!) {
  resolveReviewThread(input: { threadId: $threadId }) {
    thread { id }
  }
}`;

/**
 * Default `User-Agent` string. GitHub demands a non-empty UA on every
 * request; ours identifies the daemon and its version line.
 */
const DEFAULT_USER_AGENT = "makina-github-client/0.1";

// ---------------------------------------------------------------------------
// GitHubClient implementation
// ---------------------------------------------------------------------------

/**
 * Concrete {@link GitHubClient} backed by `@octokit/core`.
 *
 * Construct one per installation. Methods accept branded `RepoFullName`
 * / `IssueNumber` arguments so the call sites cannot accidentally swap
 * an installation id in for an issue number.
 *
 * @example
 * ```ts
 * import { GitHubAppAuth } from "./app-auth.ts";
 * const auth = new GitHubAppAuth({ ... });
 * const client = new GitHubClientImpl({
 *   auth,
 *   installationId: makeInstallationId(123),
 * });
 * const issue = await client.getIssue(makeRepoFullName("a/b"), makeIssueNumber(1));
 * ```
 */
export class GitHubClientImpl implements StabilizeGitHubClient {
  private readonly auth: GitHubAuth;
  private readonly installationId: InstallationId;
  private readonly octokit: Octokit;
  private readonly nowMilliseconds: () => number;
  private readonly sleepMilliseconds: (milliseconds: number) => Promise<void>;
  private readonly maxRetrySleepMilliseconds: number;

  /**
   * Construct a {@link GitHubClientImpl}.
   *
   * @param options Auth, installation, and (optionally) test seams. See
   *   {@link GitHubClientOptions} for the full surface.
   */
  constructor(options: GitHubClientOptions) {
    this.auth = options.auth;
    this.installationId = options.installationId;
    this.nowMilliseconds = options.nowMilliseconds ?? (() => Date.now());
    this.sleepMilliseconds = options.sleepMilliseconds ?? defaultSleep;
    this.maxRetrySleepMilliseconds = options.maxRetrySleepMilliseconds ??
      DEFAULT_MAX_RETRY_SLEEP_MILLISECONDS;

    const userAgent = options.userAgent ?? DEFAULT_USER_AGENT;
    const octokitOptions: Record<string, unknown> = { userAgent };
    if (options.baseUrl !== undefined) {
      octokitOptions.baseUrl = options.baseUrl;
    }
    if (options.fetch !== undefined) {
      octokitOptions.request = { fetch: options.fetch };
    }
    this.octokit = new Octokit(octokitOptions);
  }

  /**
   * Fetch issue details.
   *
   * @param repo Target repository.
   * @param issueNumber Issue number within `repo`.
   * @returns A subset of the GitHub issue payload the supervisor reads.
   */
  async getIssue(repo: RepoFullName, issueNumber: IssueNumber): Promise<IssueDetails> {
    const { owner, name } = splitRepo(repo);
    const response = await this.requestWithRetry(
      "GET /repos/{owner}/{repo}/issues/{issue_number}",
      { owner, repo: name, issue_number: issueNumber },
    );
    const data = response.data as IssueResponse;
    return {
      number: makeIssueNumber(data.number),
      title: data.title,
      body: data.body ?? "",
      state: data.state,
    };
  }

  /**
   * Open a pull request.
   *
   * @param repo Target repository.
   * @param args Head/base refs and PR metadata.
   * @returns The freshly-opened PR's id, head SHA, and refs.
   */
  async createPullRequest(
    repo: RepoFullName,
    args: { headRef: string; baseRef: string; title: string; body: string },
  ): Promise<PullRequestDetails> {
    const { owner, name } = splitRepo(repo);
    const response = await this.requestWithRetry(
      "POST /repos/{owner}/{repo}/pulls",
      {
        owner,
        repo: name,
        head: args.headRef,
        base: args.baseRef,
        title: args.title,
        body: args.body,
      },
    );
    return projectPullRequest(response.data as PullRequestResponse);
  }

  /**
   * Request reviewers (e.g. `["Copilot"]`) on a pull request. The
   * GitHub API treats this as fire-and-forget; no body is returned to
   * the caller.
   *
   * @param repo Target repository.
   * @param pullRequestNumber Pull-request number.
   * @param reviewers GitHub logins to request review from.
   */
  async requestReviewers(
    repo: RepoFullName,
    pullRequestNumber: IssueNumber,
    reviewers: readonly string[],
  ): Promise<void> {
    const { owner, name } = splitRepo(repo);
    await this.requestWithRetry(
      "POST /repos/{owner}/{repo}/pulls/{pull_number}/requested_reviewers",
      {
        owner,
        repo: name,
        pull_number: pullRequestNumber,
        reviewers: [...reviewers],
      },
    );
  }

  /**
   * Read the combined commit status (legacy Statuses API aggregate) for
   * `sha`. The CI phase polls this until the state is non-pending.
   *
   * @param repo Target repository.
   * @param sha Commit SHA.
   * @returns Aggregate status across every contributing check.
   */
  async getCombinedStatus(repo: RepoFullName, sha: string): Promise<CombinedStatus> {
    const { owner, name } = splitRepo(repo);
    const response = await this.requestWithRetry(
      "GET /repos/{owner}/{repo}/commits/{ref}/status",
      { owner, repo: name, ref: sha },
    );
    const data = response.data as CombinedStatusResponse;
    return { state: data.state, sha: data.sha };
  }

  /**
   * List individual check runs (Checks API) attached to `sha`. The
   * stabilize loop's CI phase reads this in addition to the combined
   * status to surface per-check failure context.
   *
   * @param repo Target repository.
   * @param sha Commit SHA.
   * @returns The check runs in GitHub's natural order.
   */
  async listCheckRuns(
    repo: RepoFullName,
    sha: string,
  ): Promise<readonly CheckRunSummary[]> {
    const { owner, name } = splitRepo(repo);
    const response = await this.requestWithRetry(
      "GET /repos/{owner}/{repo}/commits/{ref}/check-runs",
      { owner, repo: name, ref: sha },
    );
    const data = response.data as CheckRunListResponse;
    return data.check_runs.map((run) => ({
      id: run.id,
      name: run.name,
      status: run.status,
      conclusion: run.conclusion,
      htmlUrl: run.html_url,
    }));
  }

  /**
   * Fetch the raw logs ZIP for a single check run. Returned as a
   * `Uint8Array` so consumers can persist or extract without coupling
   * to a streaming API.
   *
   * GitHub responds with `application/zip`; the agent runner's
   * "explain CI failure" prompt unpacks the entry it cares about.
   *
   * @param repo Target repository.
   * @param checkRunId Check-run id from {@link listCheckRuns}.
   * @returns The ZIP bytes.
   */
  async getCheckRunLogs(repo: RepoFullName, checkRunId: number): Promise<Uint8Array> {
    const { owner, name } = splitRepo(repo);
    const response = await this.requestWithRetry(
      "GET /repos/{owner}/{repo}/check-runs/{check_run_id}/logs",
      {
        owner,
        repo: name,
        check_run_id: checkRunId,
        request: { parseSuccessResponseBody: false },
      },
    );
    return await coerceToBytes(response.data);
  }

  /**
   * List submitted reviews on a pull request, in submission order.
   *
   * @param repo Target repository.
   * @param pullRequestNumber Pull-request number.
   * @returns Reviews in their submission order.
   */
  async listReviews(
    repo: RepoFullName,
    pullRequestNumber: IssueNumber,
  ): Promise<readonly PullRequestReview[]> {
    const { owner, name } = splitRepo(repo);
    const response = await this.requestWithRetry(
      "GET /repos/{owner}/{repo}/pulls/{pull_number}/reviews",
      { owner, repo: name, pull_number: pullRequestNumber },
    );
    const data = response.data as ReviewResponse[];
    return data.map((review) => projectReview(review));
  }

  /**
   * List inline review comments on a pull request, in creation order.
   *
   * @param repo Target repository.
   * @param pullRequestNumber Pull-request number.
   * @returns Inline review comments in their creation order.
   */
  async listReviewComments(
    repo: RepoFullName,
    pullRequestNumber: IssueNumber,
  ): Promise<readonly PullRequestReviewComment[]> {
    const { owner, name } = splitRepo(repo);
    const response = await this.requestWithRetry(
      "GET /repos/{owner}/{repo}/pulls/{pull_number}/comments",
      { owner, repo: name, pull_number: pullRequestNumber },
    );
    const data = response.data as ReviewCommentResponse[];
    return data.map((comment) => projectReviewComment(comment));
  }

  /**
   * Resolve a single review thread via GitHub's GraphQL
   * `resolveReviewThread` mutation.
   *
   * Routes the mutation through `@octokit/core`'s built-in `graphql()`
   * helper (see ADR-019) so we do not pull in `@octokit/graphql` as a
   * separate dependency. The token is minted on every call from the
   * same {@link GitHubAuth} strategy the REST methods use, so token
   * rotation is automatic.
   *
   * @param threadId GraphQL node id of the review thread to resolve.
   * @throws The same surface `@octokit/core` raises for GraphQL
   *   errors — typically `GraphqlResponseError` with a `status` field.
   */
  async resolveReviewThread(threadId: string): Promise<void> {
    const token = await this.auth.getInstallationToken(this.installationId);
    await this.octokit.graphql<{ resolveReviewThread: { thread: { id: string } } }>(
      RESOLVE_REVIEW_THREAD_MUTATION,
      {
        threadId,
        headers: { authorization: `token ${token}` },
      },
    );
  }

  /**
   * Merge the pull request per `mode`.
   *
   * - `"squash"` performs a squash merge.
   * - `"rebase"` performs a rebase merge.
   * - `"manual"` is a no-op at the API level — the supervisor stays in
   *   `READY_TO_MERGE` and waits for the operator to merge from the
   *   GitHub UI; we still validate the input so a stale call site
   *   cannot pass `"manual"` and get a silent merge.
   *
   * @param repo Target repository.
   * @param pullRequestNumber Pull-request number.
   * @param mode Merge strategy.
   */
  async mergePullRequest(
    repo: RepoFullName,
    pullRequestNumber: IssueNumber,
    mode: MergeMode,
  ): Promise<void> {
    if (mode === "manual") {
      return;
    }
    const { owner, name } = splitRepo(repo);
    await this.requestWithRetry(
      "PUT /repos/{owner}/{repo}/pulls/{pull_number}/merge",
      {
        owner,
        repo: name,
        pull_number: pullRequestNumber,
        merge_method: mode,
      },
    );
  }

  // -------------------------------------------------------------------------
  // Internals
  // -------------------------------------------------------------------------

  /**
   * Issue an Octokit request and retry exactly once when GitHub gave us a
   * clear rate-limit signal (see ADR-011 for the full policy):
   *
   * - `Retry-After` header present (any status) — the secondary
   *   rate-limit signal.
   * - `X-RateLimit-Remaining: 0` paired with `X-RateLimit-Reset` (any
   *   status) — the primary rate-limit signal.
   * - `429` with neither header — the status itself is the signal.
   *
   * The retry decision lives entirely in
   * {@link GitHubClientImpl.computeRetrySleepMilliseconds}: it returns a
   * sleep duration when one of the above conditions holds and `null`
   * otherwise. A `403` with no rate-limit headers therefore propagates
   * immediately (permission/scope error, not a quota event), and 5xx
   * propagates so the supervisor's higher-level retry policy can decide
   * whether to back off.
   */
  private async requestWithRetry(
    route: string,
    parameters: Record<string, unknown>,
  ): Promise<{ data: unknown; status: number; headers: Record<string, string> }> {
    try {
      return await this.executeRequest(route, parameters);
    } catch (firstError) {
      if (!(firstError instanceof Error)) throw firstError;
      const status = readStatus(firstError);
      if (status === undefined) {
        throw firstError;
      }
      const headers = readHeaders(firstError);
      const waitMilliseconds = this.computeRetrySleepMilliseconds(status, headers);
      if (waitMilliseconds === null) {
        throw firstError;
      }
      await this.sleepMilliseconds(waitMilliseconds);
      return await this.executeRequest(route, parameters);
    }
  }

  /**
   * Single Octokit invocation: mints a token, attaches it to the
   * request, executes, and normalizes the response.
   */
  private async executeRequest(
    route: string,
    parameters: Record<string, unknown>,
  ): Promise<{ data: unknown; status: number; headers: Record<string, string> }> {
    const token = await this.auth.getInstallationToken(this.installationId);
    const response = await this.octokit.request(route, {
      ...parameters,
      headers: {
        authorization: `token ${token}`,
      },
    });
    return {
      data: response.data,
      status: response.status,
      headers: normalizeHeaderRecord(response.headers),
    };
  }

  /**
   * Translate the rate-limit response headers into a sleep duration in
   * milliseconds, or `null` when no retry should fire. Honors the
   * header-driven signals regardless of HTTP status (see ADR-011):
   *
   * - `Retry-After` (seconds, GitHub secondary rate limit).
   * - `X-RateLimit-Remaining: 0` paired with `X-RateLimit-Reset`
   *   (Unix-seconds epoch, GitHub primary rate limit).
   *
   * If neither header applies, the status itself decides:
   *
   * - `429` falls back to {@link DEFAULT_FALLBACK_RETRY_SLEEP_MILLISECONDS}
   *   (the status alone is a rate-limit signal).
   * - Anything else returns `null` so the caller propagates without
   *   retrying — a `403` with no rate-limit headers is a permission
   *   error, not a quota event.
   */
  private computeRetrySleepMilliseconds(
    status: number,
    headers: Record<string, string>,
  ): number | null {
    const retryAfter = headers[HEADER_RETRY_AFTER];
    if (retryAfter !== undefined) {
      const seconds = Number.parseFloat(retryAfter);
      if (!Number.isNaN(seconds) && seconds >= 0) {
        return clamp(seconds * 1_000, 0, this.maxRetrySleepMilliseconds);
      }
    }
    const remainingRaw = headers[HEADER_RATE_LIMIT_REMAINING];
    const resetRaw = headers[HEADER_RATE_LIMIT_RESET];
    if (resetRaw !== undefined && remainingRaw !== undefined) {
      const remaining = Number.parseInt(remainingRaw, 10);
      if (!Number.isNaN(remaining) && remaining <= 0) {
        const resetSeconds = Number.parseInt(resetRaw, 10);
        if (!Number.isNaN(resetSeconds)) {
          const deltaMilliseconds = resetSeconds * 1_000 - this.nowMilliseconds();
          return clamp(deltaMilliseconds, 0, this.maxRetrySleepMilliseconds);
        }
      }
    }
    if (status === 429) {
      // 429 with no header guidance: status alone is the rate-limit
      // signal, so fall back to a small constant. Cap by the configured
      // max so a future caller cannot exceed the retry ceiling.
      return clamp(
        DEFAULT_FALLBACK_RETRY_SLEEP_MILLISECONDS,
        0,
        this.maxRetrySleepMilliseconds,
      );
    }
    return null;
  }
}

// ---------------------------------------------------------------------------
// Local helpers
// ---------------------------------------------------------------------------

interface IssueResponse {
  readonly number: number;
  readonly title: string;
  readonly body: string | null;
  readonly state: "open" | "closed";
}

interface PullRequestResponse {
  readonly number: number;
  readonly head: { readonly sha: string; readonly ref: string };
  readonly base: { readonly ref: string };
  readonly state: "open" | "closed";
  readonly merged?: boolean;
  readonly merged_at?: string | null;
}

interface CombinedStatusResponse {
  readonly state: "pending" | "success" | "failure" | "error";
  readonly sha: string;
}

interface CheckRunListResponse {
  readonly check_runs: readonly CheckRunResponse[];
}

interface CheckRunResponse {
  readonly id: number;
  readonly name: string;
  readonly status: "queued" | "in_progress" | "completed";
  readonly conclusion: CheckRunSummary["conclusion"];
  readonly html_url: string;
}

interface ReviewResponse {
  readonly id: number;
  readonly user: { readonly login: string } | null;
  readonly state: PullRequestReview["state"];
  readonly body: string | null;
  readonly submitted_at?: string | null;
}

interface ReviewCommentResponse {
  readonly id: number;
  readonly pull_request_review_id: number | null;
  readonly user: { readonly login: string } | null;
  readonly body: string;
  readonly path: string;
  readonly line: number | null;
  readonly in_reply_to_id?: number | null;
  readonly created_at: string;
}

function splitRepo(repo: RepoFullName): { owner: string; name: string } {
  const slashIndex = repo.indexOf("/");
  // `RepoFullName` is brand-validated; the slash is always present.
  const owner = repo.slice(0, slashIndex);
  const name = repo.slice(slashIndex + 1);
  return { owner, name };
}

function projectPullRequest(data: PullRequestResponse): PullRequestDetails {
  const state: PullRequestDetails["state"] = data.merged === true
    ? "merged"
    : (data.merged_at !== undefined && data.merged_at !== null)
    ? "merged"
    : data.state;
  return {
    number: makeIssueNumber(data.number),
    headSha: data.head.sha,
    headRef: data.head.ref,
    baseRef: data.base.ref,
    state,
  };
}

function projectReview(data: ReviewResponse): PullRequestReview {
  const base: {
    id: number;
    user: string;
    state: PullRequestReview["state"];
    body: string;
  } = {
    id: data.id,
    user: data.user?.login ?? "",
    state: data.state,
    body: data.body ?? "",
  };
  if (data.submitted_at !== undefined && data.submitted_at !== null) {
    return { ...base, submittedAtIso: data.submitted_at };
  }
  return base;
}

function projectReviewComment(data: ReviewCommentResponse): PullRequestReviewComment {
  return {
    id: data.id,
    pullRequestReviewId: data.pull_request_review_id,
    user: data.user?.login ?? "",
    body: data.body,
    path: data.path,
    line: data.line,
    inReplyToId: data.in_reply_to_id ?? null,
    createdAtIso: data.created_at,
  };
}

/**
 * Best-effort extraction of an HTTP status from an error thrown by
 * `@octokit/core`. Octokit raises `RequestError` from
 * `@octokit/request-error` with a `status` field; some test doubles
 * might throw plain `Error` objects with a `status` property attached.
 */
function readStatus(error: Error): number | undefined {
  const candidate = error as Error & { status?: unknown };
  if (typeof candidate.status === "number") {
    return candidate.status;
  }
  return undefined;
}

function readHeaders(error: Error): Record<string, string> {
  const candidate = error as Error & {
    response?: { headers?: unknown };
    headers?: unknown;
  };
  if (candidate.response !== undefined) {
    const responseHeaders = candidate.response.headers;
    if (responseHeaders !== undefined) {
      return normalizeHeaderRecord(responseHeaders);
    }
  }
  if (candidate.headers !== undefined) {
    return normalizeHeaderRecord(candidate.headers);
  }
  return {};
}

function normalizeHeaderRecord(headers: unknown): Record<string, string> {
  if (headers === null || headers === undefined) {
    return {};
  }
  if (typeof Headers !== "undefined" && headers instanceof Headers) {
    const out: Record<string, string> = {};
    headers.forEach((value, key) => {
      out[key.toLowerCase()] = value;
    });
    return out;
  }
  if (typeof headers !== "object") {
    return {};
  }
  const source = headers as Record<string, unknown>;
  const out: Record<string, string> = {};
  for (const [key, value] of Object.entries(source)) {
    if (typeof value === "string") {
      out[key.toLowerCase()] = value;
    } else if (typeof value === "number") {
      out[key.toLowerCase()] = String(value);
    }
  }
  return out;
}

function clamp(value: number, lower: number, upper: number): number {
  if (Number.isNaN(value)) return lower;
  if (value < lower) return lower;
  if (value > upper) return upper;
  return value;
}

async function coerceToBytes(body: unknown): Promise<Uint8Array> {
  if (body instanceof Uint8Array) {
    return body;
  }
  if (body instanceof ArrayBuffer) {
    return new Uint8Array(body);
  }
  if (typeof Response !== "undefined" && body instanceof Response) {
    const buffer = await body.arrayBuffer();
    return new Uint8Array(buffer);
  }
  if (typeof Blob !== "undefined" && body instanceof Blob) {
    const buffer = await body.arrayBuffer();
    return new Uint8Array(buffer);
  }
  if (typeof body === "string") {
    return new TextEncoder().encode(body);
  }
  // With `parseSuccessResponseBody: false`, Octokit returns the raw
  // `Response` body which on Deno may surface as a ReadableStream.
  // `new Response(stream).arrayBuffer()` collects it without consuming
  // the global runtime stream API directly.
  if (body !== null && typeof body === "object") {
    const buffer = await new Response(body as BodyInit).arrayBuffer();
    return new Uint8Array(buffer);
  }
  throw new TypeError(
    `Unexpected response body type for binary endpoint: ${typeof body}`,
  );
}

function defaultSleep(milliseconds: number): Promise<void> {
  return new Promise((resolve) => {
    if (milliseconds <= 0) {
      resolve();
      return;
    }
    const handle = setTimeout(resolve, milliseconds);
    // `setTimeout` in Deno returns a numeric handle; `unref` is not on
    // every runtime. We do not block daemon shutdown on a pending retry
    // sleep — the supervisor cancels in-flight requests on shutdown.
    const maybeUnref = handle as unknown as { unref?: () => void };
    if (typeof maybeUnref.unref === "function") {
      maybeUnref.unref();
    }
  });
}
