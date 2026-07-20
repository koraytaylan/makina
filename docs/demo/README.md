# Makina Demo Tape

This directory contains a scriptable demo tape for the Makina TUI using [vhs](https://github.com/charmbracelet/vhs) (VHS — Your VHS tape editor).

## Status

The rendered GIF (`docs/demo/makina.gif`) is pending a vhs render. To generate it, install vhs and run the render command below.

## Rendering the Demo

To render the demo tape into a GIF:

```sh
vhs docs/demo/makina.tape
```

This will generate `docs/demo/makina.gif`, showing the Makina TUI in action.

## Installation

If you don't have vhs installed, install it from https://github.com/charmbracelet/vhs.

On macOS:
```sh
brew install charmbracelet/tap/vhs
```

On Linux, see the [vhs installation guide](https://github.com/charmbracelet/vhs#installation).

## Re-rendering

Re-running the render command will regenerate the GIF with the current version of Makina and vhs settings.

## Demo Contents

The tape:
1. Launches the Makina TUI via `cargo run -p makina`
2. Waits for the TUI to initialize
3. Opens a discovered plan directory (via `o` key)
4. Shows the task view briefly
5. Quits the application (via `q` key)

See `makina.tape` for the full script and timing.
