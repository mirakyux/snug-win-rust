# 权限与真实驱动验收记录（2026-11-26）

本文保留首次非提权检查的受阻证据，并追加管理员重验和新版候选的构建、签名结果。2026-11-26 的管理员权限验收已通过。v9 首轮实机验收暴露的空预留检查错误已修正。第二轮、第三轮及隔离 v10 实验均确认候选加载、清理和回滚成功，但 Windows 桌面源模式未采用请求尺寸。隔离 v11 首轮已观察到首次创建和同 ID 替换的桌面模式收敛，仍因 Release 后离场等待检查失败。修复验收程序后，v11 第二轮的受控 owner 范围 Smoke、清理、回滚和精确桌面核验均通过；替换后的 CCD 目标信号仍未采到，应用启动和任意模式不在本轮通过范围。v10、v11 候选的行为改动均属于 `target` 隔离实验。随后生产 v12 在两组横竖屏 Smoke 中完成四种请求模式的源尺寸、CCD 目标信号、owner 安全检查、清理及回滚验收；本轮结束已恢复原驱动，未永久安装 v12，应用界面和完整模式矩阵仍未覆盖。

## 首次验证计划

1. 确认当前进程的完整性级别、管理员组状态和符号链接权限。
2. 使用现有生产权限描述符，补测真实中完整性令牌的访问检查；单独记录需要提权的 System32 集成测试，失败或跳过不计作通过。
3. 在独立临时目录尝试普通文件和目录符号链接。具备权限时验证断链仍返回读取错误，禁止覆盖未知恢复来源；不修改开发者模式或本地安全策略。
4. 只读检查当前 PnP 设备、已安装驱动版本、签名及候选包。区分已加载旧驱动与本轮候选，不使用旧驱动结果替代新源码验收。
5. 记录实际命令、退出码和覆盖范围。所有测试使用独立 target，不安装驱动、不调整真实显示拓扑、不触发 UAC。

## 初始环境

首次检查的进程为中完整性级别。管理员组仅用于拒绝，令牌不含 `SeCreateSymbolicLinkPrivilege`。没有更改系统权限或启动配置。

PnP 初查显示已安装 `Virtual Display` 为 `0.4.0.1`，INF 为 `oem17.inf`；这不是上一轮构建的 `0.4.0.8` 候选。签名与测试加载条件另行核对。

## 首次 ACL 验证结果

新增 `scripts/tests/run-permission-acceptance.ps1`，直接导入生产 `system_paths.rs`，并通过真实 `IsUserAnAdmin` 查询执行测试的令牌。没有用固定返回值绕过管理员检查。

Debug、Release 的普通 ACL 组均为 **4 项通过、1 项忽略**。通过的范围包括生产 DACL 的访问检查、已有子项的权限、原子目录创建拒绝已有内容，以及系统工具路径校验。它们不替代真实 System32 管理员集成测试。

脚本又分别显式运行 `elevated_atomic_staging_inherits_production_acl`。两次均在首次提权断言处退出 101，未创建 System32 测试目录。当前令牌为中完整性级别，管理员组为仅拒绝，故记录为 `blocked-by-medium-token`，不能记作通过。

## 首次普通 symlink 验证结果

脚本实际调用 `CreateSymbolicLinkW`，文件使用标志 2，目录使用标志 3，即均包含 `SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE`。两种创建均返回 Win32 错误 **1314**（客户端不具备所需权限）。当前令牌未启用符号链接特权，开发者模式注册项不存在。

新增的 `store-symlink-harness.template.rs` 调用真实 `snug-core` store 公共 API，覆盖文件、目录普通符号链接断开后的错误返回、恢复记录读取、链接保留及删除链接后的正常保存。Debug、Release 编译均成功；本次因无法创建测试夹具，**未执行这两项普通 symlink 测试**。没有把 junction 验证或 `cargo test --no-run` 计为普通 symlink 通过。

完整入口如下；无需安装依赖或修改项目工作区。权限不足时脚本退出 **77**，与成功的 0 区分。

```powershell
.\scripts\tests\run-permission-acceptance.ps1
```

## 管理员重验结果

用户确认 UAC 后，第二轮管理员验收于 2026-11-26 11:19:48～11:20:16（北京时间）执行。`admin-run-2/admin-results.json` 记录 `elevated: true`、`status: passed`、`permissionExitCode: 0`，38 项输入校验通过，错误列表为空。

| 验收项 | Debug | Release |
| --- | --- | --- |
| 普通 ACL 组 | 4 项通过、1 项忽略 | 4 项通过、1 项忽略 |
| 显式 System32 ACL 集成测试 | 1 项通过 | 1 项通过 |
| 普通文件、目录 symlink 存储回归 | 2 项通过 | 2 项通过 |

普通 ACL 组忽略的管理员用例已在同一轮显式执行，两个 profile 均通过 `elevated_atomic_staging_inherits_production_acl`，退出码为 0。这次真实管理员执行补齐首次中完整性令牌受阻的覆盖，没有把普通 ACL 测试替代为 System32 集成测试。

文件、目录的 `CreateSymbolicLinkW` 创建均成功。含 `ALLOW_UNPRIVILEGED_CREATE` 的标志 2、3，以及普通标志 0、1，Win32 错误码均为 0。真实存储回归分别执行 `broken_file_symlink_preserves_unknown_restore_source` 和 `broken_directory_symlink_is_not_an_absent_source`；Debug、Release 均为 2 项通过、0 项失败。首次管理员尝试中的夹具路径冲突已修复，夹具与能力探针使用独立路径。

