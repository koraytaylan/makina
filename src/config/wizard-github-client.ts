/**
 * config/wizard-github-client.ts — production
 * {@link "./setup-wizard.ts".WizardGitHubClient} backed by an
 * {@link "../github/app-client.ts".AppClient}.
 *
 * The wizard's `WizardGitHubClient` interface is intentionally narrow —
 * one method, `getInstallations({ appId, privateKeyPath })` — so the
 * unit tests can stub it without inheriting the full
 * `@octokit/auth-app` + Octokit surface. This module is the production
 * wiring that bridges that narrow interface to the real GitHub API:
 *
 *   1. Read the PEM private key from disk (with `~/` expansion via
 *      {@link "./load.ts".expandHome}).
 *   2. Construct an {@link AppClient} bound to the App credentials.
 *   3. Call {@link AppClient.listAppInstallations} to enumerate every
 *      installation the App can see.
 *   4. For each installation, call
 *      {@link AppClient.listInstallationRepositories} to fetch the
 *      repositories that installation can act on.
 *   5. Project the joined result into the
 *      {@link WizardInstallation}[] shape the wizard renders to the
 *      user as a numbered picker.
 *
 * The bridge lives in `src/config/` rather than `src/github/` because
 * the consumer is the config wizard — keeping the module beside
 * `setup-wizard.ts` makes the wiring locality obvious without bloating
 * the App-level GitHub surface with config-specific knowledge of the
 * wizard's `~/`-prefixed paths.
 *
 * **Test seam.** Both collaborators are injectable: the disk read goes
 * through a `readKeyFile` callback (so unit tests can hand in an
 * in-memory key without writing a temp file) and the AppClient is
 * created via a `createClient` callback (so unit tests can pass a
 * scripted double instead of a real `Octokit` round-trip).
 *
 * @module
 */

import {
  type AppClient,
  createAppClient,
  type CreateAppClientOptions,
} from "../github/app-client.ts";
import { defaultEnvLookup, type EnvLookup, expandHome } from "./load.ts";
import {
  SetupWizardError,
  type WizardGitHubClient,
  type WizardInstallation,
} from "./setup-wizard.ts";

/**
 * Read a private key from disk given a `~/`-prefixed (or absolute) path.
 *
 * Tests inject an in-memory variant; production code uses
 * {@link defaultReadKeyFile}, which delegates to `Deno.readTextFile`
 * after expanding `~/`.
 */
export type ReadKeyFile = (
  privateKeyPath: string,
  envLookup: EnvLookup,
) => Promise<string>;

/**
 * Production-time {@link ReadKeyFile}. Expands `~/` and reads the file
 * as UTF-8 text. The expanded path is the same one the wizard's
 * existence check uses, so a failure here is unambiguous: the file did
 * exist when the user typed the path but is no longer readable.
 *
 * @param privateKeyPath The user-supplied path (may begin with `~/`).
 * @param envLookup Env reader for `$HOME` resolution. The default
 *   delegates to `Deno.env.get`.
 * @returns The PEM contents of the file.
 * @throws Whatever `Deno.readTextFile` throws (e.g. `Deno.errors.NotFound`,
 *   `Deno.errors.PermissionDenied`). The thrown value is caught and
 *   re-thrown as a `Setup_*` error in
 *   {@link createWizardGitHubClient}'s `getInstallations` body before
 *   it ever reaches `setup-wizard.ts`; that re-throw carries the
 *   original message in its `cause` chain so operators still see the
 *   underlying filesystem error.
 */
export async function defaultReadKeyFile(
  privateKeyPath: string,
  envLookup: EnvLookup,
): Promise<string> {
  const expanded = expandHome(privateKeyPath, envLookup);
  return await Deno.readTextFile(expanded);
}

/**
 * Factory used to construct the underlying {@link AppClient}. Tests pass
 * a scripted version; production omits this and the real
 * {@link createAppClient} is used.
 *
 * @internal
 */
export type CreateAppClient = (opts: CreateAppClientOptions) => AppClient;

/**
 * Options accepted by {@link createWizardGitHubClient}.
 */
