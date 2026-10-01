# Build from source

[Back to the quick start](../README.md) · [中文](build.zh_CN.md)

Install a [Rust toolchain](https://www.rust-lang.org/tools/install), Git and the [runtime dependencies](dependencies.md). Then clone and build:

```bash
git clone https://github.com/Detteee/bilistream.git
cd bilistream
cargo build --locked --release --bin bilistream
./target/release/bilistream
```

On Windows, run `target\release\bilistream.exe` instead. Open `http://localhost:3150` and follow the [first-run guide](first-run.md).

## Web UI assets

Compiled Web UI assets are included. After editing `webui/src` or `webui/public-src`, rebuild them with Node.js 22+ before compiling Rust:

```bash
npm ci --prefix webui --ignore-scripts
npm --prefix webui run build
```

## Optional desktop app

The optional desktop package is `src-tauri` (`bilistream-tauri`). It shares the Rust backend and needs the [Tauri platform prerequisites](https://v2.tauri.app/start/prerequisites/).

## Linux cross-build

With [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild) and [Zig](https://ziglang.org/download/) installed, Linux cross-builds can use:

```bash
cargo zigbuild --locked --target x86_64-unknown-linux-gnu.2.36 --release
```
