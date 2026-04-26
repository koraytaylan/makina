/**
 * github/app-auth.ts — GitHub App authentication backed by
 * `@octokit/auth-app`.
 *
 * Wave 1 froze the {@link GitHubAuth} contract; this module is the real
 * implementation. The supervisor and the high-level GitHub client both
 * obtain installation tokens through {@link GitHubAuth.getInstallationToken},
 * which here is wired to:
 *
 * 1. Sign a short-lived (10-minute) JWT with the App's private key.
 * 2. Exchange the JWT for an installation access token via
 *    `POST /app/installations/{id}/access_tokens`.
 * 3. Return the cached token for subsequent calls until
 *    `expiresAt - INSTALLATION_TOKEN_REFRESH_LEAD_MILLISECONDS`.
 *
 * Steps 1 and 2 are delegated to `@octokit/auth-app` per
 * {@link "../../docs/adrs/005-no-jose-using-octokit-auth-app.md" | ADR-005} —
 * we do not reimplement the JOSE/JWT primitives, the install-token
 * exchange, or its retry/clock-skew handling. The cache is **ours**: we
 * intentionally bypass `@octokit/auth-app`'s built-in cache so the lead
 * time matches {@link INSTALLATION_TOKEN_REFRESH_LEAD_MILLISECONDS} exactly
 * and so the cache key is the branded {@link InstallationId} rather than a
 * Octokit-internal string.
 *
 * **Errors.** Anything thrown by `@octokit/auth-app` (private-key parse
 * failure, `403 Bad credentials`, network outage) is rewrapped as a
 * {@link GitHubAppAuthError} that names the failing operation. The
 * original error is preserved on `cause` so call-site tracebacks are not
 * lost.
 *
 * @module
 */

import { createAppAuth } from "@octokit/auth-app";

import { INSTALLATION_TOKEN_REFRESH_LEAD_MILLISECONDS } from "../constants.ts";
import { type GitHubAuth, type InstallationId } from "../types.ts";

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/**
 * Strategy function compatible with `@octokit/auth-app`'s `createAppAuth`.
 *
 * Tests inject a stub strategy so no JWT signing or HTTPS round-trip
 * happens; production code uses the default that delegates to the real
 * `@octokit/auth-app`.
 */
export interface CreateAppAuthStrategy {
  (options: {
    readonly appId: number;
    readonly privateKey: string;
    readonly installationId?: number;
  }): InstallationAuthHook;
}

/**
 * The narrow surface of `@octokit/auth-app`'s returned `auth()` function
 * that this module actually uses: ask for an installation token, get back
 * `{ token, expiresAt }`.
 *
 * Modeled as an interface so a test stub can implement it without pulling
 * in the full Octokit `AuthInterface` (which mostly covers OAuth flows we
 * do not exercise).
 */
export interface InstallationAuthHook {
  (options: {
    readonly type: "installation";
    readonly installationId: number;
  }): Promise<InstallationAuthResult>;
}

/**
 * Subset of `@octokit/auth-app`'s `InstallationAccessTokenAuthentication`
 * we read. `expiresAt` is an ISO-8601 timestamp; the cache compares it
 * (parsed via `Date.parse`) against the injected clock.
 */
export interface InstallationAuthResult {
  /** The minted installation access token. */
  readonly token: string;
  /** ISO-8601 timestamp at which the token stops being valid. */
  readonly expiresAt: string;
}

/**
 * Options accepted by {@link createGitHubAppAuth}.
 */
export interface CreateGitHubAppAuthOptions {
  /** GitHub App id (the integer printed on the App settings page). */
  readonly appId: number;
  /**
   * PEM-encoded private key as a string. Reading the key from disk is the
   * caller's responsibility (the config loader handles `~/` expansion).
   */
  readonly privateKey: string;
  /**
   * Default installation id passed through to `createAppAuth`. Callers
   * that target a single installation can omit `installationId` from each
   * `getInstallationToken` call; the default is used.
   *
   * Note: every call to {@link GitHubAuth.getInstallationToken} still
   * receives an explicit {@link InstallationId} per the W1 contract; this
   * default lets us reuse the same auth strategy for any installation
   * instead of constructing a fresh one per token request.
   */
  readonly installationId: number;
  /**
   * Optional injection point for the auth strategy factory. Tests pass a
   * stub; production callers omit this and the real `@octokit/auth-app`
   * is used.
   *
   * @internal
   */
  readonly createAppAuthStrategy?: CreateAppAuthStrategy;
  /**
   * Optional injection point for the wall-clock used by the cache. Tests
   * pass a deterministic clock so expiry behavior is reproducible;
   * production callers omit this and `Date.now()` is used.
   *
   * @internal
   */
  readonly nowMilliseconds?: () => number;
}

/**
 * Error thrown when an underlying `@octokit/auth-app` call fails.
 *
 * The message starts with `GitHub App auth failed during <operation>:` so
 * logs are scannable. The original error (private-key parse error, HTTP
 * `RequestError`, etc.) is preserved on `cause`.
 *
 * @example
 * ```ts
 * try {
 *   await auth.getInstallationToken(installationId);
 * } catch (error) {
 *   if (error instanceof GitHubAppAuthError) {
 *     console.error(error.operation, error.cause);
 *   }
 * }
 * ```
 */
export class GitHubAppAuthError extends Error {
  /** The operation that failed (e.g. `"getInstallationToken"`). */
  readonly operation: string;

