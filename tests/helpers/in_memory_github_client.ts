/**
 * tests/helpers/in_memory_github_client.ts — scriptable
 * {@link StabilizeGitHubClient} double.
 *
 * Wave 2 ships the real client over `@octokit/core`. Consumer waves test
 * against this double: per-method timelines let a test say "the first
 * `getCombinedStatus` returns `pending`, the second returns `success`" so
 * stabilize-loop tests can simulate CI flipping without sleeping or
 * polling. Every call is recorded so assertions can verify call shape and
 * order.
 *
 * Compiles against `src/types.ts` (W1 contract) and
 * `src/github/client.ts` (the additive {@link StabilizeGitHubClient}
 * interface) so a contract drift is caught at the `deno check` step.
 */

import {
  type CombinedStatus,
  type GitHubClient,
  type IssueDetails,
  type IssueNumber,
  type MergeMode,
  type PullRequestDetails,
  type RepoFullName,
} from "../../src/types.ts";
import {
  type CheckRunSummary,
  type PullRequestReview,
  type PullRequestReviewComment,
  type PullRequestReviewThread,
  type StabilizeGitHubClient,
} from "../../src/github/client.ts";

// `GitHubClient` is imported so the `{@inheritdoc GitHubClient.*}` JSDoc
// tags below resolve under `deno doc --lint`. The type itself is not
// referenced at runtime (the implemented contract is the wider
// `StabilizeGitHubClient`), so we tag it as intentionally used.
type _UseGitHubClient = GitHubClient;

/** A scripted reply or thrown error. */
export type ScriptedReply<TValue> =
  | { readonly kind: "value"; readonly value: TValue }
  | { readonly kind: "error"; readonly error: Error };

/**
 * One recorded invocation, useful for assertions.
 */
export type RecordedCall =
  | {
    readonly method: "getIssue";
    readonly repo: RepoFullName;
    readonly issueNumber: IssueNumber;
  }
  | {
    readonly method: "createPullRequest";
    readonly repo: RepoFullName;
    readonly args: {
      readonly headRef: string;
      readonly baseRef: string;
      readonly title: string;
      readonly body: string;
    };
  }
  | {
    readonly method: "requestReviewers";
    readonly repo: RepoFullName;
    readonly pullRequestNumber: IssueNumber;
    readonly reviewers: readonly string[];
  }
  | {
    readonly method: "getCombinedStatus";
    readonly repo: RepoFullName;
    readonly sha: string;
  }
  | {
    readonly method: "listCheckRuns";
    readonly repo: RepoFullName;
    readonly sha: string;
  }
  | {
    readonly method: "getCheckRunLogs";
    readonly repo: RepoFullName;
    readonly checkRunId: number;
  }
  | {
    readonly method: "listReviews";
    readonly repo: RepoFullName;
    readonly pullRequestNumber: IssueNumber;
  }
  | {
    readonly method: "listReviewComments";
    readonly repo: RepoFullName;
    readonly pullRequestNumber: IssueNumber;
  }
  | {
    readonly method: "listReviewThreads";
    readonly repo: RepoFullName;
    readonly pullRequestNumber: IssueNumber;
  }
  | {
    readonly method: "resolveReviewThread";
    readonly threadId: string;
  }
  | {
    readonly method: "mergePullRequest";
    readonly repo: RepoFullName;
    readonly pullRequestNumber: IssueNumber;
    readonly mode: MergeMode;
  };

interface MethodTimelines {
  getIssue: ScriptedReply<IssueDetails>[];
  createPullRequest: ScriptedReply<PullRequestDetails>[];
  requestReviewers: ScriptedReply<void>[];
  getCombinedStatus: ScriptedReply<CombinedStatus>[];
  listCheckRuns: ScriptedReply<readonly CheckRunSummary[]>[];
  getCheckRunLogs: ScriptedReply<Uint8Array>[];
  listReviews: ScriptedReply<readonly PullRequestReview[]>[];
  listReviewComments: ScriptedReply<readonly PullRequestReviewComment[]>[];
  listReviewThreads: ScriptedReply<readonly PullRequestReviewThread[]>[];
  resolveReviewThread: ScriptedReply<void>[];
  mergePullRequest: ScriptedReply<void>[];
}

/**
 * In-memory {@link GitHubClient}. Each method consumes one entry from its
 * scripted timeline; if the timeline is empty the method throws so the
 * test fails loudly rather than receiving an undefined fallback.
 *
 * @example
 * ```ts
 * const client = new InMemoryGitHubClient();
 * client.queueGetCombinedStatus({ kind: "value", value: { state: "pending", sha: "abc" } });
 * client.queueGetCombinedStatus({ kind: "value", value: { state: "success", sha: "abc" } });
 *
 * const first = await client.getCombinedStatus(repo, "abc");
 * const second = await client.getCombinedStatus(repo, "abc");
 * assertEquals(first.state, "pending");
 * assertEquals(second.state, "success");
 * ```
 */
