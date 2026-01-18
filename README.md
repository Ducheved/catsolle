# catsolle

TUI SSH client with dual-pane file manager and key management.

## Build

```
cargo build
```

## Run

```
./target/debug/catsolle
```

## CLI

```
./target/debug/catsolle connect user@host
./target/debug/catsolle keys generate --name id_ed25519 --algorithm ed25519
```

## Config

Default config path:
- Linux/macOS: ~/.config/catsolle/config.toml
- Windows: %APPDATA%\catsolle\config.toml

Initialize defaults:

```
./target/debug/catsolle config --init
```
