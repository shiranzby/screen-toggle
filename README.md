# screen-toggle v0.2.0

Rust / Win32 一键熄屏工具的历史源码快照。此标签的 `rust/` 对应该版本，当前版本请访问 [main](https://github.com/shiranzby/screen-toggle)。

## 目录

- `rust/src/`：对应版本的 Rust 源码。
- `rust/Cargo.toml`、`Cargo.lock`：包信息与固定依赖。
- `rust/build.rs`：Windows 资源构建。
- `VERSION`、`LICENSE`：版本与 MIT 许可。

## 编译检查

需要 Windows Rust MSVC 工具链、MSVC 链接器及 Windows SDK。

```powershell
cargo check --locked --manifest-path .\rust\Cargo.toml
cargo build --release --locked --manifest-path .\rust\Cargo.toml
```

已完成源码编译检查，未重做历史版本的界面和硬件验收。归档源码与早期用户提供的 EXE 没有逐字节构建一致性保证。

作者：[shiranzby](https://github.com/shiranzby)。
