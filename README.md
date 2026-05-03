# GlideClient Launcher

An unofficial Rust launcher for GlideClient on Minecraft 1.8.9.
Built with `eframe/egui` (no Electron).

## Run (.exe)

- Download launcher (.exe)
- Double Client for open launcher

## Features

- Microsoft account login (device code flow)
- Offline launch mode
- Launcher data in `%APPDATA%\\.glideclient`
- Resource packs from `%APPDATA%\\.minecraft\\resourcepacks`
- Memory slider with auto-detected max RAM
- Minimal custom UI with sliding settings panel

## Requirements

- Windows
- Rust toolchain (`cargo`)

## Run (dev)

```powershell
cargo run
```

## Build (release)

```powershell
cargo build --release
```

Output:

- `target\\release\\glide-client-launcher.exe`

## Data directories

- Launcher data: `%APPDATA%\\.glideclient`
- Minecraft directory: `%APPDATA%\\.minecraft`
- Resource packs: `%APPDATA%\\.minecraft\\resourcepacks`

## Notes

- `GlideClient.json` is used to launch GlideClient 1.8.9.
- Please follow the original project's terms before redistributing related assets.
