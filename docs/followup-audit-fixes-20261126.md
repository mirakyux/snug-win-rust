# 后续审查与修复记录（2026-11-26）

本轮在[上一轮已验收源码](continued-audit-fixes-20261126.md)上继续审查应用、恢复账本、Windows 通信和驱动。实施顺序见[本轮计划](followup-audit-plan-20261126.md)。保留既有工作区修改，不新增依赖，不提交或推送。

分辨率编辑保持草稿，必须显式应用。没有启动会自动建屏的应用，没有签名、安装或切换驱动，也没有改变真实显示拓扑。

## 默认选屏与预览

主触屏被禁用且另有启用屏时，控制器会选择启用屏，但桌面预览、主页标题和选屏勾选仍可能使用禁用的主屏。预览因此采用错误的分辨率或设备类型。

`crates/snug-app/src/ui.rs` 新增 `selected_screen_config`，复用控制器已有的 `selected_screen_id`。预览尺寸、电脑屏判断、主页标题和勾选共同遵守控制器规则，删除重复的默认选择分支；显式无效 ID 不暗中回退。

两项回归先红后绿，应用专项 126 项通过。证据：`target/followup-preview-red-20261126.log`、`target/followup-app-green-20261126.log`。独立只读复核及相关六项回归通过。桌面设置覆盖并发候选另经 eframe 0.32.3 回调顺序核查，没有证实实际配置丢失，未据此修改。

## 无令牌旧账本与权限代理

旧归属记录只有 `DisplayIds`，没有 V2 owner token。若编号已被其他 owner 复用，原启动清理会发送裸删除；仍存活的 Legacy 会话切换到 V2 后，也可能裸替换外来显示的模式。两条路径均已通过隔离回归复现。

`crates/snug-win/src/vdd.rs` 的 `replace_legacy` 和 `remove` 现在在 V2 返回 `Unsupported`，不发送 State 或无令牌写入。Legacy 写入失败、重连探测到 V2 时也停止重放，删除旧的 JSON 类型猜测、重新分帧和裸删除后查询状态的分支。Legacy 的正常替换和删除保留原行为，删除仍只报告已提交。

已登记的旧 helper 不含新保护，仍可能接受旧请求后无令牌操作 V2。因此 `vdd_broker.rs` 将代理请求名改为 `LegacyOnlyReplace` 和 `LegacyOnlyRemove`；旧 helper 在访问驱动前拒绝未知请求，新 helper 不接受旧名字作为别名。没有修改驱动协议。

该阶段 `display.rs` 仅增加夹具和回归，验证外来显示不被删除或改模式、旧字号和设备名绑定保留、旧归属不误销账，以及空编号旧记录不被复用。生产客户端和代理另用真实测试命名管道验证 V2 拒绝路径发送零字节；新 helper 的序列化到执行再到 Legacy 管道的正常路径通过。

该阶段 Windows 专项 392 项单元测试通过，另三项几何对拍通过，一项既有管理员 ACL 测试忽略。常规 SHA-256 集成入口虽返回成功，但该次未提供资产环境变量，不能计作实际资产校验；最终工作区验收另显式指定本轮驱动 ZIP。Clippy correctness/suspicious 通过，仍有既有风格建议。

证据：`target/reviewer-tokenless-cleanup-20261126/proof.log`、`target/tokenless-v2-red-20261126.log`、`target/tokenless-v2-reshape-red-20261126.log`、`target/tokenless-v2-broker-red-20261126.log`、`target/tokenless-v2-green-20261126.log` 和 `target/tokenless-v2-clippy-20261126.log`。

V2 上无令牌的旧账保留供人工核对，不自动猜测归属或清空；已登记的旧 helper 需要重新登记新版。

## 原生分辨率和 DPI 调整的归属保护

继续复核确认：旧会话的编号与 GDI 名称同时被外来 owner 复用后，模式表更新虽会拒绝旧凭据，`set_width` 仍能直接修改新显示的分辨率和 DPI。该路径不需要并发竞态。隔离复现记录见 `target/reviewer-width-owner-20261126/observation.log` 和 `safety-red.log`。

