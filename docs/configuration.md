# Configuration

`src/config/schema.ts` defines the typed shape; `src/config/load.ts` reads, expands `~/`,
JSONC-parses, and validates the file. Errors include the failing field path.

## Location

- macOS: `~/Library/Application Support/makina/config.json`
- Linux: `~/.config/makina/config.json`

The file is parsed and validated on every daemon start; the daemon also tolerates a missing file by
binding a fallback `${TMPDIR:-/tmp}/makina.sock` so `--version` and the smoke test still work before
`makina setup` has run.

## Shape

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

Walks through the App ID, private-key path, installation discovery, and default repo, then writes
`config.json` to the platform-appropriate path. See `docs/setup-github-app.md` for App creation.

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
