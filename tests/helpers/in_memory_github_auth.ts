/**
 * tests/helpers/in_memory_github_auth.ts — deterministic, fake-clock
 * implementation of {@link GitHubAuth}.
 *
 * Wave 2 ships the real `src/github/app-auth.ts` over `@octokit/auth-app`
 * (JWT, installation-token exchange, expiry-aware cache). Tests for any
 * consumer that depends only on the `GitHubAuth` interface use this
 * double: tokens are deterministic strings derived from
 * `(installationId, sequence)`, the "current time" advances only when the
 * test calls {@link InMemoryGitHubAuth.advanceClockMilliseconds}, and the
 * cache invalidates when virtual time crosses each token's `expiresAtMs`.
 *
 * Doubles compile against `src/types.ts` so a contract drift breaks the
 * test build immediately.
 */

import { type GitHubAuth, type InstallationId } from "../../src/types.ts";
import { INSTALLATION_TOKEN_REFRESH_LEAD_MILLISECONDS } from "../../src/constants.ts";

/**
 * Deterministic token TTL used by the double, in milliseconds.
 *
 * Mirrors GitHub's actual installation-token TTL (one hour) so any
 * consumer that respects {@link INSTALLATION_TOKEN_REFRESH_LEAD_MILLISECONDS}
 * sees realistic refresh cadence.
 */
const FAKE_TOKEN_TTL_MILLISECONDS = 60 * 60 * 1_000;

interface CachedToken {
  readonly token: string;
  readonly expiresAtMilliseconds: number;
}

/**
 * In-memory {@link GitHubAuth} double with a synthetic clock.
 *
 * @example
 * ```ts
 * const auth = new InMemoryGitHubAuth();
 * const installation = makeInstallationId(123);
 *
 * const first = await auth.getInstallationToken(installation);
 * const second = await auth.getInstallationToken(installation);
 * assertEquals(first, second); // cache hit
 *
 * auth.advanceClockMilliseconds(60 * 60 * 1_000);
 * const refreshed = await auth.getInstallationToken(installation);
 * assertNotEquals(first, refreshed); // expired and refreshed
 * ```
 */
export class InMemoryGitHubAuth implements GitHubAuth {
  /** Total minted-token count, used to keep token strings deterministic. */
  private mintCount = 0;
  /** Per-installation cache. */
  private readonly cache = new Map<InstallationId, CachedToken>();
  /** Current virtual time, in milliseconds since the Unix epoch. */
  private currentTimeMilliseconds: number;
  /** Per-installation invocation log; assertions read this. */
  private readonly tokenRequests: InstallationId[] = [];

  /**
   * @param initialTimeMilliseconds Starting virtual time. Defaults to a
   *   fixed instant so tests are reproducible regardless of wall-clock.
   */
  constructor(initialTimeMilliseconds: number = 1_700_000_000_000) {
    this.currentTimeMilliseconds = initialTimeMilliseconds;
  }

  /**
   * Implementation of {@link GitHubAuth.getInstallationToken}.
   *
   * Returns the cached token if it is still within its TTL minus
   * {@link INSTALLATION_TOKEN_REFRESH_LEAD_MILLISECONDS}; otherwise mints
   * a fresh deterministic token and caches it.
   *
   * @param installationId Installation to mint a token for.
   * @returns A fake token string of the form
   *   `inmemory-token-<installationId>-<mintSequence>`.
   */
  getInstallationToken(installationId: InstallationId): Promise<string> {
    this.tokenRequests.push(installationId);
    const cached = this.cache.get(installationId);
    const refreshThresholdMilliseconds = this.currentTimeMilliseconds +
      INSTALLATION_TOKEN_REFRESH_LEAD_MILLISECONDS;
    if (cached !== undefined && cached.expiresAtMilliseconds > refreshThresholdMilliseconds) {
      return Promise.resolve(cached.token);
    }
    this.mintCount += 1;
    const token = `inmemory-token-${installationId}-${this.mintCount}`;
    const expiresAtMilliseconds = this.currentTimeMilliseconds + FAKE_TOKEN_TTL_MILLISECONDS;
    this.cache.set(installationId, { token, expiresAtMilliseconds });
    return Promise.resolve(token);
  }

  /**
   * Move the virtual clock forward by `deltaMilliseconds`.
   *
   * @param deltaMilliseconds Milliseconds to advance. Negative values
   *   throw because the cache invalidation logic assumes monotonic time.
   * @throws RangeError when `deltaMilliseconds` is negative.
   */
  advanceClockMilliseconds(deltaMilliseconds: number): void {
    if (deltaMilliseconds < 0) {
      throw new RangeError(
        `advanceClockMilliseconds requires a non-negative delta; got ${deltaMilliseconds}`,
      );
    }
    this.currentTimeMilliseconds += deltaMilliseconds;
  }

  /**
   * Read the current virtual time.
   *
   * @returns The clock value in milliseconds since the Unix epoch.
   */
  nowMilliseconds(): number {
    return this.currentTimeMilliseconds;
  }

  /**
   * Read the installation ids the SUT has requested tokens for, in order.
   *
   * @returns A defensive copy of the request log.
   */
  recordedRequests(): readonly InstallationId[] {
    return [...this.tokenRequests];
  }

  /**
   * Forget every cached token. Useful when a test needs a clean slate
   * mid-run (e.g. simulating a daemon restart).
   */
  resetCache(): void {
    this.cache.clear();
  }
}
