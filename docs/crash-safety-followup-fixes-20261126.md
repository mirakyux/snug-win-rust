# 崩溃安全与归属竞态补修（2026-11-26）

本文接续 `followup-audit-fixes-20261126.md` 末尾列出的风险。上一轮只记录了问题，没有完成代码补修；本轮继续修复并区分代码验证与环境验收。保留原有工作区修改，没有增加依赖、提交或推送。

## 跨屏转移的持久记录

跨屏转移不再先删来源、再建立目标记录。目标先完成已有窗口归还、残留账本恢复和窗口预检；来源记录在整个准备阶段持续存在。准备成功后，`RestoreRecordStore::try_transfer_to` 在同一目录内原子移交账本，成功之后才解除来源的内存归属。

Windows 使用 `MoveFileExW`，只设置 `WRITE_THROUGH`，没有 `REPLACE_EXISTING` 或跨卷复制降级。目标即使在预检后突然出现，也会拒绝覆盖并保留来源。目标接管后若窗口摆放失败，由目标继续恢复，不重新创建旧来源。Linux 使用 `renameat2(RENAME_NOREPLACE)`，本机未执行 Linux 分支；其他平台明确返回不支持。

回归覆盖旧实现中两份持久记录同时不存在的窗口、预检后出现目标、已有目标、跨目录和断开的 junction。保证范围是应用进程中断时不经过“先删除再保存”的空窗，不将其扩大为任意文件系统和机器掉电条件下的持久性证明。

## 字号中断的持久意图

新增独立的 `zoom-operation.json` 单槽执行意图，复用多屏共用串行队列，没有改变 C# 兼容的 `PhoneRecord` 格式。原生输入前先持久化 `Pending` 意图和权威账本的预留结果；已知输入结果先写入意图，再完成账本保存与意图删除。

- 完整输入计数为 `Applied`，确认未发送为 `NotApplied`，部分输入、已发送后失焦等结果为 `Uncertain`。
- 已知结果的保存或删除失败，只重试存储，不重复输入。
- 崩溃遗留的 `Pending`、部分发送、损坏或不可读的意图均阻止自动偿还和来源消费，不隔离意图后当作不存在。
- 不确定状态显示“先手动还原字号，再点击已手动还原字号”。确认只将完整匹配的权威来源记为零；来源不匹配时拒绝确认，删除意图失败可幂等重试。
- 通知集中保护，普通关闭通知或后续提示不会解除安全阻挡。

`JsonFileStore` 临时文件使用独占创建、完整写入、`sync_all`、关闭后改名；失败时清理临时文件。回归覆盖输入前意图持久化、意图损坏、部分输入、重启后确认、无关新债务、共享来源、锁文件保存失败和删除失败。

最后一轮集成复核另增加七项先红后绿的回归，补齐意图创建后的调用链：应用级归还循环遇到未决操作立即停止，同轮后续扫描不再消费来源；重新铺满、改变布局和显式摆放在读取或修改来源前阻断；旧记录移交刚产生未决操作时，本次摆放立即停止。应用级债务在输入明确未发送时继续保留，正常归还成功后才允许再次摆放。避免通过 `returned_as` 清零、改写几何或继续扫描而间接绕过恢复保护。

这些措施保留中断的不确定性，避免静默丢账或自动重复快捷键。`SendInput` 返回完整计数仅证明事件进入输入流，不证明应用已按预期消费；前台归属检查和全局输入注入也不是原子调用，不能宣称跨外部应用的 exactly-once 事务。

## V2 显示模式的归属写入

`SnugCheck` 不再授权随后按可复用的 GDI 名、adapter/source 标识进行写入。V2 创建和模式调整仅向已有驱动原子接口提交带 owner 的单目标模式；驱动在已有修改锁内校验 owner 并完成修改。应用等待归属及实测分辨率、刷新率匹配后才报告成功。

V2 不再执行原生临时模式、持久模式、DPI 或附加桌面回退。Windows 未自动启用输出或未采用唯一模式时明确失败，保留清理凭据。V2 保持 Windows 当前 DPI，布局按实际屏幕快照计算；设置界面说明可以在 Windows 显示设置中调整缩放。分辨率仍按固定值、显式应用处理。