`permissions-elevated-2/verified-results.json` 的总状态和 `fixtureCleanup` 均为 `passed`。测试记录的生产 `system_paths.rs`、`store.rs` 哈希与当前文件一致；没有把首次 1314 受阻、编译通过或 junction 测试计入本次普通 symlink 通过结果。

管理员包装器另行成功导出旧驱动的 DLL、CAT、INF，并记录逐项 SHA-256。该轮观察到的加载模块文件仍为 `0.4.0.1`；显示快照前后命令均退出 0。启动配置读取成功，Secure Boot 查询为 `true`。这些结果属于该次权限重验时的观察，不能证明候选驱动已经加载。

本轮证据位于 `target/acceptance-complete-20261126/`：

- `admin-run-2/admin-results.json`：管理员令牌、包装器结果、38 项输入校验和旧驱动导出记录。
- `permissions-elevated-2/verified-results.json`：编译与实际运行退出码、symlink 创建结果、生产源码哈希和夹具清理状态。
- `permissions-elevated-2/acl-*.log`、`acl-elevated-*.log`、`symlink-*.log`：Debug、Release 原始输出。

## 首次真实驱动检查

2026-11-26 的只读检查结果如下。

| 对象 | 观察结果 | 结论范围 |
| --- | --- | --- |
| `ROOT\DISPLAY\0001` | PnP 状态 OK；`oem17.inf`；版本 `0.4.0.1` | 已安装设备状态，不证明新候选可用 |
| 系统 UMDF DLL | `0.4.0.1`；Authenticode 为 Valid；签名者为 `Snug Virtual Display 0.4.0.1 Test` | 系统现有文件有测试签名 |
| 候选 DLL | `0.4.0.8`；Authenticode 为 NotSigned | 不是当前系统 DLL，未完成签名或安装验收 |
| 候选 catalog | Authenticode 为 NotSigned | 不能用现有旧驱动签名替代候选签名 |
| 启动配置 | `bcdedit /enum` 退出 1，Access is denied | 测试签名配置未知 |
| Secure Boot | 读取权限被拒绝 | 当前状态未知 |

CIM 的 `IsSigned=False` 与系统 DLL 的 Authenticode Valid 来自不同检查层面，分别保留在证据中；不据此推断驱动包已通过生产签名要求。

候选 INF 为 `DriverVer = 11/26/2026,0.4.0.8`。候选 DLL SHA-256 为 `F95BE233F59E77ED9DBEBCB6733CD96BF0ED57CBA7796CB6585C76151D798BE3`；系统旧 DLL SHA-256 为 `00E6F27AA934DCAFC25FC4AF2735E59B966A7B0C169AFD55670E40CBF8A4FFFF`。本次读取了磁盘文件和 PnP 元数据，没有证明候选已加载。

首次检查时，真实驱动验收受权限和候选签名条件限制，尚未执行。后续进展见下一节；不能用代码构建或旧 `0.4.0.1` 的状态冒充新候选的真机结果。

## 0.4.0.9 候选构建与签名

后续复核发现无凭据的旧命令可以绕过归属保护，已在 `0.4.0.9` 修复。因此，历史 `0.4.0.8` 构建和签名包均不能作为本轮最终候选。

新版构建位于 `target/legacy-owner-driver-build-20261126-v9/`。独立静态验收 40 项通过，14 项 ABI 对照一致，Inf2Cat 无错误、无警告。53 项构建输入经逐项长度和 SHA-256 复查，与当前生产源码一致。当前生产源码提取的生命周期测试 Debug、Release 各 45 项通过，日志另存于 `target/driver-review-current-20261126/`。这些检查未接入真实驱动或改变桌面。

签名结果位于 `target/acceptance-complete-20261126/signed-candidate-v9/result.json`，完成于 2026-11-26 11:20:24（北京时间），状态为 `signed-and-verified`。使用原有测试证书及现存私钥，证书指纹为 `BCCCC3AE6F561ED813B9BAE387CB50C5A5890B31`。DLL 和 CAT 的 Authenticode 均为 `Valid`，签名及 CAT 成员校验命令均退出 0。证书信任存储和安全策略未修改，临时导入的个人存储证书已删除，未签名源包保持不变。

| 产物 | SHA-256 |
| --- | --- |
| 签名后 `VirtualDisplayDriver.dll` | `A8A46230FAECB1CD295060EE1099A85343B59A8CC35D485388789D8095BF4965` |
| `snug-virtual-display-0.4.0.9-x64-signed.zip` | `1A215D1A0C87363D4382FFD2786C1E10FA5EDE82B024B99AE64F431A70D7A68E` |

签名脚本的 `installed: false` 仅说明该签名步骤没有安装驱动。签名通过不等于生产发行签名合规，也不证明驱动已加载、模式已收敛或撤屏已成功。后续真实安装、首次 Smoke 失败和回滚结果见下文；不从签名结果推导功能验收通过。

## 首轮 v9 实机验收与测试程序修正

首轮受控安装于 2026-11-26 11:26:41～11:27:13（北京时间）执行。Windows 成功暂存并绑定 `0.4.0.9`，OEM INF 为 `oem68.inf`，设备状态 OK、问题代码 0。观察到的 WUDFHost PID 为 317500；默认驱动管道的服务进程 PID、启动时间与该模块匹配。加载模块对应文件的版本为 `0.4.0.9`、签名 Valid，SHA-256 与签名后候选 DLL 一致。

只读预检通过，Smoke 未通过。原始错误为“虚拟屏预留凭据不存在”，发生在 Reserve 成功后、Create 发送前。验收程序把仅适用于已接受 monitor 的 `SnugCheck` 用于空预留，并错误要求 Applied。生产 `snug_check` 按约定对空预留返回 NotFound；这不是 Reserve 被拒绝，也不是候选驱动加载失败。