驱动 `0.4.0.8` 新增专用只读 `SnugCheck`，在 `MONITOR_MUTATIONS` 内检查已接受的缓存和匹配凭据。查询不刷新预留、不创建或替换显示、不调用 WDF，也不广播变化。原 `SnugReserve` 的空编号预留语义保持不变。

`vdd.rs` 复用现有严格归属应答解析；只有命令、owner 和 `Applied` 均匹配才继续。无令牌只允许 Legacy，且不发送查询命令。代理新增 `VerifyOwner` 请求名，旧 helper 在访问驱动前拒绝；新 helper 重新确认协议。通信失败不自动重放，也不回退裸模式操作。

`display.rs` 的临时分辨率写入、持久回退和 DPI 写入分别先验证归属，再重新确认显示绑定。失败保留当前 fit 和持久凭据供后续恢复。检查失败不会重新注册模式或猜测资源已被删除。该保护覆盖写入前可观察到的归属失效；查询与后续原生调用仍有跨进程 TOCTOU 窗口。

新增五项显示控制回归和六项客户端／代理回归均通过，修复前红灯已留存。Windows 整库为 403 项通过、一项既有 ACL 测试忽略。驱动协议 18 项通过；直接抽取生产逻辑的生命周期测试在 Debug、Release 各 37 项通过，新增五项验证只读性、空预留拒绝、未完成到达的缓存、非法参数和修改锁等待。证据为 `target/native-mode-owner-{red-fresh,green}-20261126.log`、`target/owner-verification-{red,green,snug-win-lib}.log` 和 `target/driver-owner-check-20261126/`。

## 跨屏恢复与残留来源

来源屏 A 已将债务写入共享账本，但源文件拒绝删除时，B 原先只能看到自身来源，可能提前消费 A 的共享副本。A 解锁后再次恢复会重复偿还字号。B 的自动归还也可能把 A 正在摆放的窗口误认成旧记录返回的窗口，移动位置并修改字号。

`crates/snug-core/src/desk/engine.rs` 在现有共享串行队列内汇总活跃和退休来源的待恢复记录、待回写影子与当前摆放窗口。共享账本的自动归还、显式放置匹配、应用级字号偿还和过期/容量裁剪共同遵守保护；来源读取失败时暂停共享消费。成功写入或结算后立即更新保护，避免同一操作继续使用旧状态。

退休来源在账本未结算时继续保留并重试。同 ID 重建复用此前的影子交接规则，旧实例不再恢复新实例的同一路径，避免迟到回写或恢复覆盖当前摆放。

配置已删除的屏在重启后原先不会创建恢复实例，残留源因此仍被遗漏。`set.rs` 在首次配置愿望或恢复时枚举精确的 `phone-screen.json` 和 `screen-<id>.json`，跳过当前愿望及活跃 ID，其余以默认 `wanted=false` 的既有实例加入退休恢复队列，不创建显示。完整目录枚举成功后才登记，失败暂停共享消费并允许重试；缺失目录视为空。精确匹配的路径即使暂为目录也交给原读取错误路径保护，不静默忽略。

最终复核又复现来源债务更新的边界：失败交接留下相同共享副本后，原窗口重新可操作，来源恢复先把 zoom 写成零，会使旧共享副本失去完整记录相等保护，随后再次偿还。来源偿还、当前摆放记录更新与待回写影子落盘前，统一先持久撤回相同的冗余共享副本；撤回失败保留源债务并停止。共享副本删除采用先保存候选、成功后发布内存的顺序，保存失败不丢失重试依据。

跨屏转移也必须遵守该顺序：刷新待回写记录、撤回共享冗余、结算来源，均成功后才移走来源 placement。新增真实 deny-delete 锁回归，覆盖共享文件可写、共享文件锁定时拒绝转移及解锁重试。来源结算前的共享撤回遗漏先红后绿；正常 `leave_behind` 有意将共享副本提升为权威，不修改通用结算函数以免撤掉有效记录。`transfer_` 七项、`handoff` 17 项及 `deny_delete` 三项专项均通过。

另用允许写入和删除、拒绝读取的真实 Windows 文件锁，复现启动读取 `left-behind.json` 失败后被当空账本，后续交接覆盖原有债务并结算新来源。`SharedDeskBooks` 现在区分未加载与空账本，读取失败保留重试状态；Hub 刷新与来源副本撤回入口都要求成功读取共享账本后才能继续。两项生产回归覆盖解锁后再交接、持续拒读但允许覆盖写入的情况：持锁时原文件字节不变、来源保留并提示不可写，解锁后新旧两笔债务同时持久化。红灯为 `target/shared-book-read-proof-20261126/shared-read-red.log`，绿灯为 `target/shared-book-read-green-20261126.log`。

