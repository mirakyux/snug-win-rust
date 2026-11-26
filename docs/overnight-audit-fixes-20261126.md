# 多轮代码、交互与驱动审查记录（2026-11-26）

本次完成 3 轮审查、修复和交叉验收，共处理 12 组已确认问题。范围包含应用、配置存储、权限代理和仓库内驱动。此前的 8 项生命周期修复另见[上一轮记录](lifecycle-audit-fixes-20261126.md)，不计入本次新增数量。

保留工作区既有修改。分辨率编辑继续作为草稿，用户显式应用后才生效。未新增依赖，未提交、推送、安装驱动或改变真实显示拓扑。

## 第一轮：创建结果、协议与初始化

| 问题 | 修复后行为 | 主要修改文件 |
| --- | --- | --- |
| V2 已执行 Create，但应答丢失后遗失归属，甚至继续创建第二块屏 | 以随机 token 预留编号；Create 前成功持久化意图。未知结果保留 token 和 pending 编号，不自动重放或换号；清理只操作匹配 token 的资源 | `crates/snug-win/src/display.rs`、`vdd.rs`、`vdd_broker.rs`、`crates/snug-core/src/store.rs`；驱动 `src/ipc.rs`、`driver-ipc/src/core.rs` |
| Legacy 替换失败后重连到 V2，继续发送裸 Add | 重连后重新核验协议；拒绝把 Legacy 写请求发给 V2 | `crates/snug-win/src/vdd.rs` |
| 忽略 DPI 设置失败，创建或调整误报成功 | 检查设置结果，并有界回读身份、分辨率和 DPI；失败沿用既有撤屏或回滚流程 | `crates/snug-win/src/display.rs` |
| 驱动忽略异步适配器初始化失败，仍发布适配器并启动 IPC | 检查 `AdapterInitStatus`；失败仅确认通知，不发布适配器、不启动 IPC | 驱动 `src/callbacks.rs` |
| D0Entry 吞掉同步初始化失败 | 向 WDF 返回真实错误；拒绝空设备指针 | 驱动 `src/callbacks.rs` |

## 第二轮：模式查询、保存与迟到请求

| 问题 | 修复后行为 | 主要修改文件 |
| --- | --- | --- |
| 模式查询将合法空缓冲查询视为错误，甚至构造 NULL slice | NULL 或零容量仅返回模式数量；足够容量才构造输出切片 | 驱动 `src/callbacks.rs` |
| 设置提示已保存，但多数已提交改动直到退出才落盘 | 每次 committed change 立即保存；失败在手机和桌面设置窗口显示持久错误与重试入口。重试只保存已提交配置，保留分辨率草稿 | `crates/snug-app/src/ui.rs`、`settings.rs`、`settings/desktop.rs` |
| Reserve 在取消后才执行，永久占用空编号 | 空预留租期 30 秒；Reserve 和 Create 清理过期空预留。已经接受的 Monitor 持续保留 token，包括 native arrival 失败的屏幕 | 驱动 `src/ipc.rs` |

## 第三轮：编号复用与交互恢复

| 问题 | 修复后行为 | 主要修改文件 |
| --- | --- | --- |
| 外部删除并重建相同编号后，安全清理仍等待他人屏幕下线 | 匹配凭据的 Release 返回 `NotFound` 时转换为 `NotOwned`；安全销账，不等待他人的屏幕。实际删除成功仍等待本屏下线；销账失败保留状态供重试 | `crates/snug-win/src/display.rs`、`vdd.rs`、`vdd_broker.rs`；驱动 `src/ipc.rs` |
| 删除当前显式选择的屏幕后，残留选择使 controller 和界面卡空 | 恢复到现存且启用的主触控屏或其他屏，同步 controller；独立修复预览选择，保留仍有效的选择 | `crates/snug-app/src/ui.rs` |
| 隐藏或禁用编号占用时，创建停在首个冲突候选；代理丢失可信错误类别 | 保留结构化 `AlreadyExists` 等错误；仅在明确冲突、guarded 清理及销账成功后尝试下一候选。未知结果和清理失败仍停止 | `crates/snug-win/src/display.rs`、`vdd_broker.rs` |
| Legacy 检测后清理临时意图失败，造成运行期及重启后的编号滞留 | 简化为 Reserve → 确认成功后持久化 → Create。Legacy 不写入临时 token；V2 写盘失败不 Create，取消空预留并保留必要的重试状态 | `crates/snug-win/src/display.rs` |

最后一项先在隔离快照中复现：未发 Create、未返回 pending 编号，同进程重试跳号，重启后无法安全销账。修复后的独立回归覆盖意图路径 4 项和 Reserve 应答丢失 1 项，全部通过。证据位于 `target/legacy-intention-release-proof-20261126/`。

## 归属边界与简化

`SnugReserve` 不改变拓扑。只有确认预留且意图落盘后，才发送 `SnugCreate`。预留后、落盘前崩溃只留下有租期的空预留；Create 的未知结果则始终有持久化 token 可清理。旧 V2 未知 token 保持守卫处理，不能因当前连接为 Legacy 而改用裸 Remove。

创建、替换和删除分别使用 `SnugCreate`、`SnugReplace`、`SnugRelease`。客户端同时验证回复的操作名与 token，损坏或丢失的写入回复归类为 `OutcomeUnknown`。只有可信 Release 的 `NotFound` 才表示 `NotOwned`，其他操作的 `NotFound` 继续报错。