已修正 `scripts/tests/driver-hardware-acceptance.template.rs`：Reserve 后要求 SnugCheck 返回 NotFound，Create 后再确认所有权。生产驱动无需修改、重建或重签。新增生命周期回归覆盖新鲜预留的 NotFound、创建后的 Applied，以及两次查询前后完整状态不变；Debug、Release 各 46 项通过，证据位于 `target/reservation-contract-fresh-20261126/`。

首轮失败后，仅使用本轮 owner 清理，退出码为 0。随后恢复原 `0.4.0.1`、`oem17.inf`，核对旧模块文件哈希及实际管道服务进程，回滚通过。只读桌面核验确认原始空 State、两块物理屏幕的身份、模式、位置、刷新率、DPI、主屏和工作区均与基线一致，未调用桌面恢复写入。首轮结果位于 `target/acceptance-complete-20261126/hardware-install-v9/result.json`，保留 `status: failed`；不得将清理和回滚成功写成 Smoke 成功。

## 修正后的重验准备与提权中止

修正后的测试程序位于 `target/acceptance-complete-20261126/hardware-v9-r2/`。Prepare 成功，纯逻辑测试 10 项通过；另 1 项真实管道探针按默认规则忽略，不计作通过。50 项 PowerShell 包装器回归通过。新的受控包装器及清单锁定 128 项输入，候选仍使用同一个签名后 v9 驱动，不修改安全策略。

2026-11-26 11:32:05（北京时间），第二轮首次提权尝试返回“操作已被用户取消”。该次管理员进程未启动，没有安装、Smoke 或清理操作。提权中止另存为 `hardware-v9-r2-elevation-cancelled.json`；`hardware-v9-r2-launch.json` 的 `processId: null` 不表示成功启动。用户随后授权重新弹出 UAC，实际执行结果见下一节。

提权取消后、再次执行前，只读核验确认原驱动及桌面正常。该时点的 `final-readonly-state.json` 记录 `0.4.0.1`、`oem17.inf`、问题代码 0，桌面核验退出码 0。该文件早于下述第二轮实机执行，不能替代后续回滚证据。修正后的 `run-hardware-v9-r2.ps1`、`hardware-v9-r2-inputs.json` 及新基线均已保留。

## 第二轮 v9 实机验收结果

第二轮于 2026-11-26 11:35:23～11:36:10（北京时间）实际执行，128 项输入校验通过。暂存、安装及只读预检均退出 0。候选设备为 `0.4.0.9`、`oem68.inf`，状态 OK、问题代码 0；加载模块和管道服务进程的核验通过。

修正后的空预留检查符合协议，Create 也成功。但 `created-mode-converged` 在 15 秒后超时。驱动 State 中的虚拟屏 ID 为 15，唯一模式为 `1280×720@60`；实际 Windows 桌面持续报告 `1080×1920@60`、DPI 150%。两块物理屏幕保持原有状态。该轮未执行原生显示写入，不能将驱动模式表接受成功记为桌面模式收敛成功。

Smoke 在首次创建模式收敛处失败，后续旧协议绕过探针、错误 owner 拒绝及 `1600×900@60` 替换模式验证均未到达，不计作通过。`smoke/result.json` 保留 `passed: false` 和原始错误 `created-mode-converged: desktop did not converge in 15s`。

失败后的 owner 范围清理通过，额外清理命令退出 0。回滚命令退出 0，并确认恢复 `0.4.0.1`、`oem17.inf`，设备状态 OK、问题代码 0。旧模块和实际管道服务进程的核验通过。最终精确桌面核验退出 0，原始空 State 和两块物理屏幕均与基线一致，未调用桌面恢复写入。

本轮总结果为 `status: failed`、`hardwarePassed: false`、`rollbackPassed: true`、`desktopPassed: true`。模式未收敛的根因尚未确定，不能仅凭桌面宽高推断 EDID、旋转或缩放机制。证据位于 `target/acceptance-complete-20261126/`：

- `hardware-install-v9-r2/result.json`：实际执行时间、候选身份、各命令退出码和回滚结果。
- `hardware-install-v9-r2/inventory-candidate.json`、`inventory-restored.json`：候选与原驱动的加载模块及管道服务进程。
- `hardware-v9-r2/smoke/result.json`、`events.jsonl`：超时错误、驱动 State、实际桌面模式及清理快照。

## 第三轮 CCD 诊断结果

第三轮诊断包装器 `run-hardware-v9-r3.ps1` 及只读 CCD 观察器已准备完成，输入清单锁定 129 项文件。观察器用于区分桌面源模式、目标信号、旋转和缩放，不执行显示写入。

2026-11-26 约 11:47（北京时间），第三轮首次提权尝试返回“操作已被用户取消”，该次管理员脚本未启动。用户随后再次授权，第三轮于 11:49:13～11:49:59 实际执行，129 项输入校验通过。候选仍为同一个签名后 `0.4.0.9` 驱动，设备状态 OK、问题代码 0。

只读 CCD 观察器正常退出，`readOnly: true`，共记录 21 次采集。虚拟屏 ID 15 对应的路径显示：桌面源模式为 `1080×1920`，目标信号的有效尺寸已为 `1280×720@60`，`rotation: 1`（identity）、`scaling: 1`。这说明请求的目标信号与桌面源尺寸分离；当前路径并非通过 CCD 旋转得到竖屏。该证据不能证明固定 EDID 首选模式或历史缓存是根因，也不能用目标信号正确替代桌面源模式收敛。

