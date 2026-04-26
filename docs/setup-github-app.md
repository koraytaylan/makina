# Setting up the GitHub App

> Wave 0 stub. Concrete steps will be revised when Wave 2's `setup` wizard lands.

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
2. Run `makina setup`. Provide the App ID (top of the App page) and the private-key path. The wizard
   then queries the App's installations endpoint to discover which repos you can target and asks you
   to pick a default.

## Copilot review

The "Copilot" reviewer is added by `POST /repos/.../pulls/.../requested_reviewers` with
`{ "reviewers": ["Copilot"] }`. Some installations require Copilot to be enabled on the repository
before this works; if the API returns 422, enable Copilot in the repo settings and retry.
