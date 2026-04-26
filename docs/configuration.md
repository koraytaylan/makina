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