本轮 Smoke 再次在首次创建模式收敛处失败。owner 范围清理和回滚命令均退出 0，恢复的设备为 `0.4.0.1`、`oem17.inf`，状态 OK、问题代码 0。最终只读精确桌面核验退出 0，确认原始空 State 和两块物理屏幕与基线一致。总结果保留 `status: failed`、`hardwarePassed: false`、`rollbackPassed: true`、`desktopPassed: true`。

第三轮证据位于 `target/acceptance-complete-20261126/`：

- `hardware-install-v9-r3/result.json`：执行时间、129 项输入校验、观察器退出码、清理与回滚状态。
- `hardware-install-v9-r3/ccd-observer/result.json`、`index.jsonl`：只读观察器结果和 21 次采集索引。
- `hardware-install-v9-r3/ccd-observer/ccd-0003.json` 等快照：源模式、目标信号、旋转、缩放和虚拟屏映射。
- `hardware-v9-r2/smoke-r3/result.json`、`events.jsonl`：本轮 Smoke 原始错误及清理快照。
- `hardware-install-v9-r3/desktop-final.stdout.log`：回滚后的精确桌面核验结果。

## v10 最小模式标志隔离实验结果

v10 基于 v9 的 53 项构建输入，在 `target/acceptance-complete-20261126/smallest-mode-v10/source-root/` 中制作隔离候选 `0.4.0.10`。除版本资源和构建身份外，唯一行为变化是在适配器能力中设置 `IDDCX_ADAPTER_FLAGS_USE_SMALLEST_MODE`。没有修改动态 EDID、模式回调时序或添加原生显示写入；该标志未合入生产源码。构建准备时记录的 `installed: false` 属于安装前快照，不能替代本节实机记录。

实机实验于 2026-11-26 11:59:37～12:00:24（北京时间）执行，223 项输入校验通过。暂存、安装及只读预检均退出 0。实际候选为 `0.4.0.10`、`oem70.inf`，设备状态 OK、问题代码 0。加载模块对应文件签名为 Valid，SHA-256 为 `6B92070621C938A7AA045161CFEB6364EF6C42708854D0DF6BED370CE1D6A42C`，与签名后候选一致；管道服务进程与加载模块所在的 WUDFHost PID、启动时间匹配。文件哈希不是进程内存哈希。

只读 CCD 观察器退出 0，记录 21 次采集。实际快照 `ccd-0003.json` 中，虚拟屏 ID 15 对应 `CHY0000`、UID 271、`ROOT\DISPLAY\0001`：桌面源模式仍为 `1080×1920`，位置为 `(2560, -1920)`；目标信号有效尺寸和总尺寸均为 `1280×720`，刷新率为 `60/1`，`rotation: 1`（identity）、`scaling: 1`。驱动 State 的唯一模式为 `1280×720@60`，桌面显示 DPI 为 150%。该实验没有使源尺寸收敛，不能据此认定 EDID 或缓存为根因，也不能把目标尺寸正确记为验收通过。

Smoke 再次在 `created-mode-converged` 等待 15 秒后失败，原始错误为 `created-mode-converged: desktop did not converge in 15s`。结果同时记录 `applicationStarted: false`、`noNativeDisplayWrites: true`、`cleanupPassed: true`。后续旧协议绕过、错误 owner 拒绝和 `1600×900@60` 替换模式验证仍未到达，不计作通过。

owner 范围额外清理和回滚命令均退出 0。最终恢复 `0.4.0.1`、`oem17.inf`，状态 OK、问题代码 0；旧模块文件哈希和实际管道服务进程核验通过。最终只读精确桌面核验退出 0，原始空 State 和两块物理屏幕均与基线一致，未执行桌面恢复写入。总结果保留 `status: failed`、`hardwarePassed: false`、`rollbackPassed: true`、`desktopPassed: true`。v10 是失败的隔离诊断实验，不作为生产修改或功能通过证据。

本轮证据位于 `target/acceptance-complete-20261126/`：

- `smallest-mode-v10/experiment.json`、`experimental-only.patch`、`delivery-verification.json`：隔离范围、单一行为改动、构建与签名校验，以及安装前状态。
- `hardware-install-v10-r1/result.json`：实际执行时间、223 项输入校验、候选身份、清理、回滚及最终桌面核验退出码。
- `hardware-install-v10-r1/inventory-candidate.json`、`inventory-restored.json`：候选与原驱动的加载模块文件和管道服务进程核验。
- `hardware-install-v10-r1/ccd-observer/result.json`、`ccd-0003.json` 等快照：21 次只读采集及实际源模式、目标信号、旋转和缩放。
- `hardware-v9-r2/smoke-v10-r1/result.json`、`events.jsonl`：首次创建收敛超时、未启动应用及清理结果。
- `hardware-install-v10-r1/desktop-final.stdout.log`：回滚后的精确桌面核验结果。

## v11 动态 EDID 隔离实验结果

v11 基于生产 v9 输入，在 `target/acceptance-complete-20261126/dynamic-edid-v11/` 中制作隔离候选 `0.4.0.11`，不包含 v10 的 `USE_SMALLEST_MODE` 标志。该实验仅为单一 `1280×720@60` 或 `1600×900@60` 模式生成相应首选 DTD，保持 EDID 的厂商、产品及序列身份不变；不支持的模式组合明确拒绝。它不是任意分辨率支持的验收，也未合入生产源码。

实机实验于 2026-11-26 12:10:36～12:11:14（北京时间）执行，292 项输入校验通过。暂存、安装及只读预检均退出 0。候选为 `0.4.0.11`、`oem71.inf`，设备状态 OK、问题代码 0；实际管道服务进程与加载模块所在的 WUDFHost PID、启动时间匹配。加载模块对应文件签名为 Valid，SHA-256 为 `A12012ED49E28208654CE7B5C3917EC89D684B39D85371836D70A83277FE4262`，与签名后候选一致；该哈希仍是文件哈希，不是进程内存哈希。