export class InMemoryGitHubClient implements StabilizeGitHubClient {
  private readonly timelines: MethodTimelines = {
    getIssue: [],
    createPullRequest: [],
    requestReviewers: [],
    getCombinedStatus: [],
    listCheckRuns: [],
    getCheckRunLogs: [],
    listReviews: [],
    listReviewComments: [],
    listReviewThreads: [],
    resolveReviewThread: [],
    mergePullRequest: [],
  };
  private readonly callLog: RecordedCall[] = [];

  /**
   * Queue the next reply for {@link InMemoryGitHubClient.getIssue}.
   * @param reply The next scripted reply.
   */
  queueGetIssue(reply: ScriptedReply<IssueDetails>): void {
    this.timelines.getIssue.push(reply);
  }

  /**
   * Queue the next reply for
   * {@link InMemoryGitHubClient.createPullRequest}.
   * @param reply The next scripted reply.
   */
  queueCreatePullRequest(reply: ScriptedReply<PullRequestDetails>): void {
    this.timelines.createPullRequest.push(reply);
  }

  /**
   * Queue the next reply for
   * {@link InMemoryGitHubClient.requestReviewers}.
   * @param reply The next scripted reply.
   */
  queueRequestReviewers(reply: ScriptedReply<void>): void {
    this.timelines.requestReviewers.push(reply);
  }

  /**
   * Queue the next reply for
   * {@link InMemoryGitHubClient.getCombinedStatus}.
   * @param reply The next scripted reply.
   */
  queueGetCombinedStatus(reply: ScriptedReply<CombinedStatus>): void {
    this.timelines.getCombinedStatus.push(reply);
  }

  /**
   * Queue the next reply for
   * {@link InMemoryGitHubClient.listCheckRuns}.
   * @param reply The next scripted reply.
   */
  queueListCheckRuns(reply: ScriptedReply<readonly CheckRunSummary[]>): void {
    this.timelines.listCheckRuns.push(reply);
  }

  /**
   * Queue the next reply for
   * {@link InMemoryGitHubClient.getCheckRunLogs}.
   * @param reply The next scripted reply.
   */
  queueGetCheckRunLogs(reply: ScriptedReply<Uint8Array>): void {
    this.timelines.getCheckRunLogs.push(reply);
  }

  /**
   * Queue the next reply for
   * {@link InMemoryGitHubClient.listReviews}.
   * @param reply The next scripted reply.
   */
  queueListReviews(reply: ScriptedReply<readonly PullRequestReview[]>): void {
    this.timelines.listReviews.push(reply);
  }

  /**
   * Queue the next reply for
   * {@link InMemoryGitHubClient.listReviewComments}.
   * @param reply The next scripted reply.
   */
  queueListReviewComments(
    reply: ScriptedReply<readonly PullRequestReviewComment[]>,
  ): void {
    this.timelines.listReviewComments.push(reply);
  }

  /**
   * Queue the next reply for
   * {@link InMemoryGitHubClient.listReviewThreads}.
   *
   * Tests that exercise the conversations phase against this double
   * usually queue a `listReviewThreads` reply alongside their
   * `listReviewComments` reply: the supervisor calls both per tick and
   * joins them on comment id to populate
   * {@link PullRequestReviewComment.threadNodeId}. When a test omits
   * this queue entirely the double's `listReviewThreads` returns the
   * empty array (see the implementation below), preserving the
   * pre-existing behavior where `threadNodeId` was set directly on the
   * scripted comment payload.
   *
   * @param reply The next scripted reply.
   */
  queueListReviewThreads(
    reply: ScriptedReply<readonly PullRequestReviewThread[]>,
  ): void {
    this.timelines.listReviewThreads.push(reply);
  }

  /**
   * Queue the next reply for
   * {@link InMemoryGitHubClient.resolveReviewThread}.
   * @param reply The next scripted reply.
   */
  queueResolveReviewThread(reply: ScriptedReply<void>): void {
    this.timelines.resolveReviewThread.push(reply);
  }

  /**
   * Queue the next reply for
   * {@link InMemoryGitHubClient.mergePullRequest}.
   * @param reply The next scripted reply.
   */
  queueMergePullRequest(reply: ScriptedReply<void>): void {
    this.timelines.mergePullRequest.push(reply);
  }

  /**
   * Read the recorded call log.
   *
   * @returns A defensive copy of every call observed so far, in order.
   */
  recordedCalls(): readonly RecordedCall[] {
    return [...this.callLog];
  }

  /** {@inheritdoc GitHubClient.getIssue} */
  getIssue(repo: RepoFullName, issueNumber: IssueNumber): Promise<IssueDetails> {
    this.callLog.push({ method: "getIssue", repo, issueNumber });
    return resolveScripted(this.timelines.getIssue, "getIssue");
  }

  /** {@inheritdoc GitHubClient.createPullRequest} */
  createPullRequest(
    repo: RepoFullName,
    args: { headRef: string; baseRef: string; title: string; body: string },
  ): Promise<PullRequestDetails> {
    this.callLog.push({ method: "createPullRequest", repo, args });
    return resolveScripted(this.timelines.createPullRequest, "createPullRequest");
  }