独立验收另增加两项隔离回归，覆盖没有活跃屏的退休来源（窗口存在或消失）、没有源文件及空 Hub 后新增屏。全部 `desk::set::tests` 41 项通过，其中生产回归 39 项、独立回归两项；没有发现此次共享加载检查的绕过路径。证据为 `target/shared-book-verification-20261126/independent-locks.log` 和 `set-verification.log`。

修改还包含 `desk/test_support.rs` 的故障夹具，复用现有账本、恢复实例和队列，没有引入事务框架或新依赖。专项红灯分别保存在 `target/reviewer-multiscreen-recovery-20261126/` 下的 `proof.log`、`active-placement-proof.log`、`startup-proof.log`、`debt-mutation-red.log` 和 `source-directory-red.log`。

## 驱动请求输入和日志重试

`vendor/virtual-display-rs/virtual-display-driver/src/ipc.rs` 的连接缓冲原先可在对端持续不发送 EOT 时无限增长，并反复扫描已经读过的全部数据。现在每帧 JSON 负载最多 512 KiB，结束符不计入；只扫描新输入，完整帧立即处理，多个合法帧和跨读取半包仍正常，超长连接直接关闭。上限覆盖当前驱动允许的最大请求，未照搬较小的客户端响应或代理请求限额。

`entry.rs` 的日志后台重试超过五分钟时，原先把 `PDRIVER_OBJECT` 强转成 `WDFDEVICE` 调用 `WdfDeviceSetFailed`，违反框架句柄契约。现在到期仅停止日志重试，删除错误调用及跨线程原始指针捕获；日志成功后立即退出重试。管道 DACL 没有调整。

新增 `driver/tests/run-input-regressions.ps1` 和 `input-harness.template.rs`，直接抽取生产帧缓冲和日志重试代码验证。初始红灯为三项通过、六项失败；修复后 Debug、Release 各九项通过。固定 `nightly-2024-07-26` 下，生命周期各 32 项、初始化各六项、资源与日志各 12 项、原子协议十项和 driver-ipc 17 项也通过，受影响驱动源码的 rustfmt 检查通过。以上均为隔离回归，没有加载真实驱动。

红绿及回归证据：`target/followup-driver-input-red-20261126/`、`target/followup-driver-input-green-20261126/`、`target/followup-driver-lifecycle-20261126/`、`target/followup-driver-init-20261126/`、`target/followup-driver-resource-20261126.log`、`target/followup-driver-atomic-20261126.log` 和 `target/followup-driver-protocol-20261126.log`。

独立只读验收另直接包含生产协议代码，通过真实序列化、解析和校验构造 16 屏、每屏 256 模式、每模式四种刷新率和最大转义名称。最大紧凑 `SnugReplace` 请求为 291,615 字节，距离新上限仍有 232,673 字节。七种长度与八种分块组合覆盖空帧、4096 字节边界、精确上限、超限、已完成帧和末尾半包。原九项与独立协议及边界 12 项在 Debug、Release 各通过，共 42 次执行；生产源码哈希前后一致。证据为 `target/driver-input-independent-20261126/`。

版本更新至 `0.4.0.7`，同步修改驱动 `build.rs`、`driver/source.lock.json` 和 `driver/README.md`。

## 0.4.0.7 候选驱动构建基线

该次基线目录为 `target/followup-driver-build-20261126-final/`。固定 nightly、WDK 和 LLVM 下离线冷构建成功，DLL 和 INF 的版本均为 `0.4.0.7`，日期均为 `11/26/2026`。后续归属查询修复使用下节的新构建，不以此产物代替当前源码。

独立静态验收 40 项通过：x64 PE32+、14 项 Rust/WDK ABI 一致、四项校验和一致、ZIP 恰好包含五项且包内哈希一致，DLL 与 CAT 均未签名。构建前源码清单共 75 项，在该次验收时字节和哈希全匹配；独立枚举的 53 项必要构建输入均被覆盖，包括三个隐藏 Cargo 配置。清单不覆盖外部工具链、SDK/WDK 头文件和 registry 源内容，不据此宣称完整可复现构建。