首次创建 ID 15 后，驱动 State 为唯一 `1280×720@60`，Windows 桌面连续两帧为 `1280×720@60`、DPI 100%，位置 `(3840, 0)`。只读 CCD 观察器退出 0，共记录 13 次采集；`ccd-0003.json`～`ccd-0007.json` 的 `CHY0000`、UID 271 路径同时显示源尺寸和目标有效尺寸均为 `1280×720`，`vSync: 60/1`、`rotation: 1`、`scaling: 1`。因此，本轮首次创建的桌面源尺寸和目标信号均已观察到请求值。

六种无凭据旧协议变更探针均未改变桌面或驱动 State，并保留 owner：Notify 替换、遗漏、相同内容，Remove、RemoveAll，以及不带凭据的 SnugUpdate。错误 owner 的 Check、Replace、Release 均被拒绝，后两者的前后快照无变化。随后同 ID Replace 为 `1600×900@60`：驱动 State 与 Windows 桌面连续两帧均为请求尺寸和刷新率，桌面 DPI 100%；适配器 LUID、source ID、stable ID 和设备名保持一致。该轮 CCD 采样未捕获替换后的目标信号，不能将桌面和 State 的替换成功扩写为 CCD 目标信号已经验证。

Smoke 的 `exerciseError` 为 `null`，但离场阶段未通过。Release 回复 `Confirmed` 后，驱动 State 已为空；首帧桌面仍包含原自有屏幕，其适配器、source ID、stable ID 和设备名保持一致，但 `virtualId` 暂时变为 `null`，友好名回退为设备名。测试程序将该帧判为已有或未知屏幕变化，记录 `cleanupError: an existing or unexpected desktop screen changed; see snapshots`、`cleanupPassed: false`。这说明离场等待路径尚需修正；不能因前序模式和安全探针通过而忽略该失败。

后续额外 owner 范围 Cleanup 回复 `NotOwned`，并观察到连续两帧仅剩原两块物理屏幕、驱动 State 为空，记录清理通过。包装器的额外清理和回滚命令均退出 0，最终恢复 `0.4.0.1`、`oem17.inf`，设备状态 OK、问题代码 0，旧模块文件哈希和实际管道服务进程核验通过。最终只读精确桌面核验退出 0，确认原始空 State 及两块物理屏幕与基线一致；没有执行桌面恢复写入。

总结果仍为 `status: failed`、`hardwarePassed: false`、`rollbackPassed: true`、`desktopPassed: true`。Smoke 另记 `applicationStarted: false`、`noNativeDisplayWrites: true`；应用启动不在本轮已验证范围。后续额外清理成功不覆盖 Smoke 原始 `cleanupPassed: false`，也不构成完整功能验收通过。该实验与历史结果的差异支持继续验证动态 EDID 方案，但单轮结果不足以证明既有源模式问题的唯一根因。

本轮证据位于 `target/acceptance-complete-20261126/`：

- `dynamic-edid-v11/delivery-verification.json`、`handoff-seal.json`：隔离候选、签名及冻结输入校验。
- `hardware-install-v11-r1/result.json`：实际执行时间、292 项输入校验、候选身份、清理、回滚和最终桌面核验退出码。
- `hardware-install-v11-r1/inventory-candidate.json`、`inventory-restored.json`：候选和旧驱动的加载模块文件及实际管道服务进程。
- `hardware-install-v11-r1/ccd-observer/result.json`、`ccd-0003.json`～`ccd-0007.json`：13 次只读采集及首次创建的源模式、目标信号；未覆盖替换后的目标信号。
- `hardware-v9-r2/smoke-v11-r1/result.json`、`events.jsonl`：创建与替换的连续桌面快照、安全探针、Release 首帧失败及后续额外清理。
- `hardware-install-v11-r1/desktop-final.stdout.log`：回滚后的精确桌面核验结果。

## v11 第二轮实机验收与离场等待修复

第一轮失败后，仅修正验收程序的 Release 等待：在所有权仍可验证时，冻结自有屏幕的适配器 LUID、source ID、stable ID 和设备名，并把该身份连同 owner、ID、原始基线持久化，供单独 Cleanup 重试使用。Release 后仅允许该已绑定身份暂时残留；每帧仍严格校验所有原始屏幕和无关驱动 State。临时屏幕必须真正消失，且完整原始快照连续两帧完全一致，才能通过；不会因 `virtualId: null` 提前通过。未创建屏幕的预留不获得任何额外屏幕豁免，身份绑定或记录失败也不阻止既已授权的 owner 范围清理。

修复前保留了失败回归，修复后的 Prepare 为 **19 项通过、1 项忽略**；忽略的真实管道探针没有计为通过。PowerShell 包装器为 **50 项回归通过**，Clippy 的 `-D warnings` 与格式检查通过。另从最终冻结模板直接抽取纯判断函数，使用第一轮真实事件进行独立 Debug、Release 离线回放：原始 null-ID 自有残影、消失首帧、消失第二帧依次判为 `[false, false, true]`。持续 32 帧残影不通过；物理屏字段篡改、陌生或重分配的原生身份、无关驱动变化、无身份预留和无所有权的新绑定均拒绝。该回放没有连接驱动或写入桌面。