  /** {@inheritdoc GitHubClient.requestReviewers} */
  requestReviewers(
    repo: RepoFullName,
    pullRequestNumber: IssueNumber,
    reviewers: readonly string[],
  ): Promise<void> {
    this.callLog.push({
      method: "requestReviewers",
      repo,
      pullRequestNumber,
      reviewers,
    });
    return resolveScripted(this.timelines.requestReviewers, "requestReviewers");
  }

  /** {@inheritdoc GitHubClient.getCombinedStatus} */
  getCombinedStatus(repo: RepoFullName, sha: string): Promise<CombinedStatus> {
    this.callLog.push({ method: "getCombinedStatus", repo, sha });
    return resolveScripted(this.timelines.getCombinedStatus, "getCombinedStatus");
  }

  /** {@inheritdoc StabilizeGitHubClient.listCheckRuns} */
  listCheckRuns(
    repo: RepoFullName,
    sha: string,
  ): Promise<readonly CheckRunSummary[]> {
    this.callLog.push({ method: "listCheckRuns", repo, sha });
    return resolveScripted(this.timelines.listCheckRuns, "listCheckRuns");
  }

  /** {@inheritdoc StabilizeGitHubClient.getCheckRunLogs} */
  getCheckRunLogs(repo: RepoFullName, checkRunId: number): Promise<Uint8Array> {
    this.callLog.push({ method: "getCheckRunLogs", repo, checkRunId });
    return resolveScripted(this.timelines.getCheckRunLogs, "getCheckRunLogs");
  }

  /** {@inheritdoc StabilizeGitHubClient.listReviews} */
  listReviews(
    repo: RepoFullName,
    pullRequestNumber: IssueNumber,
  ): Promise<readonly PullRequestReview[]> {
    this.callLog.push({ method: "listReviews", repo, pullRequestNumber });
    return resolveScripted(this.timelines.listReviews, "listReviews");
  }

  /** {@inheritdoc StabilizeGitHubClient.listReviewComments} */
  listReviewComments(
    repo: RepoFullName,
    pullRequestNumber: IssueNumber,
  ): Promise<readonly PullRequestReviewComment[]> {
    this.callLog.push({
      method: "listReviewComments",
      repo,
      pullRequestNumber,
    });
    return resolveScripted(
      this.timelines.listReviewComments,
      "listReviewComments",
    );
  }

  /**
   * {@inheritdoc StabilizeGitHubClient.listReviewThreads}
   *
   * Unlike the other timelines, `listReviewThreads` returns the empty
   * array (rather than throwing) when the queue is empty. This keeps
   * existing tests that pre-populate
   * {@link PullRequestReviewComment.threadNodeId} directly on their
   * scripted `listReviewComments` payload working without modification:
   * the supervisor merges thread mappings onto comments, and an empty
   * mapping leaves whatever the test set on the comment in place. Tests
   * that want to exercise the join itself should call
   * {@link InMemoryGitHubClient.queueListReviewThreads} explicitly.
   */
  listReviewThreads(
    repo: RepoFullName,
    pullRequestNumber: IssueNumber,
  ): Promise<readonly PullRequestReviewThread[]> {
    this.callLog.push({
      method: "listReviewThreads",
      repo,
      pullRequestNumber,
    });
    const next = this.timelines.listReviewThreads.shift();
    if (next === undefined) {
      return Promise.resolve([]);
    }
    if (next.kind === "error") {
      return Promise.reject(next.error);
    }
    return Promise.resolve(next.value);
  }

  /** {@inheritdoc StabilizeGitHubClient.resolveReviewThread} */
  resolveReviewThread(threadId: string): Promise<void> {
    this.callLog.push({ method: "resolveReviewThread", threadId });
    return resolveScripted(
      this.timelines.resolveReviewThread,
      "resolveReviewThread",
    );
  }

  /** {@inheritdoc GitHubClient.mergePullRequest} */
  mergePullRequest(
    repo: RepoFullName,
    pullRequestNumber: IssueNumber,
    mode: MergeMode,
  ): Promise<void> {
    this.callLog.push({
      method: "mergePullRequest",
      repo,
      pullRequestNumber,
      mode,
    });
    return resolveScripted(this.timelines.mergePullRequest, "mergePullRequest");
  }
}

/**
 * Pull the next scripted reply off the queue and return its value as a
 * resolved promise (or rejected, for `error`). Throws synchronously if
 * the queue is empty so test assertions see the unmet expectation rather
 * than a stalled promise.
 */
function resolveScripted<TValue>(
  queue: ScriptedReply<TValue>[],
  methodName: string,
): Promise<TValue> {
  const next = queue.shift();
  if (next === undefined) {
    throw new Error(
      `InMemoryGitHubClient.${methodName} called with no scripted reply queued`,
    );
  }
  if (next.kind === "error") {
    return Promise.reject(next.error);
  }
  return Promise.resolve(next.value);
}
