/**
 * github/app-client.ts — App-level GitHub client used by the setup wizard.
 *
 * Companion to {@link "./client.ts".GitHubClientImpl}, which is
 * **installation-scoped** (every request resolves an installation token
 * before going on the wire). The setup wizard runs **before** any
 * installation has been chosen, so it cannot use the installation-scoped
 * client. Instead it needs:
 *
 *   1. **App-level** access to `GET /app/installations` — the endpoint
 *      that lists every installation reachable from the App. This call is
 *      authenticated with a short-lived App JWT (RS256, 10-minute TTL),
 *      not an installation token.
 *   2. **Per-installation** access to `GET /installation/repositories` —
 *      the endpoint that lists the repos a specific installation can see.
 *      This call is authenticated with an installation access token.
 *
 * Both flows are delegated to `@octokit/auth-app` per
 * {@link "../../docs/adrs/005-no-jose-using-octokit-auth-app.md" | ADR-005}
 * and
 * {@link "../../docs/adrs/024-app-level-github-client-for-setup-wizard.md" | ADR-024}:
 * the JWT is minted via `auth({ type: "app" })` and the installation
 * token via `auth({ type: "installation", installationId, refresh: true })`.
 * We do not reimplement either primitive — the audited path lives inside
 * `@octokit/auth-app`.
 *
 * **Errors.** Anything thrown by `@octokit/auth-app`, `@octokit/core`, or
 * the underlying fetch is rewrapped as a {@link GitHubAppClientError} that
 * names the failing operation. The original error is preserved on `cause`
 * so the wizard's "failed to list installations: <reason>" diagnostic
 * carries useful context without leaking a stack trace.
 *
 * **Test seam.** Like {@link "./client.ts".GitHubClientImpl}, the factory
 * accepts an `Octokit` constructor and a `createAppAuthStrategy` for
 * dependency injection. Unit tests pass scripted versions so no real JWT
 * signing or HTTPS round-trip happens; production callers omit both
 * options and the real Octokit + `@octokit/auth-app` are used.
 *
 * @module
 */
import { createAppAuth } from "@octokit/auth-app";
import { Octokit } from "@octokit/core";

import {
  APP_INSTALLATION_REPOSITORIES_MAX_PAGES,
  APP_INSTALLATION_REPOSITORIES_PAGE_SIZE,
  APP_INSTALLATIONS_MAX_PAGES,
  APP_INSTALLATIONS_PAGE_SIZE,
  GITHUB_CLIENT_USER_AGENT,
} from "../constants.ts";

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/**
 * One installation listed by the App's `/app/installations` endpoint.
 *
 * The wizard only reads `id` (to mint a per-installation token) and
 * `account.login` (informational); no other fields are needed at this
 * layer, but the projection mirrors the GitHub payload shape so a future
 * caller can extend it without rewriting the parser.
 */
export interface AppInstallation {
  /**
   * Numeric installation id used by
   * `POST /app/installations/{id}/access_tokens` to mint an installation
   * token. The wizard treats this as the canonical identifier of an
   * installation.
   */
  readonly id: number;
  /**
   * Login of the account (user or org) the App is installed on. Optional
   * because the GitHub payload omits it when the account has been
   * deleted out from under the installation; the wizard tolerates the
   * absent case by displaying the installation id alone.
   */
  readonly accountLogin?: string;
}

/**
 * One repository visible to a specific installation, projected from
 * `GET /installation/repositories`.
 *
 * The wizard renders these as `<owner>/<name>` strings and treats them
 * as the unit of repository discovery; we do not surface the numeric
 * repo id because it does not show up in any user-facing config field.
 */
export interface InstallationRepository {
  /** Owning user/org login. */
  readonly owner: string;
  /** Repository name (without the owner prefix). */
  readonly name: string;
}

/**
 * App-scoped GitHub client used by the setup wizard.
 *
 * Distinct from the installation-scoped {@link "./client.ts".GitHubClientImpl}
 * because the calls this surface owns are App-level (require a JWT) or
 * are installation-level under a token minted **before** the daemon
 * picks an installation to operate on. The supervisor never holds an
 * `AppClient` — once `setup` is done, every supervisor call is
 * installation-scoped through `GitHubClient`.
 *
 * @see {@link "../../docs/adrs/024-app-level-github-client-for-setup-wizard.md" | ADR-024}
 */