第二轮于 2026-11-26 12:29:14～12:29:53（北京时间）执行，304 项输入校验通过。使用的驱动仍是第一轮同一份签名后 `0.4.0.11` 候选，DLL SHA-256 为 `A12012ED49E28208654CE7B5C3917EC89D684B39D85371836D70A83277FE4262`，没有以修改测试程序为由重建或替换驱动字节。候选绑定为 `oem71.inf`，状态 OK、问题代码 0；加载模块文件版本、Valid 签名、文件哈希及实际管道服务进程核验通过。

本轮首次创建 `1280×720@60`、同 ID 替换 `1600×900@60` 的驱动 State 和 Windows 桌面均连续两帧符合请求，桌面 DPI 100%。六种无凭据旧协议变更探针保持无效，owner 保留；错误 owner 的 Check、Replace、Release 均拒绝。只读 CCD 观察器退出 0，共 48 次采集；`ccd-0014.json`～`ccd-0033.json` 共 20 帧的虚拟路径均为源 `1280×720`、位置 `(3840, 0)`，目标有效尺寸和总尺寸 `1280×720`、`vSync: 60/1`、`rotation: 1`、`scaling: 1`。该轮仍未捕获替换后的目标信号，因此不能将 `1600×900` 的桌面及 State 收敛写成 CCD 目标信号通过；配置 200 毫秒间隔也不表示每 200 毫秒都有有效快照。

实际 Release 前成功绑定自有原生身份；Release 回复 `Confirmed` 后，首帧确实出现驱动 State 为空、同一身份的桌面屏幕 `virtualId: null` 的过渡状态。修复后的程序继续等待，随后连续两帧只剩精确原始屏幕，Smoke 记录 `passed: true`、`cleanupPassed: true`、`cleanupError: null`、`exerciseError: null`。随后额外 Cleanup 使用持久化的同一身份，回复 `NotOwned`，完整原始状态连续两帧核验通过。这里的实机事件验证了本次离场修复所针对的过渡，而非仅依赖离线测试。

本轮安装、只读预检、Smoke、额外清理、回滚及最终桌面核验命令均退出 0。最终恢复 `0.4.0.1`、`oem17.inf`，状态 OK、问题代码 0；旧模块文件哈希及实际管道服务进程核验通过，精确原始 State 和桌面核验输出为 `original State and exact desktop verified, no write`。总结果为 `status: passed`，`hardwarePassed`、`rollbackPassed`、`desktopPassed` 均为 `true`，错误列表为空。

通过范围为此隔离候选在本机的两种固定模式、owner 保护及受控创建、替换、撤屏和回滚流程；第一轮失败记录不被覆盖。Smoke 仍记录 `applicationStarted: false`、`noNativeDisplayWrites: true`，不能扩写为应用完整启动、任意分辨率或多模式的实机验收，也不能用该原型的通过代替后续生产候选的独立验收。

本轮证据位于 `target/acceptance-complete-20261126/`：

- `release-guard-repair/repair-plan.md`、`red.log`、`prepare.log`、`powershell-tests.log`、`clippy.log`、`format.log`：修复计划、失败回归及修复后的逻辑、包装器和静态检查。
- `release-guard-independent-replay-final/source-record.json`、`result.json`、`debug-run.log`、`release-run.log`：最终冻结模板与真实事件哈希、独立纯逻辑回放及其边界反例。
- `hardware-v11-r2-release-guard/prepared.json`、`prepare-test.log`：与实际执行 EXE 绑定的输入和 19 项通过、1 项忽略记录。
- `hardware-install-v11-r2/result.json`、`inventory-candidate.json`、`inventory-restored.json`：304 项输入、同一候选字节、加载模块及实际回滚结果。
- `hardware-v11-r2-release-guard/smoke-v11-r2/result.json`、`events.jsonl`、`release-identity.json`：两种桌面模式、安全探针、已绑定残影的真实离场等待和持久化重试。
- `hardware-install-v11-r2/ccd-observer/result.json`：48 次只读采集；仍缺替换后的目标信号证据。
- `hardware-install-v11-r2/desktop-final.stdout.log`：回滚后的精确桌面核验结果。

## 生产 v12 准备与提权受阻

生产候选 `0.4.0.12` 已从冻结的 53 项源码输入构建；相对生产基线恰有 4 项变更：`edid.rs`、`context.rs`、驱动 `build.rs` 与 `driver/source.lock.json`。首 mode、首 refresh rate 在可表示时生成受范围及量化误差检查的首选 DTD，仅调整 DTD、必要的最大像素时钟上限和校验和；不可表示时返回同 serial 的原始 EDID。原多模式、多刷新率 IPC 校验及模式回调保持不变，不将 v11 的两模式隔离实验白名单带入生产。

离线回归 Debug、Release 分别通过 lifecycle 56 项、atomic 11 项、input 9 项、init 6 项、cursor 9 项、swapchain 3 项。lifecycle 保留原 46 项并新增 10 项 EDID 与真实抽取调用路径回归；独立复跑两个配置均为 56 项通过。EDID 模块 Clippy 严格警告检查与限定文件格式检查通过；构建退出 0，40 项产物验收和 14 项 swapchain ABI 检查通过。候选使用已有证书签名，DLL、CAT 签名均为 `Valid`，签名前后信任存储快照一致。构建、签名及离线测试均不能替代实机验收。

两组 standard、phone 验收资产完成 Prepare 和 EXE／源码绑定校验，计划在同次安装中分别观察横屏及手机竖屏的创建、替换。最终执行封存记录为 503 项输入、漂移 0、脚本解析错误 0。2026-11-26 12:52:21（北京时间）首次发起提权，随后返回“操作已被用户取消”；`hardware-v12-r1-launch.json` 为 `status: elevation-failed`、`processId: null`，`hardware-install-v12-r1` 未创建，管理员验收脚本没有开始。取消后的只读复核确认原驱动仍为 `0.4.0.1`、`oem17.inf`，状态 OK、问题代码 0，精确桌面核验退出 0。因此本轮没有生产 v12 的安装、Smoke、CCD、清理或回滚通过结果；后续重新提权须单独记录实际执行证据。