  /**
   * @param operation A short identifier for the failing operation.
   * @param cause The original error thrown by `@octokit/auth-app`.
   */
  constructor(operation: string, cause: unknown) {
    super(
      `GitHub App auth failed during ${operation}: ${describeError(cause)}`,
      { cause },
    );
    this.name = "GitHubAppAuthError";
    this.operation = operation;
  }
}

// ---------------------------------------------------------------------------
// Public factory
// ---------------------------------------------------------------------------

/**
 * Create a {@link GitHubAuth} backed by `@octokit/auth-app`.
 *
 * The returned object holds an in-memory cache keyed by
 * {@link InstallationId}. A cache entry is reused until it is within
 * {@link INSTALLATION_TOKEN_REFRESH_LEAD_MILLISECONDS} of its `expiresAt`,
 * at which point a fresh token is minted.
 *
 * Concurrent requests for the same installation share the in-flight
 * promise so the App is not hit twice for one expiry boundary.
 *
 * @param opts See {@link CreateGitHubAppAuthOptions}.
 * @returns A {@link GitHubAuth} implementation.
 *
 * @example
 * ```ts
 * const auth = createGitHubAppAuth({
 *   appId: 1234,
 *   privateKey: await Deno.readTextFile("private-key.pem"),
 *   installationId: 9876543,
 * });
 * const token = await auth.getInstallationToken(makeInstallationId(9876543));
 * ```
 */
export function createGitHubAppAuth(opts: CreateGitHubAppAuthOptions): GitHubAuth {
  const strategy = opts.createAppAuthStrategy ?? defaultCreateAppAuthStrategy;
  const nowMilliseconds = opts.nowMilliseconds ?? defaultNowMilliseconds;

  let authHook: InstallationAuthHook;
  try {
    authHook = strategy({
      appId: opts.appId,
      privateKey: opts.privateKey,
      installationId: opts.installationId,
    });
  } catch (error) {
    throw new GitHubAppAuthError("createAppAuth", error);
  }

  const cache = new Map<InstallationId, CachedToken>();
  const inFlight = new Map<InstallationId, Promise<string>>();

  async function refresh(installationId: InstallationId): Promise<string> {
    let result: InstallationAuthResult;
    try {
      result = await authHook({
        type: "installation",
        installationId,
      });
    } catch (error) {
      throw new GitHubAppAuthError("getInstallationToken", error);
    }
    const expiresAtMilliseconds = parseExpiresAt(result.expiresAt);
    cache.set(installationId, {
      token: result.token,
      expiresAtMilliseconds,
    });
    return result.token;
  }

  return {
    async getInstallationToken(installationId: InstallationId): Promise<string> {
      const cached = cache.get(installationId);
      const refreshThreshold = nowMilliseconds() +
        INSTALLATION_TOKEN_REFRESH_LEAD_MILLISECONDS;
      if (cached !== undefined && cached.expiresAtMilliseconds > refreshThreshold) {
        return cached.token;
      }

      const pending = inFlight.get(installationId);
      if (pending !== undefined) {
        return await pending;
      }

      const promise = refresh(installationId);
      inFlight.set(installationId, promise);
      try {
        return await promise;
      } finally {
        inFlight.delete(installationId);
      }
    },
  };
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/**
 * Cache entry written when a fresh token is minted.
 */
interface CachedToken {
  /** The cached installation access token. */
  readonly token: string;
  /** Wall-clock expiry, in milliseconds since the Unix epoch. */
  readonly expiresAtMilliseconds: number;
}

/**
 * Default strategy: real `@octokit/auth-app`. The Octokit `auth()`
 * function returns an `InstallationAccessTokenAuthentication`; we narrow
 * it to the {@link InstallationAuthResult} surface this module needs.
 */
function defaultCreateAppAuthStrategy(options: {
  readonly appId: number;
  readonly privateKey: string;
  readonly installationId?: number;
}): InstallationAuthHook {
  const auth = createAppAuth({
    appId: options.appId,
    privateKey: options.privateKey,
    ...(options.installationId === undefined ? {} : { installationId: options.installationId }),
  });
  return (request) => auth({ type: "installation", installationId: request.installationId });
}

/**
 * Default clock: `Date.now()` wall-clock in milliseconds since the Unix
 * epoch.
 */
function defaultNowMilliseconds(): number {
  return Date.now();
}

/**
 * Parse the `expiresAt` ISO-8601 string into a millisecond timestamp.
 * Throws a {@link GitHubAppAuthError} if the string is unparseable —
 * `@octokit/auth-app` should never produce one, but defensive parsing
 * keeps a malformed third-party response from poisoning the cache with
 * `NaN`.
 */
function parseExpiresAt(expiresAt: string): number {
  const milliseconds = Date.parse(expiresAt);
  if (Number.isNaN(milliseconds)) {
    throw new GitHubAppAuthError(
      "parseExpiresAt",
      new Error(`Could not parse expiresAt timestamp: ${JSON.stringify(expiresAt)}`),
    );
  }
  return milliseconds;
}

/**
 * Coerce an unknown thrown value to a printable message. `Error.message`
 * for true Errors, JSON for everything else.
 */
function describeError(error: unknown): string {
  if (error instanceof Error) {
    return error.message;
  }
  try {
    return JSON.stringify(error);
  } catch {
    return String(error);
  }
}