- 复用已有撤屏、回滚、pending 恢复和设置同步流程，删除 Legacy 的“先写临时意图、再删除”步骤。
- 保持原 `DisplayIds` 账本的 C# 格式，以独立 sidecar 保存 token；损坏的 token 文件拒绝改写。
- 权限代理保留结构化错误类别，不根据错误文本推断归属或重试条件。
- 驱动所有写入共用现有修改锁；回归脚本复用生产源码提取器，不复制一套修复实现。
- `crates/snug-win/src/driver_health.rs` 同步处理未知结果；驱动版本与资源统一为 `0.4.0.5`。

新增及更新的回归位于 `driver/tests/`、`scripts/tests/` 和对应 Rust 测试模块。协议定义见[归属预留协议](../driver/atomic-update.md)，构建方式见[驱动说明](../driver/README.md)。

## 验证结果

| 验证 | 结果 | 证据 |
| --- | --- | --- |
| 工作区 Debug 全部目标 | 780 通过，0 失败，1 跳过 | `target/overnight-workspace-debug-20261126.log` |
| 工作区 Release 全部目标 | 780 通过，0 失败，1 跳过 | `target/overnight-workspace-release-20261126.log` |
| 工作区 Clippy | correctness、suspicious 设为错误后通过；其他建议类告警仍存在 | `target/overnight-workspace-clippy-20261126.log` |
| 应用 Release 构建 | 成功，文件与产品版本均为 1.0.7 | `target/overnight-app-release-build-20261126.log`、`target/overnight-app-artifact-20261126.json`、`target/release/snug.exe` |
| 设置保存与删除选屏的生产方法回归 | Debug、Release 各 5 通过 | `target/overnight-settings-final-20261126.log` |
| 最终 display 回归 | 99 通过 | `target/display-repair-round2-20261126/display-intention-order-tests-final.log` |
| 客户端及代理专项 | 客户端 30、代理 13 通过 | `target/broker-audit-round2-20261126/` |
| 固定 nightly 的 driver-ipc 库测试 | 17 通过 | `target/overnight-driver-ipc-20261126.log` |
| 驱动生命周期、归属与模式边界 | Debug、Release 各 32 通过；独立复跑同样通过 | `target/driver-owned-final-20261126.log`、`target/driver-owned-independent-final-20261126/` |
| 驱动初始化回归 | Debug、Release 各 6 通过 | `target/overnight-driver-init-20261126.log` |
| 原子协议回归 | 10 通过 | `target/overnight-driver-atomic-20261126.log` |
| 光标与 swapchain 资源回归 | 11 通过 | `target/overnight-driver-resource-20261126.log` |
| 驱动 x64 Release 构建及 Inf2Cat | 构建成功；Inf2Cat 无错误、无警告 | `target/driver-owned-build-20261126.log` |
| 独立产物与 ABI 验收 | DLL/INF 版本一致，4 项校验和、5 个 ZIP 成员哈希、14 项 ABI 一致 | `target/driver-owned-build-20261126/independent-artifact-acceptance.log` |
| 固定 nightly 的 4 个驱动生产文件 rustfmt | 通过 | `target/overnight-driver-rustfmt-20261126.log` |
| 差异与脚本语法检查 | `git diff --check` 通过；6 份回归 PowerShell 脚本解析通过 | 工作区差异及 `driver/tests/`、`scripts/tests/` |

工作区全测使用 `cargo test --workspace --all-targets --offline`，Release 增加 `--release`；Clippy 使用 `cargo clippy --workspace --all-targets --offline -- -D clippy::correctness -D clippy::suspicious`。两种测试配置跳过的均为既有 `elevated_atomic_staging_inherits_production_acl`，需要提升的 Windows 令牌。此次未为运行该项而提权。整仓排版仍有既有差异，未扩大为全仓格式化。

## 产物与剩余验证

应用仍为 `1.0.7`。驱动为 `0.4.0.5`，未签名包位于 `target/driver-owned-build-20261126/snug-virtual-display-0.4.0.5-x64-unsigned.zip`。ZIP SHA-256 为 `9f62aae7048c960fe5dea2620696fcaae166ec82aeb6d8b1ed3cf20c15a6a7cd`。

独立验收校验了 3 份回归源码记录，共 6 项哈希、4 个不同生产文件。构建目录没有完整的构建前源码 manifest；`build.rs`、`driver-ipc/src/core.rs` 和 `mock.rs` 另有验收时快照，不能当作构建前记录。

驱动回归使用实际生产函数与 native 故障注入；设置测试覆盖实际 egui 渲染和点击。这些结果不能证明安装后的显示拓扑、GPU 输出、睡眠唤醒和 UU 远控质量。此次未签名、安装或切换驱动，也未启动会自动建屏的应用实例。真机仍需使用签名包覆盖反复建撤屏、异常断连、重启恢复和长时资源观测。

默认安装入口仍内嵌上游 `0.3.1`，走 Legacy。要使用本轮 V2 归属协议，需要加载新驱动，并重新登记新版 helper；旧 V2 驱动不支持时会明确失败。将独立驱动包接入正式安装与升级流程，仍需相应的安装器和真机验收。
