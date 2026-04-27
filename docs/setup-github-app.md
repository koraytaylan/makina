# Setting up the GitHub App

> Wave 2's `setup` wizard drives steps 3.2 onwards. Steps 1 and 2 are manual GitHub actions taken in
> your browser; the wizard handles everything after the App is installed. The wizard runs end-to-end
> except for installation discovery, which lands with `[W2-github-app-auth]` (#4) — until then the
> wizard exits with a clear "not yet implemented" message at that step.

makina authenticates to GitHub as a GitHub App (rather than a personal access token). This is
ADR-003: it gives us higher rate limits, fine-grained per-repo permissions, and a clean bot identity
for commits.

## 1. Create the App

1. Go to <https://github.com/settings/apps/new>.
2. **App name**: any unique value, e.g. `makina-<your-handle>`.
3. **Homepage URL**: anything (the App is local-only, no callback).
4. **Webhook**: uncheck "Active". makina polls; we do not host an HTTPS endpoint.
5. **Repository permissions** (this is the minimum that the daemon and stabilize loop require):
   - Contents: **Read and write**
   - Issues: **Read**
   - Metadata: **Read** (mandatory)
   - Pull requests: **Read and write**
   - Commit statuses: **Read**
   - Checks: **Read**
6. **Where can this App be installed?**: Only on this account.
7. Create the App, then on the App page generate a **Private key** (downloads a `.pem` file).

## 2. Install the App on your repo(s)

On the App page → **Install App** → choose the account → choose specific repositories → Install.

## 3. Wire up makina

1. Save the `.pem` somewhere private — e.g. `~/.config/makina/app-private-key.pem` with mode `0600`.
2. Run `makina setup`. Once the App-level client lands (issue #4 — `[W2-github-app-auth]`), the
   wizard will ask for the App ID, private-key path, query the App's installations endpoint to
   discover which repos you can target, and write the resulting `config.json` to the
   platform-appropriate location (see `docs/configuration.md`).

> **Status (this PR):** the wizard's prompts, validation, and config-writing are implemented and run
> unconditionally. The GitHub App client used for installation discovery is wired in
> `[W2-github-app-auth]` (#4); until that lands, `makina setup` will fail with a clear "GitHub App
> auth not yet implemented (#4)" error at the discovery step (after the App ID and private-key path
> have been collected). When the App client lands the wizard completes end-to-end with no further
> changes here.

The wizard validates the private-key path against the filesystem before calling GitHub, prints the
list of reachable repositories with one-based numbers, and writes the resulting `config.json`. On
EOF or invalid input the wizard exits with a one-line summary; rerun the command to start over.

## Copilot review

The "Copilot" reviewer is added by `POST /repos/.../pulls/.../requested_reviewers` with
`{ "reviewers": ["Copilot"] }`. Some installations require Copilot to be enabled on the repository
before this works; if the API returns 422, enable Copilot in the repo settings and retry.
