# 当前合入门禁（2026-11-27）

状态：**BLOCKED（不可合入主分支或发版）**

当前候选分支为 `codex/snug-rust-main-integration`，基于 `origin/main` 的
`c223cd4`。Rust 本地 HEAD `7706d1e` 是候选合并提交 `e9454e8` 的第二父提交，
`git merge-base --is-ancestor rust HEAD` 通过；候选相对主线包含 346 个文件的 Rust 产品迁移。
Rust 与候选中的 `settings.rs`、`desktop.rs`、`desktop/screens.rs` 文件哈希一致。面板保留
屏幕、交互、通用、分辨率、自启、驱动、更新和日志入口；分辨率预设、自定义宽高及显式点击
“应用分辨率”后才生效的实现仍在候选中。

本机 `C:\Program Files\SnugWin\SnugWin.App.exe` 是旧安装版 `1.2.0.0`；当前 Snug 进程
映像则为候选 `target/release/snug.exe`，FileVersion/ProductVersion 均为 `1.4.0`。
当前 Windows 窗口清单没有返回 Snug 窗口，UU 远程画面显示的是 IDE，因此无法确认用户此前看到的
设置界面由哪个窗口呈现；旧安装版仍存在，可能造成从旧快捷方式启动时看到不同界面。

## 已验证

- `cargo metadata --offline --no-deps --format-version 1`：三个 crate 均为 `1.4.0`。
- `cargo test --offline --workspace --locked` 与 `cargo test --offline --workspace --release --locked`：本轮对最终候选各通过 909 项测试、1 项忽略；新增的竖屏切横屏失败关闭回归在 Debug/Release 均通过。
- `cargo clippy --offline --workspace --all-targets --locked -- -D clippy::correctness -D clippy::suspicious`：通过；仍有非阻断风格告警。
- `cargo build --offline --release -p snug-app --locked`：通过；EXE 的 FileVersion 与 ProductVersion 均为 `1.4.0`。
- 本机候选工作树外的 ignored 资源采样记录包含 31 个约每分钟样本，覆盖 30.05 分钟；候选进程一直响应，Private Bytes 约 641.85→641.66 MiB、Working Set 约 272.50→272.48 MiB。这只是资源采样，不包含手机画面、点击或滚动验收。
- `.github/workflows/ci.yml` 配置了 PR、`main` 推送及手动触发的 Windows CI；当前候选尚无 PR，也没有对应的远端 runner 结果。分支推送本身不触发该 workflow。
- Rust 代码中的 owner 隔离、无原生写入回退及失败后保留原 owner/账本已有离线回归；另有 portrait→landscape 模式不收敛时的专项回归。
- 设置持久化回归：5 项通过；驱动包一致性离线回归：8 项通过。

## 当前阻断

1. **横屏 owner 隔离尚未完成实机通过。** `docs/acceptance.md` 记录的失败是驱动目标为 `1920×1080`、桌面仍显示 `1080×1920`，且 CCD hold 为 0、应用未启动；对应 runner 原始产物当前缺失，不能重新核查。代码离线测试证明失败会保留原 owner/账本且不做 GDI、临时分辨率或 DPI 写入，但不能证明真实 CCD/驱动会收敛。
2. **手机稳定性和异常恢复没有足够证据。** 30.05 分钟资源采样虽通过，但没有手机画面和每 5 分钟点击/滚动观察。UU 同进程导航、面板跳页/虚拟屏意外消失现场、受控故障恢复、正常退出和重启、强杀残留恢复，以及手机交互长稳均未完成。
3. **正式驱动包不一致。** `driver/source.lock.json` 要求源码驱动 `0.4.0.12` 和五项 owner 协议能力，但 releasePackage 和应用内嵌仍为 `0.3.1`。当前仓库没有匹配的已签名 `0.4.0.12` 包；本机虽有 Windows SDK 和 Rust nightly，但缺少构建脚本要求的 UMDF/IddCx headers、Stampinf 与 Inf2Cat，且未发现可验证的正式签名产物。不能伪造哈希、清单或签名。

严格 `cargo fmt --all -- --check` 仍报告仓库既有的整体格式基线差异；未自动重排，以免改动 vendored 驱动源码哈希和扩大迁移噪声。严格 `clippy -D warnings` 也有既有非正确性 lint；正确性/可疑性门槛已通过。

历史验收报告中的通过记录只描述当时封存的阶段产物；对应 `target` 证据目录当前不在工作树中，不能把它们当作本轮可复验的合入证据。用户提供的横屏对照截图显示 `landscape-create-control-r1/start-matrix.ps1` 在第 32 行报告子 runner 退出码 1。截图引用的脚本、manifest、bootstrap JSON 及原始 `result.json`/`events.jsonl` 均不存在；历史文档只能将失败定位在模式收敛/target-hold 前，不能区分子进程退出还是超时，也不能确认底层根因。

在以上三项清除前，只保留主线集成候选提交和离线验证结果，不推进到主分支，不发布安装包。
