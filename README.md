# GlideClient Launcher

Rust + `eframe/egui` で作った、Minecraft 1.8.9 向け GlideClient ランチャーです。
Electron は使っていません。

## Features

- Microsoft アカウントログイン（デバイスコードフロー）
- オフライン起動
- `%APPDATA%\\.glideclient` にランチャーデータを保存
- テクスチャパックは `%APPDATA%\\.minecraft\\resourcepacks` を参照
- メモリ割り当て（PC の物理メモリに応じて上限を自動調整）
- 最小構成のカスタム UI（設定画面スライドイン）

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

生成物:

- `target\\release\\glide-client-launcher.exe`

## Data Directories

- ランチャーデータ:
  - `%APPDATA%\\.glideclient`
- Minecraft 本体:
  - `%APPDATA%\\.minecraft`
- Resource Packs:
  - `%APPDATA%\\.minecraft\\resourcepacks`

## Notes

- `GlideClient.json` をもとに GlideClient 1.8.9 を起動します。
- 配布物の権利や利用規約は、必ず配布元のルールを確認してください。