export interface AppClient {
  /**
   * List every installation reachable from the configured App.
   *
   * Uses an App-level JWT against `GET /app/installations`. The JWT is
   * minted on every call because the wizard runs at most once per
   * `makina setup` invocation; caching a 10-minute JWT for that duration
   * would buy nothing. Pagination follows the same shape as
   * {@link AppClient.listInstallationRepositories}: walk pages with the
   * documented `per_page` maximum until a short page is observed, bounded
   * by a defensive max-page cap that surfaces a `pagination-overflow`
   * error rather than silently truncating the list.
   *
   * @returns Every installation the App can see, in GitHub's natural order.
   * @throws {GitHubAppClientError} on auth, network, or JSON-projection failure.
   */
  listAppInstallations(): Promise<readonly AppInstallation[]>;
  /**
   * List the repositories visible to a specific installation.
   *
   * Mints a fresh installation token via `@octokit/auth-app` and calls
   * `GET /installation/repositories`. The token is not retained — the
   * wizard only needs the repository list once.
   *
   * @param installationId Numeric installation id from
   *   {@link AppClient.listAppInstallations}.
   * @returns Repositories the installation can see, in GitHub's natural
   *   order. Returns an empty array if the installation has been granted
   *   access to no repos.
   * @throws {GitHubAppClientError} on auth, network, or JSON-projection failure.
   */
  listInstallationRepositories(
    installationId: number,
  ): Promise<readonly InstallationRepository[]>;
}

/**
 * Strategy function compatible with `@octokit/auth-app`'s `createAppAuth`.
 *
 * Tests inject a stub strategy so no JWT signing or HTTPS round-trip
 * happens; production code uses the default that delegates to the real
 * `@octokit/auth-app`. Mirrors the seam used by
 * {@link "./app-auth.ts".CreateAppAuthStrategy} so the two modules remain
 * test-symmetric.
 */
export interface AppClientAuthStrategy {
  (options: {
    readonly appId: number;
    readonly privateKey: string;
    /**
     * Optional GitHub Enterprise / staging API base URL forwarded from
     * {@link CreateAppClientOptions.baseUrl}. The default strategy
     * passes this to `@octokit/auth-app` so installation-token minting
     * targets the same host as the rest of the client; an unset value
     * falls back to `@octokit/auth-app`'s default (`api.github.com`).
     */
    readonly baseUrl?: string;
    /**
     * Optional `fetch` implementation forwarded from
     * {@link CreateAppClientOptions.fetch}. The default strategy passes
     * this to `@octokit/auth-app` so installation-token minting goes
     * through the same test seam as the rest of the client. Unit tests
     * inject a scripted fake fetch here so the auth path never touches
     * the network.
     */
    readonly fetch?: typeof globalThis.fetch;
  }): AppClientAuthHook;
}

/**
 * Narrow subset of `@octokit/auth-app`'s returned `auth()` function this
 * module uses. Two modes:
 *
 *   - `{ type: "app" }` — return an App JWT (`AppAuthentication`).
 *   - `{ type: "installation", installationId, refresh: true }` — mint
 *     an installation access token, bypassing the `@octokit/auth-app`
 *     LRU cache so the wizard always sees fresh state.
 *
 * Modeled as a callable interface so tests can implement it without
 * pulling in the full Octokit `AuthInterface`.
 */
export interface AppClientAuthHook {
  (options: { readonly type: "app" }): Promise<AppAuthResult>;
  (options: {
    readonly type: "installation";
    readonly installationId: number;
    readonly refresh: true;
  }): Promise<InstallationAuthResult>;
}

/**
 * Subset of `@octokit/auth-app`'s `AppAuthentication` we read.
 */
export interface AppAuthResult {
  /** App JWT used as `Authorization: Bearer <token>`. */
  readonly token: string;
}

/**
 * Subset of `@octokit/auth-app`'s `InstallationAccessTokenAuthentication`
 * we read.
 */