经用户授权，retry1 于 2026-11-26 13:03:42.234（北京时间，`05:03:42.234Z`）请求提权，13:05:44.776（`05:05:44.776Z`）再次返回“操作已被用户取消”，PID 仍为 `null`。用户随后明确表示未看到 UAC；这里仅记录系统返回文案，不能据此认定用户主动取消。用户表示已按管理员方式重启后，当前只读逐级 token 探针仍显示 `pwsh.exe → codex.exe → ChatGPT.exe → ChatGPT.exe → explorer.exe` 均为 standard，尚不能视为已进入管理员上下文。

2026-11-26 13:11:55.2156516（北京时间，`05:11:55.2156516Z`）的重启后只读复核再次确认：503 项冻结输入的哈希全部匹配、漂移 0、`administrator: false`、`runDirectoryExists: false`。精确桌面核验退出 0，辅助程序输出 `original State and exact desktop verified, no write`。截至该次复核，生产 v12 实机验收尚未开始。

本轮准备及受阻证据位于 `target/acceptance-complete-20261126/`：

- `production-edid-v12/` 下的 `production-source-manifest.json`、`changes-manifest.json`、`build-invocation.json`、`verification-summary.json`：53 项生产输入、4 项变更、构建及离线测试范围。
- `production-edid-v12/build/` 下的 `independent-artifact-acceptance.json`、`abi/verified.txt`：40 项产物与 14 项 ABI 检查。
- `production-edid-v12/signed/` 下的 `authenticode.json`、`trust-before.json`、`trust-after.json`，以及 `production-edid-v12/handoff-seal.json`：已有证书签名、信任边界与尚未实机验收的候选封存。
- `v12-hardware-assets/asset-tests.json`、`v12-hardware-assets/prepared-verification.json`、`hardware-v12-r1-finalize.json`、`hardware-v12-r1-inputs.json`：验收资产、Prepare 绑定及 503 项执行输入。
- `hardware-v12-r1-launch.json`、`hardware-v12-r1-elevation-cancelled.json`：首次提权取消且管理员进程未启动。
- `hardware-v12-r1-post-cancel-readonly.json`：取消后原驱动身份、设备状态及精确桌面的只读复核。
- `hardware-v12-r1-retry1-launch.json`：授权重试的请求时间、系统取消返回时间及空 PID；不代表用户看到了 UAC 或主动取消。
- `hardware-v12-r1-post-restart-readonly.json`：重启后 503 项冻结输入、当前权限、未创建的实机执行目录及精确桌面的只读复核。

## 生产 v12 实机验收通过

2026-11-26 13:14:07–13:15:19（北京时间，`05:14:07–05:15:19Z`），生产候选 `0.4.0.12` 的管理员实机验收实际执行并通过。`hardware-install-v12-r1/result.json` 记录 `status: passed`、`hardwarePassed: true`、`rollbackPassed: true`、`desktopPassed: true`、错误列表为空，503 项封存输入校验通过，16 条原生命令全部退出 0。一次候选安装依次运行 standard、phone 两组 Smoke；组间先完成本组清理并恢复桌面基线，最后统一回滚原驱动。

管理员候选清单确认目标设备为 `ROOT\DISPLAY\0001`，候选为 `0.4.0.12`、`oem72.inf`，设备状态 OK、问题代码 0。实际承载候选模块的 WUDFHost PID 为 `312700`，管道进程 PID 与启动时间匹配，模块文件版本为 `0.4.0.12`、签名为 `Valid`，模块枚举无错误。所加载模块的后备 DLL 文件 SHA-256 为 `D7F424A0BCA371DE54C3A9C40ABF1C799FF1B9099F84F3B6B021A41350A0E38F`；该哈希是磁盘模块文件身份，不是进程内存哈希。

### 两组 Smoke 与独立驱动复核

两组均在受控 owner 范围使用显示器 ID `15`，分别通过创建、同 ID 替换、持有及释放；standard 从 `1280×720@60`、DPI 100 替换为 `1600×900@60`、DPI 100，phone 从 `1080×1920@60`、DPI 100 替换为 `1440×2560@60`、DPI 150。两组替换持有阶段各记录 10 次正确桌面尺寸快照，持续约 5 秒。两组结果均为 `passed: true`、`cleanupPassed: true`，清理及执行错误为空；`applicationStarted: false`、`noNativeDisplayWrites: true`，因此不将此 Smoke 解释为应用界面启动或应用端主动写显示配置的验收。

六项 legacy 无效果探针均保留当前 owner；错误 owner 的 Check、Replace、Release 均被拒绝且快照不变。首次释放返回 `Confirmed`，后续重复清理返回 `NotOwned`，均恢复精确桌面基线。本轮未观察到已绑定残影，不能把 v11 的残影离场覆盖记作 v12 的实测结果。独立驱动复核的 43 项检查全部通过，两组各 39 份物理屏快照均未变化。

### 四种模式的 CCD 复核

独立 CCD 复核解析 211 份 JSON：210 条正常记录无错误，最后索引为 210 的记录是观察器退出 0 时的正常停止标记。共观察到 90 份包含本轮虚拟路径的 CCD 采样快照，四种模式的源尺寸、目标 active 尺寸及目标 total 尺寸均一致，路径刷新率及目标 vSync 均为精确 `60/1`；源与目标尺寸不匹配为 0，物理 CCD 路径变化为 0。