Legacy 为兼容旧驱动保留原生路径，不宣称具有 V2 的 owner 原子保护。本轮复用此前已加入驱动的原子 replacement，没有新增驱动协议或生产驱动版本。新增生产源码提取回归覆盖查询通过后 connector 被新 owner 重建，旧 owner 的替换仍被拒绝且无副作用。

追加回归还复现了 Legacy 分流后、写入前出现 V2 owner token 的绕过。三处原生写入前检查统一改为 `verified_legacy_monitor`：再次读取到 token 就拒绝，仅允许无 token 的 Legacy 校验通过，不会因第二次归属查询成功而进入原生写入。

## 管理员 ACL 验收

普通 ACL 回归 Debug、Release 各四项通过、一项忽略。本报告形成时显式执行的 System32 管理员集成测试两次均因当时的中完整性令牌在提权断言处退出 101，未创建系统目录；这是该阶段受阻结果。后续经用户确认 UAC 的管理员复验已通过，见下方时间线及 `permissions-hardware-validation-20261126.md`。

## 普通 symlink 验收

本报告早期的普通令牌检查实际调用带 `ALLOW_UNPRIVILEGED_CREATE` 的 `CreateSymbolicLinkW`，文件、目录均返回 Win32 1314。新增真实 symlink 存储测试当时虽在 Debug、Release 编译，但夹具无法创建，因此未执行。后续管理员重验中，文件/目录创建及两种配置的存储回归均通过；首次受阻仍保留为环境快照，详情见下方时间线。

## 真实驱动硬件验收

本报告当时的只读检查确认系统安装的是 `0.4.0.1 / oem17.inf`；候选 `0.4.0.8` DLL 和 catalog 未签名、未加载，测试签名启动配置与 Secure Boot 查询受当时权限限制。这些是 0.4.0.8 阶段快照，不表示后续候选仍未验收。