独立隔离副本再次运行 Inf2Cat，退出码为零，错误和警告均为 None。新旧 CAT 的字节差异仅为目录标识和生成时间，成员与扩展一致；原始 DLL、INF、CAT 和 ZIP 的哈希复查均未变化。

- 未签名包：`target/followup-driver-build-20261126-final/snug-virtual-display-0.4.0.7-x64-unsigned.zip`。
- ZIP SHA-256：`837f82f820c092db29578e5243e10b7872258f560cc94e6286e7115128e6633b`。
- DLL SHA-256：`343f8423cdc78a21f35eb55ef863f1814ea3e3e9884defdfa9f2f1a0445f17b3`。

证据为目录内 `build-invocation.json`、`pre-build-source-manifest.json`、`independent-artifact-acceptance.json`、`independent-build-source-coverage.json`、`independent-inf2cat/original-artifact-hash-recheck.json` 和 `independent-inf2cat/catalog-comparison.json`。

驱动预留状态机另经只读审查，直接抽取生产状态逻辑的原 32 项和独立五项回归在 Debug、Release 各通过，共 74 次执行，未确认新缺陷。证据为 `target/reservation-state-review-20261126/`。整合前再次核对 75 项源码清单、40 项静态验收与驱动 ZIP，零源码不匹配、零验收失败，见 `target/followup-driver-artifact-recheck-20261126.json`。

## 0.4.0.8 候选驱动与独立复核

新增归属查询后，在 `target/owner-check-driver-build-20261126-final/` 重新离线冷构建完整 UMDF 驱动。构建前后均为 75 项输入，文件集合、大小和 SHA-256 无变化。该清单包含构建脚本、锁文件和隐藏 Cargo 配置；外部工具链、SDK/WDK 及 registry 内容仍不在清单范围。

独立静态验收 40 项通过：DLL 为 x64 PE32+，文件与产品版本均为 `0.4.0.8`，INF 为 `11/26/2026,0.4.0.8`，ABI 对照 14 项一致。Inf2Cat 无错误、无警告；ZIP 精确包含五项，包内逐项哈希与四项校验和一致。DLL 和 CAT 均为 `NotSigned`。

- 未签名包：`target/owner-check-driver-build-20261126-final/snug-virtual-display-0.4.0.8-x64-unsigned.zip`。
- ZIP SHA-256：`64e3fc77670a275d01d22d16bbb2ca5f0ebc1048ad4aab5e3c5de410f0cb3298`。
- DLL SHA-256：`f95be233f59e77ed9dbebcb6733cd96bf0ed57cba7796cb6585c76151d798be3`。
- 输入清单 SHA-256：`86ab9a47c435625d268cf037cbade5f18831613f4c7cf942641c325d7b6e7736`。

构建证据为该目录内的 `build-invocation.json`、`pre-build-source-manifest.json`、`independent-artifact-acceptance.json` 和 `artifact-recheck.json`。

另一独立审查者核对了三个原生模式写点、客户端严格应答、代理兼容拒绝和驱动只读查询，没有发现可稳定复现的新绕过。独立客户端 11 项回归通过，驱动生产抽取生命周期 Debug、Release 各 37 项通过。证据分别位于 `target/reviewer-owner-check-20261126/` 和 `target/reviewer-owner-driver-20261126/`。这些检查不覆盖真实显示硬件，也不能消除查询到原生写入之间的竞态。

## 最终工作区与驱动回归

所有生产改动完成后，在全新目录 `target/owner-check-workspace-build-20261126-final/` 离线验收。工作区 110 项输入的集合、大小和 SHA-256 在全部命令执行前后保持一致。Debug、Release 都显式设置资产路径和系统预期哈希，实际对本轮 `0.4.0.8` ZIP 执行 Rust／系统 SHA-256 交叉校验；没有将未指定资产的提前返回算作资产验证。

