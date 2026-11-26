# 继续审查与修复记录（2026-11-26）

本轮继续检查应用交互、还原记录、命名管道客户端、权限代理和驱动。此前的归属协议与设置保存修复见[上一轮记录](overnight-audit-fixes-20261126.md)，不重复计入本轮结果。

保留工作区既有修改，不新增依赖。分辨率编辑仍为草稿，必须显式应用。未提交、推送、签名、安装驱动或改变真实显示拓扑，也未启动会自动建屏的应用实例。

## 客户端与权限代理

合法状态原先可能超过客户端和代理共同使用的 128 KiB 上限。专项先复现 8 屏、每屏 256 模式的 133,692 字节状态被拒绝；再用真实驱动约束构造最坏合法状态，State 为 291,554 字节，代理包装后为 291,619 字节。

`crates/snug-win/src/vdd.rs` 按驱动的 16 屏、每屏 256 模式、尺寸上限、刷新率及名字转义推导出 292,576 字节的有界响应上限。直连按单个 EOT 帧检查，连续的合法 Changed 通知不会累计触发大小限制。

`crates/snug-win/src/vdd_broker.rs` 复用响应上限，请求仍限制为 128 KiB。真实命名管道回归另发现，非阻塞写入一次提交大于内核缓冲的回复会一直无法前进，直到 15 秒超时；现在按 4 KiB 分段发送，保留原绝对超时和对端存活检查。

新增 7 项回归覆盖最大合法状态、精确上限与 EOT、超限 State/Changed、代理双向限额和非法长度头。Debug、Release 的客户端 43 项、代理 22 项均通过。独立复核还调用了实际驱动校验函数，确认最大状态样本合法。

证据：`target/broker-size-audit-20261126/response-size-acceptance.json`、同目录红灯及测试日志、`target/broker-size-independent-review-20261126/`。

## 驱动交换链退出

`vendor/virtual-display-rs/virtual-display-driver/src/swap_chain_processor.rs` 原先只在取帧返回 E_PENDING 时检查停止信号。持续成功取帧时，清理等待处理线程退出可能一直卡住。

新增生产函数回归在修复前运行 200 毫秒、处理 9,328,652 帧仍不能自行退出，必须注入 native 错误才释放。修复后每帧完成处理也检查停止信号；Release 回归处理约 30,033 帧后自行停止，交换链仅删除一次。表面释放、帧完成和交换链删除顺序沿用既有实现。

驱动版本更新为 `0.4.0.6`。修改文件还包括 `driver/tests/swapchain-harness.rs`、`driver/source.lock.json`、驱动 `build.rs` 和 `driver/README.md`。

固定 `nightly-2024-07-26` 下，资源回归 Debug、Release 各 12 项，初始化各 6 项，生命周期各 32 项通过。源码格式检查通过。日志分别位于 `target/audit-driver-resources-20261126/`、`target/audit-driver-init-20261126/`、`target/audit-driver-lifecycle-20261126/`。

D0 重复初始化的候选问题已对照微软回调文档和样例检查。目前没有证据证明本设备在同一进程中会产生第二次成功的适配器初始化，未据此修改电源回调；睡眠唤醒仍属于硬件验收范围。

## 设置交互

主触控屏禁用、另有启用屏时，默认建屏会回退到启用屏，分辨率失败提示却标在禁用的主屏上，实际使用的屏幕看不到失败提示。

`crates/snug-app/src/controller.rs` 提供统一的 `selected_screen_id`；`crates/snug-app/src/ui.rs` 的失败提示复用同一选择结果。默认状态、建屏愿望和失败提示保持一致，删除了重复的选择分支。显式选择仍按原语义处理，无启用屏时不虚构编号。

专项先红后绿，应用 Debug、Release 各 124 项通过。证据为 `target/ui-failed-resolution-{red,debug,release,clippy}-20261126.log`。

## 还原记录与重试

修改集中在 `crates/snug-core/src/desk/engine.rs`、`crates/snug-core/src/desk/set.rs`、`crates/snug-core/src/desk/test_support.rs` 和 `crates/snug-core/src/store.rs`，复用原存储和恢复流程。

