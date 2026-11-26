# 生命周期审计修复记录（2026-11-26）

本轮处理审计确认的 8 项问题，范围包含应用、权限代理和仓库内驱动源码。保留工作区已有修改，未提交、推送、安装驱动或改变显示拓扑。

## 修复内容与文件

| 问题 | 修复后行为 | 主要文件 |
| --- | --- | --- |
| Arrival 或上下文初始化失败后缓存失效对象 | 删除未到达的对象、清空对应缓存，保留接受的模式意图；相同配置可重试 | `vendor/virtual-display-rs/virtual-display-driver/src/context.rs` |
| Departure 失败仍遗忘对象和配置 | 保留旧对象和旧配置；Remove、RemoveAll、Notify 和增量替换共用撤离处理，不创建第二个对象 | `vendor/virtual-display-rs/virtual-display-driver/src/ipc.rs` |
| 时序重复使用高度并以 u32 计算像素时钟 | 使用实际宽高和 u64 乘法；monitor 与 target 的时序保持一致；共享输入边界检查约束水平同步频率范围 | `vendor/virtual-display-rs/virtual-display-driver/src/callbacks.rs`、`vendor/virtual-display-rs/driver-ipc/src/core.rs` |
| 换模式及恢复均失败，原屏离线后会话仍无法恢复 | 还原已搬移窗口、清理旧绑定并保留待撤屏任务，有限退避后重建；退避耗尽仍可显式重新应用 | `crates/snug-core/src/desk/engine.rs` |
| GDI 名称被实体屏或其他虚拟屏复用后查询返回错误位置 | 查询复用已有绑定核验，必须匹配唯一驱动编号与缓存 GDI 名称，拒绝重复编号 | `crates/snug-win/src/display.rs` |
| V2 已接受创建但 Windows 上线超时后丢失归属 | 先保留内存归属并尝试落盘，再等待上线；超时或写账失败返回 pending 编号，撤屏失败继续保留归属 | `crates/snug-win/src/display.rs` |
| 已退出 helper 的第一次显式重连仍复用死进程 | 显式重连先检查进程存活状态，首次即可重启；普通写请求保持不自动重放 | `crates/snug-win/src/vdd_broker.rs` |
| 临时读取错误被当作 JSON 损坏，归属账本可能被覆盖 | 区分 I/O 错误和内容损坏；读取失败阻止隔离、读后改写和新建；隔离、写入及删除错误向 checked API 传播 | `crates/snug-core/src/store.rs`、`crates/snug-win/src/display.rs` |

驱动替换物理失败新增 `ApplyFailed` 回复，客户端在 `crates/snug-win/src/vdd.rs` 中保留连接并返回错误，供现有模式恢复流程处理。创建的 `Applied` 仅确认驱动已接受编号和配置，应用不会据此声称桌面已上线。

所有驱动修改共用修改锁，`Changed` 广播实际缓存快照。增量请求只重试本次目标编号，其他待上线屏幕不会使当前替换误报失败；旧全量 Notify 保留原有重试语义。物理批次可部分成功，具体语义见 [原子更新协议](../driver/atomic-update.md)。

## 简化与兼容

- 显示查询复用已有身份核验；驱动 Notify 与 SnugUpdate 复用同一输入校验；各撤屏入口复用同一失败保留逻辑。
- 生命周期和资源回归复用 `driver/tests/rust-source-extraction.ps1`，直接提取生产函数，避免复制修复实现。
- 未新增依赖。Legacy 超时后继续不认领、不删除未经确认的编号；已确认上线后才记录归属。
- 驱动版本在 `driver/source.lock.json` 和驱动 `build.rs` 中更新为 `0.4.0.4`。应用仍为 `1.0.7`，默认内嵌安装包仍为上游 v0.3.1。

## 验证结果

| 验证 | 结果 | 证据 |
| --- | --- | --- |
| `cargo test --workspace --all-targets --offline` | 742 通过，0 失败，1 跳过 | `target/lifecycle-workspace-debug-20261126.log` |
| 同上，增加 `--release` | 742 通过，0 失败，1 跳过 | `target/lifecycle-workspace-release-20261126.log` |
| 工作区 Clippy，correctness 和 suspicious 设为错误 | 通过，保留其他既有告警 | `target/lifecycle-clippy-20261126.log` |
| 应用 Release 构建 | 成功，EXE 版本 1.0.7 | `target/lifecycle-app-release-build-20261126.log`、`target/release/snug.exe` |
| 固定 nightly 的 driver-ipc 库测试 | 16 通过 | `target/driver-ipc-lifecycle-check-20261126.log` |
| 原子协议与边界测试 | 9 通过 | `target/driver-atomic-acceptance-20261126.log` |
| 驱动生命周期和时序测试 | Debug、Release 各 16 通过，以最终生产函数提取执行 | `target/driver-lifecycle-final-acceptance-20261126.log` |
| 光标与 swapchain 资源回归 | 11 通过 | `target/driver-resource-acceptance-20261126.log` |
| 独立存储与状态机补测 | 6 通过：隔离、销账、替换拒绝，以及 retiring、dismissed、wanted 边界 | `target/core-verifier-20261126/` |
| 驱动 x64 Release 构建 | 成功，无编译警告；DLL/INF 日期与版本一致 | `target/driver-lifecycle-build-final-20261126.log` |
| WDK 与 Rust ABI | 14 项布局、对齐和偏移一致 | `target/driver-lifecycle-build-20261126/abi/verified.txt` |
| Inf2Cat | 0 错误，0 警告 | `target/driver-lifecycle-build-final-20261126.log` |
| 最终产物独立核验 | 4 项清单哈希、5 个 ZIP 成员哈希、6 项生产源文件哈希一致，DLL 文件/产品版本均为 0.4.0.4 | `target/driver-lifecycle-build-20261126/artifact-verification.log` |

两种工作区测试跳过的是既有的管理员 staging ACL 测试，需要提升令牌；本次没有为运行该项而提权。固定 nightly 的 4 个驱动修改文件通过 rustfmt 检查，`git diff --check` 通过。整仓 `cargo fmt --all -- --check` 仍受包括未改动 `crates/snug-app/build.rs` 在内的既有排版差异阻断，未扩大为全仓格式化。

## 产物与剩余验证

最终驱动包为 `target/driver-lifecycle-build-20261126/snug-virtual-display-0.4.0.4-x64-unsigned.zip`。同目录有 SHA-256 清单、源码哈希、ABI 记录，`package/` 包含 DLL、INF、CAT 和许可证。

生命周期回归使用真实生产函数与 native 故障注入桩；资源测试包含真实进程内事件句柄。它们不能证明 Windows 上加载后的显示拓扑、GPU 行为或 UU 远控质量。此次未签名、安装或切换本机驱动，仍需签名包加载、反复建撤屏、睡眠唤醒、长时资源观测和 UU 联调。

持久归属格式继续兼容现有的编号账本；本轮没有引入跨进程的不可复用身份令牌。旧 Legacy 协议只确认提交的限制保留，未将其当作驱动已经完成操作。
