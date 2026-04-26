# Configuration

> Wave 1 freezes the typed shape (`src/config/schema.ts`); the loader (file IO, `~/` expansion,
> error pretty-printing) lands with Wave 2.

## Location

- macOS: `~/Library/Application Support/makina/config.json`
- Linux: `~/.config/makina/config.json`

The file is parsed and validated by zod on every daemon start; errors include the failing field
path.

## Shape (target)

```jsonc
{
  "github": {
    "appId": 1234567,
    "privateKeyPath": "~/.config/makina/app-private-key.pem",
    "installations": {
      "owner1/repoA": 9876543,
      "owner2/repoB": 9876544
    },
    "defaultRepo": "owner1/repoA"
  },
  "agent": {
    "model": "claude-sonnet-4-6",
    "permissionMode": "acceptEdits",
    "maxIterationsPerTask": 8
  },
  "lifecycle": {
    "mergeMode": "squash",
    "settlingWindowMilliseconds": 60000,
    "pollIntervalMilliseconds": 30000,
    "preserveWorktreeOnMerge": false
  },
  "workspace": "~/Library/Application Support/makina/workspace",
  "daemon": {
    "socketPath": "~/Library/Application Support/makina/daemon.sock",
    "autoStart": true
  },
  "tui": {
    "keybindings": {
      "commandPalette": "ctrl+p",
      "taskSwitcher": "ctrl+g"
    }
  }
}
```

## First-run setup

```bash
makina setup
```

Walks through the App ID, private-key path, installation discovery, and default repo. See
`docs/setup-github-app.md` for App creation.

> **Status (this PR):** the wizard's interactive flow is implemented but the GitHub App client used
> for installation discovery lands with `[W2-github-app-auth]` (#4). Until then, `makina setup`
> exits immediately with a pointer to issue #4. Set `MAKINA_ALLOW_WIZARD_STUB=1` to dry-run the
> prompts; the discovery step will fail with a clear "App client not yet wired" message.

## Loader behavior

`src/config/load.ts` is the only place the daemon and TUI read the file. It expands a single leading
`~/` against `$HOME` and then parses the file as JSONC, so `// line comments`,
`/* block comments */`, and trailing commas are tolerated. Validation goes through the same
`parseConfig` the W1 schema exposes; failures raise a `ConfigLoadError` whose `message` embeds the
failing field path (e.g. `github.installations["owner/repo"]`) and whose `kind` discriminates
`not-found`, `read-failed`, `invalid-json`, and `invalid-schema` so callers can branch (the daemon
prints "run `makina setup` first" on `not-found`, for example). Other path fields inside the config
(`github.privateKeyPath`, `daemon.socketPath`, `workspace`) keep their original `~/`-prefixed form;
each consumer expands them at the boundary it cares about.

Per ADR-008 (Windows deferred to a later wave), only POSIX `$HOME` is honored. WSL2 and other
POSIX-like environments work out of the box; native Windows is not yet supported and therefore not
emulated via `%USERPROFILE%`.
