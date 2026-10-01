# 源码编译

[返回快速开始](../README.zh_CN.md) · [English](build.md)

安装 [Rust 工具链](https://www.rust-lang.org/tools/install)、Git 和[运行依赖](dependencies.zh_CN.md)，再克隆并编译：

```bash
git clone https://github.com/Detteee/bilistream.git
cd bilistream
cargo build --locked --release --bin bilistream
./target/release/bilistream
```

Windows 请改为运行 `target\release\bilistream.exe`。打开 `http://localhost:3150`，按[首次设置说明](first-run.md)操作。

## Web UI 资源

仓库已包含编译后的 Web UI。修改 `webui/src` 或 `webui/public-src` 后，先用 Node.js 22+ 重新生成，再编译 Rust：

```bash
npm ci --prefix webui --ignore-scripts
npm --prefix webui run build
```

## 可选桌面应用

可选桌面包位于 `src-tauri`（`bilistream-tauri`），共用 Rust 后端，需要 [Tauri 平台依赖](https://v2.tauri.app/start/prerequisites/)。

## Linux 交叉编译

安装 [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild) 和 [Zig](https://ziglang.org/download/) 后，可用以下命令进行 Linux 交叉编译：

```bash
cargo zigbuild --locked --target x86_64-unknown-linux-gnu.2.36 --release
```