1. 窗口消失后迁移到 `left-behind.json`，若目标保存失败，原流程仍删除源记录，或者让新窗口覆盖源记录。现在先保存目标，失败保留源与原摆放状态，不把未保存的记录发布到内存账本。恢复成功后才交接字号债务，失败重试不会提前重复偿还。
2. 源还原记录临时无法读取时，原流程把它当作不存在。真实 Windows 文件锁回归允许写入和删除、拒绝读取，已复现新窗口覆盖旧文件。现在通过 `try_pending` 传播读取失败、提示错误并停止本次放置；解锁后恢复旧窗口，再允许放置新窗口。
3. 独立复核又用允许读写、拒绝删除的文件锁，复现源记录结算失败后重复迁移与字号偿还：两次恢复产生两条目标记录，字号变化为 -4。普通窗口恢复也会在字号记账失败后重复偿还。现在通过 `try_settle` 检查源删除结果；失败保留记录并停止交接。目标交接按相同记录去重，自动归还在源、目标仍同时待交接或源暂时不可读时停止消费；重启加载也执行同一保护。正常待还原记录继续允许无关窗口归还。
4. 普通恢复先持久化字号偿还意图，成功后才操作窗口。记账失败不改字号，窗口操作失败则恢复原债务；源删除失败保留重试。错误提示保留“记录无法写入”，不误报成窗口无响应。
5. 去重与过期清理的先后顺序另有一项回归：先去重再清理可能删除唯一的目标记录。现在先清理，再补入当前交接；账本已满时为当前记录保留一个槽，即使已有记录的时间戳在未来，也不会把刚交接的唯一目标淘汰后再删除源。
6. 独立故障注入进一步复现：字号偿还意图写入成功，但原生操作失败，恢复原债务的回写又被文件锁阻止。原恢复流程丢弃返回记录，下次读到磁盘上的零债务后直接删除源，漏掉偿还。现在在进程内保留待回写记录，相关恢复、写入、结算和自动归还先完成回写；跨屏转移失败明确停止，不能把失败当成无源继续放置。删除屏配置仍保留待写债务；同一屏重新创建时将债务交给新实例，防止旧实例迟到写入覆盖新记录。
7. 跨屏转移原先只清除源屏摆放状态，没有结算源账本。独立回归记录到 A 放大两级、转到 B 后两个账本各欠两级；恢复 A 后再归还 B，最终错误缩小两级。修复将交接放在共享串行队列内协调，目标是否接管以首条还原记录成功保存为准。源无法结算时停止转移，目标尚未接管时恢复源记录与摆放状态；目标已经接管后，后续摆放失败沿用目标的恢复流程，避免重新生成源债务。最终交互复核又补上拒收时的资源库可见状态回滚，保留来源原先显示窗口或显示资源库的选择。

前两项有 5 项回归先红后绿，并经独立复跑。后续补充 10 项回归，覆盖拒绝删除、重启加载、自动归还、无关窗口、字号失败回滚、零字号结算、存储错误语义和过期/容量边界。

最终再增加 9 项回归，覆盖回滚写失败的恢复与新窗口放置、直接字号修改、跨屏源拒删、目标拒收、目标后续摆放失败，以及撤屏和同编号重建。目标拒收测试同时覆盖资源库原先隐藏和显示两种状态，有独立红灯及绿灯日志。核心测试因此从 293 项增至 302 项。

独立复核额外使用 3 项隔离测试，覆盖先恢复源再归还目标、目标拒收且源补偿写入也失败、目标已经接管但摆放和字号回退都失败。隔离核心快照 305 项通过，其中 302 项为生产测试、3 项为独立验证；不把这 3 项重复计入工作区测试数。最后的资源库状态小修另经只读复核通过。

主要证据为 `target/continued-recovery-{red,zoom-red,final}-20261126.log`、`target/continued-recovery-read-{red,green}-20261126.log`、`target/restore-settlement-{red,prune-red,green-debug,core-debug,core-release,clippy}-20261126.log`。独立红灯另见 `target/reviewer-restore-delete-20261126/proof.log`、`target/reviewer-put-back-delete-20261126/proof.log`、`target/reviewer-restore-final-20261126/rollback-proof.log` 和 `target/reviewer-transfer-ledger-20261126/proof.log`。