后续实测（2026-11-26）：11:19:48–11:20:16（北京时间）的管理员 ACL/symlink 重验通过；13:14:07–13:15:19，签名候选 `0.4.0.12` 在本机通过四种 60 Hz 模式的受控 owner Smoke 与 CCD 复核，随后回滚至 `0.4.0.1 / oem17.inf`，未永久安装 v12。该轮不覆盖完整应用界面、手机端画面与触控、完整模式矩阵、长时间稳定性、Windows 重启、崩溃注入或设备端原始 EDID 回读；这些仍须另行验收。默认内嵌包仍是上游 `0.3.1`。完整证据见[权限与真实驱动验收记录](permissions-hardware-validation-20261126.md)的[管理员重验结果](permissions-hardware-validation-20261126.md#管理员重验结果)和[生产 v12 实机验收通过](permissions-hardware-validation-20261126.md#生产-v12-实机验收通过)。

## 验证证据

### 工作区集成

最终源码上执行 `cargo test --workspace --all-targets --offline`，分别覆盖 Debug、Release。两组结果相同：

| 测试目标 | 通过 | 忽略 |
| --- | ---: | ---: |
| snug-app | 128 | 0 |
| snug-core | 340 | 0 |
| snug-win | 419 | 1 |
| geometry_parity 集成测试 | 3 | 0 |
| sha256_cross 集成测试 | 1 | 0 |
| 合计 | 891 | 1 |

忽略项是这次 workspace Debug/Release 运行中的管理员专用 System32 ACL 测试；后续已单独以管理员令牌执行并通过，不能将该次忽略误读为 ACL 尚未通过。两个 profile 都设置真实驱动包路径和预期 SHA256，日志均包含实际交叉校验成功的输出，没有把缺少环境变量后的提前返回当作资产验证。

`cargo clippy --workspace --all-targets --offline -- -D clippy::correctness -D clippy::suspicious` 成功退出，覆盖工作区类型检查及所列静态检查；其余既有风格类警告仍在。`git diff --check` 和新增 `zoom_operation.rs` 的 `rustfmt --check` 均通过，没有整文件重排已有代码。独立只读复核没有发现新增高置信来源消费遗漏。

日志与结构化证据：

- `target/crash-safety-workspace-debug-final-20261126.log`
- `target/crash-safety-workspace-release-final-20261126.log`
- `target/crash-safety-workspace-clippy-final-20261126.log`
- `target/crash-safety-final-verification-20261126.json`
- `target/crash-safety-final-source-before-20261126.json`
- `target/crash-safety-final-source-after-20261126.json`

构建前后核对 111 项应用构建输入，长度及 SHA256 全部一致。最后七项回归、各自旧实现红灯日志和局部检查另见 `zoom-outcome-plan-20261126.md`。

### 发布构建

`cargo build -p snug-app --release --offline --target-dir target/owner-check-workspace-build-20261126-final` 成功退出。产物为 `target/owner-check-workspace-build-20261126-final/release/snug.exe`，文件及产品版本均为 `1.0.7`，大小为 9,572,352 字节，SHA256 为：

```text
b1116e480d7de0e757588144c4bc6be9e1122f333d0e8d09066030d4e96f1b80
```

构建日志为 `target/crash-safety-app-build-final-20261126.log`。没有启动应用、替换安装、提交或推送。

### 驱动与环境验收

生产驱动源码提取的生命周期测试在 Debug、Release 各通过 38 项，证据为 `target/owner-race-driver-20261126/`。候选包的 53 项生产构建输入均与当前源码匹配，4 项产物 checksum 和 5 项 ZIP 成员核验通过，包内 DLL 与编译 DLL 一致。未签名候选包 SHA256 为：

```text
64e3fc77670a275d01d22d16bbb2ca5f0ebc1048ad4aab5e3c5de410f0cb3298
```

完整历史范围从 75 项变为 77 项，差异是一份生命周期测试修改及两份权限测试新增，不能继续声称历史 75/75 输入断言或全部 40 项历史验收断言适用于当前完整范围。准确记录为 `target/owner-check-driver-build-20261126-final/owner-race-final-consistency.json`；生产候选与当前驱动生产源码仍一致。

权限脚本、原始退出码和硬件只读证据位于 `target/permission-acceptance-20261126/`。ACL 测试使用的生产模块 SHA256 也已与最终源码核对一致。本报告中管理员 ACL、普通 symlink 和新候选驱动真机验收记载的是该阶段受阻结果；后续显式管理员 ACL/symlink 复验和签名 0.4.0.12 的有限实机验收已通过，详见[权限与真实驱动验收记录](permissions-hardware-validation-20261126.md)的“管理员重验结果”及“生产 v12 实机验收通过”。这些后续结果不覆盖完整应用界面、全分辨率矩阵、长稳或重启/异常注入。

## 本轮相关文件与简化

- `crates/snug-core/src/store.rs`：严格意图读取、显式删除、临时文件同步及账本原子移交。
- `crates/snug-core/src/desk/zoom_operation.rs`：独立执行意图；`engine.rs` 接入输入、恢复与来源保护，`mod.rs`、`model.rs`、`services.rs`、`set.rs`、`test_support.rs` 配套导出、状态及回归。
- `crates/snug-win/src/window.rs`：精确输入结果；`display.rs`：V2 归属写入和 Legacy 分流复核。
- `crates/snug-app/src/controller.rs`、`mobile_library.rs`、`ui.rs`：手动恢复确认和当前 DPI 说明。
- `driver/tests/lifecycle-harness.template.rs`：旧 owner 在 connector 重建后的拒绝回归；`scripts/tests/run-permission-acceptance.ps1`、`scripts/tests/store-symlink-harness.template.rs`：真实权限验收入口。

复用现有串行队列、账本格式和驱动原子接口，移除 V2 的原生写入回退；没有增加依赖或另造跨屏事务框架。剩余边界为外部应用对输入流的实际消费、Legacy 原生路径的非原子归属，以及受权限、签名和硬件环境限制的真实验收。