export interface InstallationAuthResult {
  /** Installation access token used as `Authorization: token <token>`. */
  readonly token: string;
}

/**
 * Construction options for {@link createAppClient}.
 */
export interface CreateAppClientOptions {
  /** GitHub App id (the integer printed on the App settings page). */
  readonly appId: number;
  /**
   * PEM-encoded private key as a string. Reading the key from disk is
   * the caller's responsibility (the wizard's production wiring expands
   * `~/` and reads the file before constructing the client).
   */
  readonly privateKey: string;
  /**
   * Optional injection point for the auth strategy factory. Tests pass
   * a stub; production callers omit this and `@octokit/auth-app` is used.
   *
   * @internal
   */
  readonly createAppAuthStrategy?: AppClientAuthStrategy;
  /**
   * Optional `User-Agent`. GitHub rejects requests without one. Defaults
   * to {@link DEFAULT_USER_AGENT}.
   */
  readonly userAgent?: string;
  /**
   * Optional override of the GitHub API base URL. Useful for testing
   * against GitHub Enterprise stand-ins. Defaults to Octokit's default
   * (`https://api.github.com`).
   */
  readonly baseUrl?: string;
  /**
   * Inject the underlying `fetch`. Tests pass a scripted fake; production
   * leaves this undefined and `@octokit/core` uses the runtime `fetch`.
   *
   * @internal
   */
  readonly fetch?: typeof fetch;
}

/**
 * Error thrown when an underlying call fails inside an {@link AppClient}.
 *
 * The message starts with `GitHub App client failed during <operation>:`
 * so logs are scannable. The original error is preserved on `cause`.
 *
 * @example
 * ```ts
 * try {
 *   await appClient.listAppInstallations();
 * } catch (error) {
 *   if (error instanceof GitHubAppClientError) {
 *     console.error(error.operation, error.cause);
 *   }
 * }
 * ```
 */
export class GitHubAppClientError extends Error {
  /** The operation that failed (e.g. `"listAppInstallations"`). */
  readonly operation: string;

  /**
   * Construct a new {@link GitHubAppClientError}.
   *
   * @param operation A short identifier for the failing operation.
   * @param cause The original error thrown by the underlying call.
   */
  constructor(operation: string, cause: unknown) {
    super(
      `GitHub App client failed during ${operation}: ${describeError(cause)}`,
      { cause },
    );
    this.name = "GitHubAppClientError";
    this.operation = operation;
  }
}

/**
 * Re-export the centralised default `User-Agent` so existing callers that
 * import from this module continue to work. The single source of truth is
 * {@link GITHUB_CLIENT_USER_AGENT} in `src/constants.ts`; both client
 * surfaces (`./client.ts` and this module) point at the same string so
 * server-side request logs see one caller, not two.
 */
export { GITHUB_CLIENT_USER_AGENT as DEFAULT_USER_AGENT };

// ---------------------------------------------------------------------------
// Public factory
// ---------------------------------------------------------------------------

/**
 * Create an {@link AppClient} backed by `@octokit/auth-app` and
 * `@octokit/core`.
 *
 * The returned object is **stateless** beyond the bound auth function
 * and Octokit instance: no token cache lives here. Installation tokens
 * are minted afresh on every call because the wizard's discovery flow
 * runs at most once per `makina setup` invocation; reuse is not worth
 * the cache-invalidation surface area.
 *
 * @param opts See {@link CreateAppClientOptions}.
 * @returns An {@link AppClient} bound to the supplied App credentials.
 *
 * @example
 * ```ts
 * const appClient = createAppClient({
 *   appId: 1234,
 *   privateKey: await Deno.readTextFile("private-key.pem"),
 * });
 * const installations = await appClient.listAppInstallations();
 * for (const installation of installations) {
 *   const repos = await appClient.listInstallationRepositories(installation.id);
 *   for (const repo of repos) {
 *     console.log(`${repo.owner}/${repo.name} via installation ${installation.id}`);
 *   }
 * }
 * ```
 */
