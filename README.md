# ClipStash

A lightweight, high-performance macOS clipboard manager written in Rust.

ClipStash maintains a configurable ring buffer of your clipboard history and provides two ways to access it: a persistent menu bar dropdown and a global-hotkey overlay picker.

## Features

- **Ring buffer clipboard history** -- keeps 1-10 items (default: 5)
- **All content types** -- text, rich text, images, files, URLs, and arbitrary binary data
- **Menu bar integration** -- click the "CS" icon to see and select clipboard items
- **Global hotkeys** -- Cmd+Shift+V for overlay picker, Cmd+Shift+1-5 for direct slot access, Cmd+Shift+[/] to cycle
- **SQLite persistence** -- history survives restarts
- **Lightweight** -- native Rust binary, ~4 MB, minimal CPU/memory footprint
- **Deduplication** -- BLAKE3 content hashing prevents duplicate entries

## Requirements

- macOS 13.0+
- Rust 1.75+ (for building)
- **Accessibility permission** required for global hotkeys (System Settings > Privacy & Security > Accessibility)

## Building

```sh
cargo build --release
```

The binary is at `target/release/clipstash`.

## Installation

```sh
# Copy to a directory in your PATH
cp target/release/clipstash /usr/local/bin/

# Or use cargo install
cargo install --path crates/clipstash-app
```

## Usage

```sh
# Run with default settings
clipstash

# Run with debug logging
RUST_LOG=debug clipstash
```

On first launch, macOS will prompt you to grant Accessibility permissions. Hotkeys won't work until this is granted.

## Configuration

ClipStash looks for a config file at `~/Library/Application Support/clipstash/config.toml` (macOS). If none exists, defaults are used.

```toml
capacity = 5              # Number of items to keep (1-10)
poll_interval_ms = 250    # Pasteboard polling interval
picker_hotkey = "cmd+shift+v"
direct_slot_hotkeys = true
cycle_forward_hotkey = "cmd+shift+]"
cycle_backward_hotkey = "cmd+shift+["
persist = true            # Save history across restarts
max_item_bytes = 52428800 # 50 MB max per item
launch_at_login = false
show_previews = true
preview_length = 80
sound_on_capture = false
auto_paste = true         # Synthesize Cmd+V after selecting an item
```

## Default Hotkeys

| Hotkey | Action |
|---|---|
| Cmd+Shift+V | Toggle overlay picker |
| Cmd+Shift+1-5 | Jump to slot 1-5 directly |
| Cmd+Shift+] | Cycle forward (older items) |
| Cmd+Shift+[ | Cycle backward (newer items) |

## Architecture

ClipStash is organized as a Cargo workspace with six crates:

```
crates/
  clipstash-types/    # Shared types, config, errors
  clipstash-store/    # Ring buffer + SQLite persistence
  clipstash-pb/       # macOS pasteboard monitor (NSPasteboard)
  clipstash-hotkey/   # Global hotkey registration (CGEvent)
  clipstash-ui/       # Menu bar + overlay picker (AppKit)
  clipstash-app/      # Application entry point
```

**Thread model:**
- Main thread: macOS run loop (UI + CGEvent)
- Background thread: pasteboard polling (250ms interval)
- Communication: `std::sync::mpsc` channels

**Persistence:** SQLite with WAL mode at `~/.local/share/clipstash/history.db`. Content serialized via MessagePack.

## Data Locations

| Path | Purpose |
|---|---|
| `~/Library/Application Support/clipstash/config.toml` | Configuration |
| `~/.local/share/clipstash/history.db` | Clipboard history (SQLite) |
| `~/.local/share/clipstash/clipstash.pid` | PID file (instance locking) |

## License

MIT