| 组别与阶段 | 请求模式 | 匹配采样数 | CCD 原始文件范围 |
| --- | --- | ---: | --- |
| standard 创建 | `1280×720@60` | 20 | `ccd-0041.json`–`ccd-0060.json` |
| standard 同 ID 替换 | `1600×900@60` | 25 | `ccd-0061.json`–`ccd-0085.json` |
| phone 创建 | `1080×1920@60` | 21 | `ccd-0147.json`–`ccd-0167.json` |
| phone 同 ID 替换 | `1440×2560@60` | 24 | `ccd-0168.json`–`ccd-0191.json` |

这里的“采样数”是只读 CCD 状态观察次数，不是渲染帧数；配置的 200 ms 采样间隔也不等于全程连续、无间隙覆盖。CCD 的 `60/1` 不代表已独立测量原始 EDID DTD 时钟量化结果。

### 回滚、封存与通过范围

管理员运行内的 `inventory-restored.json` 确认设备恢复为 `0.4.0.1`、`oem17.inf`、状态 OK、问题代码 0；恢复后的 WUDFHost PID 为 `308872`，管道 PID 与启动时间匹配，模块版本为 `0.4.0.1`、签名为 `Valid`，模块枚举无错误，后备 DLL 文件 SHA-256 为 `00E6F27AA934DCAFC25FC4AF2735E59B966A7B0C169AFD55670E40CBF8A4FFFF`。精确桌面核验退出 0，输出 `original State and exact desktop verified, no write`，Secure Boot 保持启用。

13:16:58 的后续非管理员只读补充检查再次确认原 PnP 版本和设备状态，但模块枚举为空、管道身份探针因启动时间为空而未完成；该补充检查不作为模块或管道身份通过证据，也不覆盖管理员运行内已成功记录的恢复清单。最终复核记录 503 项输入复查漂移为 0，702 份原始输出文件重新计算哈希漂移为 0，运行后精确桌面再次核验退出 0。输出封存清单自身 SHA-256 为 `25C1A675CD4DCE5F3F3589A9D65C2F283B7A40892DD5E64F73EE236240EA7E96`。

本次通过范围为上述四种模式的受控 owner 实机 Smoke、CCD 状态、清理及回滚。没有覆盖真实应用界面、物理手机端画面、完整分辨率／刷新率矩阵、长时间稳定性、重启、崩溃注入或设备端原始 EDID 字节回读；不能据此宣称所有模式或整个应用已完成验收。当前已恢复原驱动，v12 未永久安装。实机阶段未修改生产源码。

本轮证据位于 `target/acceptance-complete-20261126/`：

- `hardware-install-v12-r1/result.json`、`inventory-candidate.json`、`inventory-restored.json`、`desktop-final.stdout.log`：实际安装、候选加载身份、16 条命令结果、回滚身份及精确桌面。
- `hardware-v12-standard-r1/smoke-v12-r1/`、`hardware-v12-phone-r1/smoke-v12-r1/` 下的 `result.json`、`events.jsonl`、`release-identity.json`：两组四模式、owner 探针、持有及重复清理。
- `hardware-install-v12-r1/ccd-observer/`：211 份 CCD 原始 JSON、观察器退出结果及四种模式的源／目标信号。
- `hardware-v12-r1-driver-review.json`、`hardware-v12-r1-ccd-review.json`：43 项驱动检查与 90 份虚拟路径 CCD 采样的独立复核。
- `hardware-v12-r1-live-restored-readonly.json`：后续非管理员补充检查及其模块／管道探针限制。
- `hardware-v12-r1-output-manifest.json`、`hardware-v12-r1-final-review.json`：702 份输出封存、503 项输入与输出零漂移复查，以及最终通过范围。

## 证据与系统变更边界

### 提权重验前的脚本修复计划

1. 先用 Windows PowerShell 5.1 复现正常 stderr 被误判为终止错误，再验证原生命令捕获只根据退出码判定成功。
2. 将每轮输出放入新的独立 target 目录；无论编译、断言或夹具创建失败，都落盘结果 JSON 并清理本轮夹具。
3. 清理仅遍历本轮指定夹具，遇到 reparse point 只删除链接；用保留目标哨兵文件的 junction 回归确认不会越界。
4. 管理员验收需 Debug/Release 的普通 ACL、显式 System32 ACL、两项普通 symlink 测试全部实际通过，并确认夹具已清理。编译通过或权限受阻均不能替代执行通过。

- `target/permission-acceptance-20261126/verified-results.json`：ACL 与 symlink 的实际退出码；symlink 编译结果和执行结果分列。
- 同目录的 `acl-*.log`、`acl-elevated-*.log`、`symlink-*.log`：Debug、Release 原始输出。
- `target/permission-acceptance-20261126/driver-readonly-evidence.json`：设备、INF、版本、文件哈希、签名及启动配置读取结果。

首次非提权检查未触发 UAC、安装或切换驱动、创建真实显示器，也未更改显示拓扑、开发者模式、Secure Boot 或启动配置。后续管理员重验经用户确认 UAC 后运行，权限测试使用独立 target、临时 symlink 夹具及隔离的 System32 测试目录。三轮 v9 候选、一轮 v10 和两轮 v11 隔离候选的安装与回滚均针对明确匹配的现有设备执行，没有更改证书信任存储、Secure Boot 或启动配置。第二轮和第三轮 v9 均经历首次提权取消，随后经用户再次授权实际执行；创建的临时虚拟屏已清理，原驱动和精确桌面核验均通过。第三轮 v9、v10 和 v11 使用的 CCD 观察器只读取状态；v10 的最小模式标志和 v11 的两模式动态 EDID 行为属于隔离实验，不能代替后续生产候选的验收。