export function createAppClient(opts: CreateAppClientOptions): AppClient {
  const strategy = opts.createAppAuthStrategy ?? defaultCreateAppAuthStrategy;
  const userAgent = opts.userAgent ?? GITHUB_CLIENT_USER_AGENT;

  let authHook: AppClientAuthHook;
  try {
    // Forward `baseUrl`/`fetch` so the auth path goes through the same
    // host and the same test seam as the client itself. Without this,
    // installation-token minting (`POST /app/installations/.../access_tokens`)
    // would always hit github.com via the default fetch even when the
    // caller injected a custom baseUrl/fetch — invisible to scripted-fetch
    // unit tests and broken on GitHub Enterprise.
    //
    // The conditional spreads exist because the project's
    // `exactOptionalPropertyTypes: true` setting forbids passing
    // `undefined` to optional fields; either include the field (with a
    // defined value) or omit it entirely.
    authHook = strategy({
      appId: opts.appId,
      privateKey: opts.privateKey,
      ...(opts.baseUrl !== undefined ? { baseUrl: opts.baseUrl } : {}),
      ...(opts.fetch !== undefined ? { fetch: opts.fetch } : {}),
    });
  } catch (error) {
    throw new GitHubAppClientError("createAppAuth", error);
  }

  const octokitOptions: Record<string, unknown> = { userAgent };
  if (opts.baseUrl !== undefined) {
    octokitOptions.baseUrl = opts.baseUrl;
  }
  if (opts.fetch !== undefined) {
    octokitOptions.request = { fetch: opts.fetch };
  }
  const octokit = new Octokit(octokitOptions);

  return {
    async listAppInstallations(): Promise<readonly AppInstallation[]> {
      let appAuth: AppAuthResult;
      try {
        appAuth = await authHook({ type: "app" });
      } catch (error) {
        throw new GitHubAppClientError("mintAppJwt", error);
      }
      // Walk every page of `/app/installations`. The endpoint paginates at a
      // default of thirty entries; an App installed in many accounts (a
      // realistic shape for any organisation-wide rollout) silently loses
      // installations past the first page if we don't ask. The `per_page`
      // and `page` parameters are GitHub-documented. Page size and the
      // defensive max-page cap are centralised in `src/constants.ts` so
      // this module carries no bare numeric literals.
      const aggregated: AppInstallation[] = [];
      let sawShortPage = false;
      for (
        let page = 1;
        page <= APP_INSTALLATIONS_MAX_PAGES;
        page += 1
      ) {
        let response: { data: unknown };
        try {
          response = await octokit.request("GET /app/installations", {
            per_page: APP_INSTALLATIONS_PAGE_SIZE,
            page,
            headers: { authorization: `Bearer ${appAuth.token}` },
          });
        } catch (error) {
          throw new GitHubAppClientError("listAppInstallations", error);
        }
        const projected = projectInstallations(response.data);
        for (const installation of projected) {
          aggregated.push(installation);
        }
        if (projected.length < APP_INSTALLATIONS_PAGE_SIZE) {
          // Short page → final page. Saves an extra round-trip for the
          // common case of an App installed in fewer than 100 accounts.
          sawShortPage = true;
          break;
        }
      }
      if (!sawShortPage) {
        // The cap was hit without a short page, which means the list is
        // almost certainly truncated. Surface this loudly rather than
        // silently returning a partial installation set: the wizard's
        // downstream flow trusts the projection to be exhaustive.
        throw new GitHubAppClientError(
          "pagination-overflow",
          new Error(
            `exceeded MAX_PAGES (${APP_INSTALLATIONS_MAX_PAGES}) ` +
              `walking GET /app/installations without observing a ` +
              `short page; refusing to silently truncate`,
          ),
        );
      }
      return aggregated;
    },

    async listInstallationRepositories(
      installationId: number,
    ): Promise<readonly InstallationRepository[]> {
      let installationAuth: InstallationAuthResult;
      try {
        installationAuth = await authHook({
          type: "installation",
          installationId,
          // Bypass the `@octokit/auth-app` LRU cache for the same reason
          // `app-auth.ts` does — the wizard's lifetime is bounded by a
          // single `makina setup` and we always want the freshest token.
          refresh: true,
        });
      } catch (error) {
        throw new GitHubAppClientError("mintInstallationToken", error);
      }
      // Walk every page of `/installation/repositories`. The endpoint
      // caps a page at 100; an installation with hundreds of repos is
      // possible (an org installation with `selected = "all"`), and the
      // wizard's downstream flattener relies on the full list to populate
      // its picker. The `page`/`per_page` parameters are GitHub-
      // documented and supported. Page size and the defensive max-page
      // cap are centralised in `src/constants.ts` so this module carries
      // no bare numeric literals.
      const aggregated: InstallationRepository[] = [];
      let sawShortPage = false;
      for (
        let page = 1;
        page <= APP_INSTALLATION_REPOSITORIES_MAX_PAGES;
        page += 1
      ) {
        let response: { data: unknown };
        try {
          response = await octokit.request("GET /installation/repositories", {
            per_page: APP_INSTALLATION_REPOSITORIES_PAGE_SIZE,
            page,
            headers: { authorization: `token ${installationAuth.token}` },
          });
        } catch (error) {
          throw new GitHubAppClientError("listInstallationRepositories", error);
        }
        const projected = projectRepositories(response.data);
        for (const repo of projected) {
          aggregated.push(repo);
        }
        if (projected.length < APP_INSTALLATION_REPOSITORIES_PAGE_SIZE) {
          // Short page → final page. Saves an extra round-trip for the
          // common case of an installation with under 100 repos.
          sawShortPage = true;
          break;
        }
      }
      if (!sawShortPage) {
        // The cap was hit without a short page, which means the list is
        // almost certainly truncated. Surface this loudly rather than
        // silently returning a partial repository set: the wizard's
        // downstream flow trusts the projection to be exhaustive.
        throw new GitHubAppClientError(
          "pagination-overflow",
          new Error(
            `exceeded MAX_PAGES (${APP_INSTALLATION_REPOSITORIES_MAX_PAGES}) ` +
              `walking GET /installation/repositories without observing a ` +
              `short page; refusing to silently truncate`,
          ),
        );
      }
      return aggregated;
    },
  };
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/**
 * Default strategy: real `@octokit/auth-app`.
 *
 * Returned hook is overloaded so calls into App-mode produce
 * `{ type: "app", token }` and calls into installation-mode produce the
 * `InstallationAccessTokenAuthentication` shape. We keep the projected
 * surface narrow; the rest of `@octokit/auth-app`'s payload (permissions,
 * repository selection, etc.) is not relevant to the wizard.
 */
function defaultCreateAppAuthStrategy(options: {
  readonly appId: number;
  readonly privateKey: string;
  readonly baseUrl?: string;
  readonly fetch?: typeof globalThis.fetch;
}): AppClientAuthHook {
  // The wider auth-app options object accepts `baseUrl` and a
  // `request: { fetch }` override — both are documented on
  // `@octokit/auth-app`. The local typing here is intentionally loose
  // to avoid pulling the full `@octokit/auth-app` types into our
  // surface; the runtime contract is what matters.
  const authOptions: Record<string, unknown> = {
    appId: options.appId,
    privateKey: options.privateKey,
  };
  if (options.baseUrl !== undefined) {
    authOptions.baseUrl = options.baseUrl;
  }
  if (options.fetch !== undefined) {
    authOptions.request = { fetch: options.fetch };
  }
  const auth = createAppAuth(
    authOptions as unknown as Parameters<typeof createAppAuth>[0],
  );
  // Single overloaded hook: forward to `auth(...)` and project the result
  // shape down to the narrow types this module owns.
  function hook(options: { readonly type: "app" }): Promise<AppAuthResult>;
  function hook(options: {
    readonly type: "installation";
    readonly installationId: number;
    readonly refresh: true;
  }): Promise<InstallationAuthResult>;
  async function hook(
    options:
      | { readonly type: "app" }
      | {
        readonly type: "installation";
        readonly installationId: number;
        readonly refresh: true;
      },
  ): Promise<AppAuthResult | InstallationAuthResult> {
    if (options.type === "app") {
      const result = await auth({ type: "app" });
      return { token: result.token };
    }
    const result = await auth({
      type: "installation",
      installationId: options.installationId,
      refresh: options.refresh,
    });
    return { token: result.token };
  }
  return hook;
}

interface AppInstallationResponse {
  readonly id: number;
  readonly account?: { readonly login?: string | null } | null;
}

interface InstallationRepositoryResponse {
  readonly owner?: { readonly login?: string | null } | null;
  readonly name?: string | null;
}

interface InstallationRepositoriesEnvelope {
  readonly repositories?: readonly InstallationRepositoryResponse[];
}

/**
 * Project the raw `/app/installations` payload into the typed
 * {@link AppInstallation} surface.
 *
 * Tolerates the documented payload shape strictly: a non-array body,
 * a missing numeric `id`, or an unknown JSON shape produce a
 * {@link GitHubAppClientError} so the wizard's diagnostic surfaces the
 * specific projection failure rather than a generic "TypeError" from a
 * downstream `.map`.
 */
/**
 * Render a `non-object` entry's type for an error message in a way
 * that distinguishes `null` from a real object — `typeof null === "object"`
 * in JavaScript, which makes "unexpected non-object entry: object" a
 * misleading diagnostic. Used by the projection helpers below.
 */
function describeNonObjectEntry(entry: unknown): string {
  if (entry === null) {
    return "null";
  }
  return typeof entry;
}

function projectInstallations(data: unknown): readonly AppInstallation[] {
  if (!Array.isArray(data)) {
    throw new GitHubAppClientError(
      "projectInstallations",
      new TypeError(
        `expected array from GET /app/installations, received ${typeof data}`,
      ),
    );
  }
  const out: AppInstallation[] = [];
  for (const entry of data as readonly AppInstallationResponse[]) {
    if (entry === null || typeof entry !== "object") {
      throw new GitHubAppClientError(
        "projectInstallations",
        new TypeError(
          `unexpected non-object installation entry: ${describeNonObjectEntry(entry)}`,
        ),
      );
    }
    const id = entry.id;
    if (typeof id !== "number" || !Number.isFinite(id)) {
      throw new GitHubAppClientError(
        "projectInstallations",
        new TypeError(
          `installation entry missing numeric id: ${JSON.stringify(entry)}`,
        ),
      );
    }
    const accountLogin = entry.account?.login ?? null;
    const projected: AppInstallation = accountLogin === null ||
        accountLogin === undefined
      ? { id }
      : { id, accountLogin };
    out.push(projected);
  }
  return out;
}

/**
 * Project the raw `/installation/repositories` payload into the typed
 * {@link InstallationRepository} surface.
 *
 * GitHub's response wraps the array in a `{ total_count, repositories }`
 * envelope (REST) — neither of which we expose. Anything else produces
 * a {@link GitHubAppClientError}.
 */
function projectRepositories(
  data: unknown,
): readonly InstallationRepository[] {
  if (data === null || typeof data !== "object") {
    throw new GitHubAppClientError(
      "projectRepositories",
      new TypeError(
        `expected object from GET /installation/repositories, received ${typeof data}`,
      ),
    );
  }
  const envelope = data as InstallationRepositoriesEnvelope;
  const repositories = envelope.repositories;
  if (!Array.isArray(repositories)) {
    throw new GitHubAppClientError(
      "projectRepositories",
      new TypeError(
        `repositories envelope missing array field: ${JSON.stringify(data)}`,
      ),
    );
  }
  const out: InstallationRepository[] = [];
  for (const entry of repositories) {
    if (entry === null || typeof entry !== "object") {
      throw new GitHubAppClientError(
        "projectRepositories",
        new TypeError(
          `unexpected non-object repository entry: ${describeNonObjectEntry(entry)}`,
        ),
      );
    }
    const owner = entry.owner?.login;
    const name = entry.name;
    if (typeof owner !== "string" || owner.length === 0) {
      throw new GitHubAppClientError(
        "projectRepositories",
        new TypeError(
          `repository entry missing owner.login: ${JSON.stringify(entry)}`,
        ),
      );
    }
    if (typeof name !== "string" || name.length === 0) {
      throw new GitHubAppClientError(
        "projectRepositories",
        new TypeError(
          `repository entry missing name: ${JSON.stringify(entry)}`,
        ),
      );
    }
    out.push({ owner, name });
  }
  return out;
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
