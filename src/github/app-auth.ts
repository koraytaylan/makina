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
 * exchange, or its retry/clock-skew handling. The cache is **ours**: every
 * call into `@octokit/auth-app` passes `refresh: true` so its built-in LRU
 * cache is bypassed and the lead time matches
 * {@link INSTALLATION_TOKEN_REFRESH_LEAD_MILLISECONDS} exactly. The cache
 * key is the branded {@link InstallationId} rather than an Octokit-internal
 * string, which keeps the surface area auditable. See
 * {@link "../../docs/adrs/010-bypass-octokit-auth-app-cache.md" | ADR-010}
 * for the trade-off.
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
 * The `refresh: true` flag is mandatory and bypasses the library's own
 * LRU cache (see {@link defaultCreateAppAuthStrategy} for why). Test stubs
 * may ignore the flag because they have no internal cache, but production
 * callers MUST forward it to `@octokit/auth-app`.
 *
 * Modeled as an interface so a test stub can implement it without pulling
 * in the full Octokit `AuthInterface` (which mostly covers OAuth flows we
 * do not exercise).
 */
export interface InstallationAuthHook {
  (options: {
    readonly type: "installation";
    readonly installationId: number;
    readonly refresh: true;
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
   * Optional installation id forwarded to `createAppAuth` as a seed.
   *
   * The authoritative {@link InstallationId} for every token request comes
   * from the per-call argument to {@link GitHubAuth.getInstallationToken},
   * not this option. The same `createGitHubAppAuth` instance is reused
   * across all installations the daemon manages, so this seed is not
   * required and is only kept for callers that wired a single-installation
   * auth in early experiments. Prefer omitting it.
   *
   * If you do supply this value, it is passed through to `createAppAuth`
   * but is **not** a fallback default for the
   * {@link GitHubAuth.getInstallationToken} parameter.
   */
  readonly installationId?: number;
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
   * Construct a new {@link GitHubAppAuthError}.
   *
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
      ...(opts.installationId === undefined ? {} : { installationId: opts.installationId }),
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
        // Force `@octokit/auth-app` to bypass its built-in LRU cache so the
        // refresh decision lives entirely in *our* lead-time logic. Without
        // this, the library could hand back a stale cached token whose
        // expiry is closer than INSTALLATION_TOKEN_REFRESH_LEAD_MILLISECONDS
        // away — defeating the whole point of our cache.
        refresh: true,
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
 *
 * `@octokit/auth-app` ships with its own LRU cache that holds installation
 * tokens for ~59 of the GitHub-issued 60 minutes. Forwarding the
 * `refresh: true` flag from the request bypasses that cache entirely so
 * cache lifetime is governed *only* by this module's
 * {@link INSTALLATION_TOKEN_REFRESH_LEAD_MILLISECONDS} lead-time logic.
 * Without this, near-expiry refreshes initiated by our cache could still
 * receive the library's stale token and the lead-time guarantee in the
 * module header would not hold.
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
  return (request) =>
    auth({
      type: "installation",
      installationId: request.installationId,
      refresh: request.refresh,
    });
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