export interface CreateWizardGitHubClientOptions {
  /**
   * Disk reader for the App's private key. Defaults to
   * {@link defaultReadKeyFile}; tests inject an in-memory reader so they
   * can hand the wizard a synthetic key without writing a temp file.
   *
   * @internal
   */
  readonly readKeyFile?: ReadKeyFile;
  /**
   * Env reader used by the default {@link ReadKeyFile} for `~/`
   * expansion. Tests inject a per-test stub to avoid mutating
   * `Deno.env` and racing under `--parallel`. Production omits this and
   * the wizard falls back to `Deno.env.get`.
   *
   * @internal
   */
  readonly envLookup?: EnvLookup;
  /**
   * Inject an alternative {@link AppClient} factory. Tests pass a
   * scripted double; production omits this and {@link createAppClient}
   * is used.
   *
   * @internal
   */
  readonly createClient?: CreateAppClient;
}

/**
 * Build the production-time {@link WizardGitHubClient}.
 *
 * The wizard prompts the user for an App ID and a private-key path,
 * then calls `getInstallations({ appId, privateKeyPath })` exactly once.
 * This factory returns an object that, on that call, walks
 * `/app/installations` + `/installation/repositories` and projects the
 * combined result into the {@link WizardInstallation}[] shape the
 * wizard renders.
 *
 * @param options See {@link CreateWizardGitHubClientOptions}. Every
 *   field is optional; production callers omit them all.
 * @returns A wizard-ready {@link WizardGitHubClient}.
 *
 * @example
 * ```ts
 * const client = createWizardGitHubClient();
 * const io = createStdioWizardIo(client);
 * const config = await runSetupWizard(io);
 * ```
 */
export function createWizardGitHubClient(
  options: CreateWizardGitHubClientOptions = {},
): WizardGitHubClient {
  const readKeyFile = options.readKeyFile ?? defaultReadKeyFile;
  const envLookup = options.envLookup ?? defaultEnvLookup;
  const createClient = options.createClient ?? createAppClient;

  return {
    async getInstallations(args: {
      readonly appId: number;
      readonly privateKeyPath: string;
    }): Promise<readonly WizardInstallation[]> {
      let privateKey: string;
      try {
        privateKey = await readKeyFile(args.privateKeyPath, envLookup);
      } catch (error) {
        // Promote the disk failure to the wizard's discriminated error so
        // the `setup` subcommand prints a tidy single-line diagnostic.
        // Without this rewrap the wizard's own catch block would still
        // wrap the error, but the message would read "failed to list
        // installations: ..." even though the failure was a key-read,
        // which is more confusing than helpful.
        throw new SetupWizardError(
          `failed to read private key at ${args.privateKeyPath}: ${describeError(error)}`,
        );
      }

      const client = createClient({
        appId: args.appId,
        privateKey,
      });

      const installations = await client.listAppInstallations();
      // Parallelise the per-installation repo fetch. Each call is
      // independent (App-scoped pagination per installation id), and
      // serializing them via a `for-await` made `makina setup`
      // perceptibly slow for Apps with many installations: N
      // round-trips at the network's RTT each. `Promise.all` collapses
      // wall-clock time to the slowest single call, which matters when
      // the wizard is the user's first impression of the daemon.
      // GitHub's installation-token rate-limit is per installation, so
      // parallel calls do not stack against a shared bucket.
      //
      // GitHub's `<owner>/<name>` slug is the wizard's canonical
      // repo identifier (it is what `config.json` stores as the key
      // of `github.installations`). Build it once here so the
      // wizard's downstream logic does not also need to know about
      // owner/name splitting.
      return await Promise.all(
        installations.map(async (installation) => {
          const repos = await client.listInstallationRepositories(installation.id);
          return {
            installationId: installation.id,
            repositories: repos.map((repo) => `${repo.owner}/${repo.name}`),
          };
        }),
      );
    },
  };
}

/**
 * Coerce an unknown thrown value to a printable message. Mirrors the
 * helper in `app-client.ts`; we duplicate the three lines to keep this
 * module's import surface narrow.
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