最终专项及独立验证见 `target/atomic-audit-20261126/`、`target/reviewer-transfer-ledger-20261126/final-independent.log`、同目录 `final-core.log` 和 `target/transfer-view-state-{red,green}-20261126.log`。

本轮复用现有账本、摆放状态和串行队列。跨屏交接集中到 `DeskHub`，选屏规则集中到控制器，保存与结算集中检查返回值，删除原先散落的选择和摘取分支；没有新增依赖或通用事务层。

## 驱动构建与独立验收

最终目录为 `target/continued-driver-build-20261126-final/`。以固定 nightly、WDK 和 LLVM 离线冷构建成功，DLL 和 INF 版本均为 `0.4.0.6`，日期为 `11/26/2026`。

独立验收 40 项通过：PE 为 x64，14 项 Rust/WDK ABI 一致，Inf2Cat 无错误、无警告，4 项校验和与 ZIP 的 5 个成员及内部哈希一致。真正的构建前清单包含隐藏 Cargo 配置，共 73 项，当前哈希与字节均匹配；独立枚举确认驱动实际输入覆盖 53 项。

清单覆盖仓库内源码与构建配置，不涵盖外部编译器、SDK、WDK 和 registry 依赖内容，因此不据此宣称完整可复现构建。早期目录遗漏隐藏配置的清单仅保留为过程证据，最终产物使用本节的新目录。

未签名包：`target/continued-driver-build-20261126-final/snug-virtual-display-0.4.0.6-x64-unsigned.zip`。

ZIP SHA-256：`e0ad21f8aa97490bc78319c565cceff68da648a81779a0e3834c8d55e4a70a1c`。

证据包括目录内 `build-invocation.json`、`build.log`、`pre-build-source-manifest.json`、`independent-artifact-acceptance.json` 和 `independent-build-source-coverage.json`。

## 整体验收与剩余边界

最后一项恢复及交互修复合入后，离线工作区 Debug、Release 各通过 813 项，另各跳过 1 项既有的管理员 ACL 测试 `elevated_atomic_staging_inherits_production_acl`。每种配置包含应用 124 项、核心 302 项、Windows 层 383 项及集成测试 4 项。验证前后对源码及 Cargo 配置清单逐项核对，整个验证期间未发生源码变化。

`cargo clippy --workspace --all-targets --offline -- -D clippy::correctness -D clippy::suspicious` 通过。这不代表零告警：既有风格建议仍存在；此前 `-D warnings` 被核心模块 20 项既有告警阻断，未扩大为全仓清理。相关结果见 `target/continued-final-clippy-20261126.log` 和 `target/restore-settlement-clippy-20261126.log`。

应用 Release 离线构建成功。静态检查 `target/release/snug.exe` 的文件及产品版本均为 `1.0.7`，大小为 9,478,656 字节，SHA-256 为 `a30648b187a323664890ac72f6f34a7fcf4c892e0db42fa22defd60644f65bb6`；未启动应用。构建和产物记录见 `target/continued-final-app-build-20261126.log`、`target/continued-app-artifact-20261126.json`。

最终命令、退出码及测试计数汇总在 `target/continued-workspace-final-validation-20261126.json`，对应源码清单为 `target/continued-workspace-final-source-20261126.json`，完整测试日志为 `target/continued-final-debug-20261126.log` 和 `target/continued-final-release-20261126.log`。

本轮没有建立跨源、目标两个文件的掉电事务，也不能保证外部窗口字号操作与磁盘写入之间的原子性。待回写记录仅保证同一进程内保留和重试；源已结算但目标尚未保存，或回滚仍被锁阻止时，突然退出、进程终止和掉电仍可能丢失恢复信息。原生字号接口返回失败但实际已产生部分效果，也不在此次服务契约内解决。驱动回归使用生产函数与 native 故障注入，代理使用真实命名管道，界面回归覆盖生产选择逻辑；这些结果不能替代安装后的 GPU 输出、显示拓扑、睡眠唤醒和 UU 远控验收。

默认安装入口仍内嵌上游 `0.3.1`，走 Legacy。新 V2 协议需加载新驱动并重新登记新版 helper。最终驱动未签名、未安装；应用未安装或运行。真实设备与长时资源测量需另行使用签名后的产物验证。
