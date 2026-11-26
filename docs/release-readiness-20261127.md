# 当前合入门禁（2026-11-27）

状态：**BLOCKED（不可合入主分支或发版）**

当前候选分支为 `codex/snug-rust-main-integration`，基于 `origin/main` 的
`c223cd4`，把 Rust 工作树作为产品替换导入主线候选并形成独立提交。设置面板没有丢失，入口仍在
`crates/snug-app/src/settings.rs` 和 `crates/snug-app/src/settings/desktop.rs`，
包含屏幕、交互、通用、分辨率、自启、驱动、更新和日志入口。候选工作树的
三个 crate 版本统一为 `1.4.0`；本机实际运行的旧安装版仍可能显示旧设置界面，
需要启动候选构建才能观察 Rust 设置面板。

## 已验证

- `cargo metadata --offline --no-deps --format-version 1`：三个 crate 均为 `1.4.0`。
- `cargo test --offline --workspace --locked`：通过，908 项测试通过，1 项忽略。
- 提交既有 Clippy 门槛 `-D clippy::correctness -D clippy::suspicious`：通过。
- Rust 代码中的 owner 保护、无原生写入回退和横屏失败安全行为已有离线回归。
- 设置持久化回归：5 项通过；驱动包一致性离线回归：8 项通过。

## 当前阻断

1. **横屏 owner 隔离尚未完成实机通过。** `docs/acceptance.md` 的最新失败记录显示驱动目标为 `1920×1080`，但桌面仍为 `1080×1920`；CCD hold 为 0，应用没有启动。当前实现保持 owner 范围更新并在实际模式未收敛时失败，不应插回无 owner 的 GDI/CCD 写入。
2. **手机稳定性和异常恢复没有足够证据。** 同一进程 UU 导航、面板跳页/虚拟屏意外消失现场、受控故障恢复、正常退出和重启、强杀残留恢复，以及至少 30 分钟手机画面/点击/滚动长稳仍未完成。已有约 903 秒 soak 不能替代这些门槛。
3. **正式驱动包不一致。** `driver/source.lock.json` 要求源码驱动 `0.4.0.12` 和五项 owner 协议能力，但 releasePackage 和应用内嵌仍为 `0.3.1`。当前工作树没有可复验的 `0.4.0.12` ZIP、签名材料或构建所需 WDK/SDK/LLVM 输入，不能伪造哈希、清单或签名。

严格 `cargo fmt --all -- --check` 仍报告仓库既有的整体格式基线差异；未自动重排，以免改动 vendored 驱动源码哈希和扩大迁移噪声。严格 `clippy -D warnings` 也有既有非正确性 lint；正确性/可疑性门槛已通过。

历史验收报告中的通过记录只描述当时封存的阶段产物；对应 `target` 证据目录当前不在工作树中，不能把它们当作本轮可复验的合入证据。

在以上三项清除前，只保留主线集成候选提交和离线验证结果，不推进到主分支，不发布安装包。