| 检查 | 最终结果 |
| --- | --- |
| 工作区 Debug，全部 target | 855 项通过，1 项既有管理员 ACL 测试忽略 |
| 工作区 Release，全部 target | 855 项通过，1 项既有管理员 ACL 测试忽略 |
| Clippy correctness、suspicious | 通过，既有风格告警仍保留 |
| 应用 Release 构建 | 通过，未运行应用 |
| 驱动 IPC 协议 | 18 项通过 |
| 驱动生命周期生产抽取 | Debug、Release 各 37 项通过 |
| 驱动原子更新生产抽取 | Debug、Release 各 11 项通过 |
| 驱动资源生产抽取 | Debug、Release 各 12 项通过 |
| 驱动初始化生产抽取 | Debug、Release 各 6 项通过 |
| 驱动输入与重试生产抽取 | Debug、Release 各 9 项通过 |
| 驱动产物静态验收与 ABI | 40 项静态检查、14 项 ABI 对照通过 |

Debug、Release 每组工作区总数分别由应用 126、核心 322、Windows 403、几何对拍 3 和实际资产交叉校验 1 项组成。驱动抽取测试使用固定 `nightly-2024-07-26`；原子更新和资源脚本原本仅跑 Debug，本次另用同源 harness 补跑 Release。四组补充回归后再次对照 75 项驱动输入，零变化。

全量验收摘要为 `target/owner-check-workspace-final-validation-20261126.json`，源码清单为 `target/owner-check-workspace-final-source-20261126.json`，命令日志为 `target/owner-check-final-{debug,release,clippy,app-build}-20261126.log`。其余四组驱动回归位于 `target/owner-check-final-driver-{atomic,resource,init,input}-20261126/`，各含实际计数、退出码、日志和哈希。主线再次运行候选产物复核，40 项检查通过且驱动输入无变化，见 `target/owner-check-driver-final-recheck-20261126.log`。

应用产物为 `target/owner-check-workspace-build-20261126-final/release/snug.exe`，文件和产品版本均为 `1.0.7`，大小为 9,525,248 字节，SHA-256 为 `be36ed944ea05b8744d2cad3177c53f13d634cf15b4084fb3bd06e123218a905`。仅检查文件元数据及哈希，没有启动该 EXE。记录见 `target/owner-check-app-artifact-20261126.json`。

此次没有新增依赖。共享恢复继续使用原账本和串行队列，归属检查复用现有应答校验；删除无令牌 V2 处理中的协议猜测和危险回退。最终 `git diff --check` 通过，未提交或推送。

## 报告时点风险与后续实测状态

本节原先记录的三类代码问题已进入后续补修，当前实现与最终验收以 [崩溃安全与归属竞态补修](crash-safety-followup-fixes-20261126.md) 为准，不能继续将旧风险描述当作当前代码状态。

- 跨屏改为同目录原子、禁止覆盖的账本移交；字号改为持久执行意图，未知结果阻止自动重放并提供明确的手动恢复入口。任意机器掉电和外部应用输入消费仍不在原子性证明范围。
- V2 模式写入改走带 owner 的驱动原子 replacement，不再以归属查询授权后续原生模式、DPI 或附加写入。Legacy 兼容路径的原生行为仍保留，V2 自动启用和模式收敛须硬件验收。
- 本报告时点的管理员 ACL 提权断言受阻、普通 symlink 创建返回 Win32 1314；这是当时的运行结果，不代表后续状态。2026-11-26 11:19:48–11:20:16 的显式管理员复验中，System32 ACL、文件/目录 symlink 与存储回归均通过，夹具清理通过。证据见[权限与真实驱动验收记录](permissions-hardware-validation-20261126.md)的“管理员重验结果”。
- 本报告描述的 0.4.0.8 候选未签名、未加载也是阶段快照。随后签名候选 0.4.0.12 于 13:14:07–13:15:19 完成有限实机 owner-scoped Smoke/CCD 验收并回滚至 0.4.0.1，未永久安装。该结果不覆盖完整应用界面、全分辨率矩阵、长稳、重启/异常注入或 UU 手机远控；仍需继续完成这些发布证据。详情见[权限与真实驱动验收记录](permissions-hardware-validation-20261126.md)的“生产 v12 实机验收通过”。
- 应用默认内嵌仍为上游 0.3.1 安装包，尚未升级为 0.4.0.12；本轮没有进行安装、登记或发布操作。v12 的临时实机验收不代表默认发布包已更换驱动。
