# 参与开发

这个项目的规矩很少，但每一条都是踩出来的（详见 `docs/04-工程规约.md`，目前 72 条）。

## 前置要求

- Rust（stable）。需要两个裸机目标：

```bash
rustup target add x86_64-unknown-none x86_64-unknown-uefi
```

- Python 3.8+（构建脚本 `build.py` / `tools/*.py` 全部是标准库，无第三方依赖）
- 想在虚拟机里跑自检的话再装 VirtualBox 7.x（脚本用 `VBoxManage`）

> 本仓库不携带 Rust 工具链（1.2 GB）。如果你的机器完全离线，
> 把一份 rustup 安装目录放到 `tools/rust/`（结构为 `tools/rust/cargo/bin/cargo`），
> `build.py` 会自动优先使用它。

## 提交前必须过的三关

```bash
cargo test --workspace --exclude lumikernel   # 单测全绿
python tools/unsafe-audit.py --check          # unsafe 密度 ≤ 5% 且逐条有证明
python build.py                               # 能出 ISO（不需要虚拟机）
```

涉及内核行为改动的，还要跑一次：

```bash
bash scripts/vmtest-efi.sh out/LumiKernel-0.1-efi.img 80 check 1
# 期望串口里出现 `M11 self-test 58/58 passed`，且没有 [FAIL] / panic
```

## 加功能的顺序

1. **先写用例**：纯逻辑放 crate 单测（宿主机可跑），涉及硬件放内核自检块。
2. **再写实现**：`unsafe` 只允许出现在三类边界（arch / MMIO-DMA / 已被证明不变量的裸指针），
   每处必须有 `// SAFETY:` 说明。
3. **最后写文档**：新踩的坑进 `docs/04-工程规约.md`，修掉的 bug 进 `docs/回归用例.md`。

## 不需要的东西

不需要你先有"完整的内核知识"——这个仓库里每一个文件开头都写了"为什么这样做"，
以及"不这样做会怎样"。按 `docs/02-路线图.md` 挑一个里程碑往下做就行。
