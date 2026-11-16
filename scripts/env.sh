#!/usr/bin/env bash
# LumiKernel 开发环境（Git Bash 里 `source scripts/env.sh`）
# 作用：把随仓库携带的 Rust 工具链接进 PATH，不污染系统环境。
# 没有自带工具链时什么都不做——用系统里的 rustup/cargo 即可。

_LUMI_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [ -d "$_LUMI_ROOT/tools/rust/cargo/bin" ]; then
  export RUSTUP_HOME="$_LUMI_ROOT/tools/rust/rustup"
  export CARGO_HOME="$_LUMI_ROOT/tools/rust/cargo"
  export PATH="$_LUMI_ROOT/tools/rust/cargo/bin:$PATH"
fi

# 每个 crate 独立 target 目录，避免多个助理并行编译时抢锁
if [ -n "$LUMI_CRATE" ]; then
  export CARGO_TARGET_DIR="$_LUMI_ROOT/target/$LUMI_CRATE"
fi
