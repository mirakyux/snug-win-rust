# 带归属显示器的原生写入竞态修复（2026-11-26）

## 修复计划

1. 先添加回归，覆盖最终写入前 connector/GDI 名被复用、单模式正常收敛、Windows 不采用目标模式、自动 DPI 不写入，以及未自动启用时不按设备名附加桌面。
2. V2 创建与模式替换只发布当前目标模式；复用现有 `SnugCreate` / `SnugReplace` 的 owner 条件，在驱动 mutation lock 内完成归属校验和模式表修改。不得退回 GDI。
3. V2 等待实测目标分辨率与刷新率；不收敛、归属丢失或读回失败返回现有可见失败，保留后续清理需要的令牌。校验只用于读回确认，不能作为随后原生写入的授权。
4. V2 停止自动 DPI 修改，保留 Windows 当前 DPI。窗口区域与底栏继续使用同一次实际屏幕快照的物理尺寸和工作区，不冒充已经达到期望 DPI。Legacy 路径保留兼容行为，不声称拥有 V2 的所有权保证。
5. 运行 display 回归、crate 测试、格式和静态检查；不安装、签名、加载候选驱动，不改变真实显示拓扑。

## 契约依据与功能边界

- `ChangeDisplaySettingsExW` 接受枚举得到的 GDI 设备名，没有 owner 或 generation 条件：<https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-changedisplaysettingsexw>。
- `DISPLAYCONFIG_DEVICE_INFO_HEADER` 仅含 type、size、adapterId、id：<https://learn.microsoft.com/en-us/windows/win32/api/wingdi/ns-wingdi-displayconfig_device_info_header>。当前 DPI 包使用未公开的负数 type，不具备已公开的 owner 绑定保证。
- `SetDisplayConfig` 通过 exclusively enabling 指定路径写完整拓扑，不能替代 owner compare-and-swap，陈旧快照还可能覆盖并发拓扑更新：<https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setdisplayconfig>。
- 因此不使用额外查询、写后校验或短租约冒充原子修复。V2 不再执行上述无法绑定 owner 的模式/DPI/附加写入；系统未自动启用或未采用唯一目标模式时，操作明确失败。
- DPI 的功能变化必须在诊断/UI 中说明；用户可在 Windows 设置中自行调整。硬件实测是单独的验收边界，mock 收敛测试不能证明新驱动在真实 Windows 会自动完成模式切换。

## 实现结果

- `display.rs` 的 V2 创建、换比例和换档改为目标单模式；应用复用已有 owner 条件的 `SnugReplace`，未增加协议、依赖或驱动版本。
- 模式设置只有在 owner 与实际桌面模式读回均满足条件时成功。拒绝、不收敛或未知结果均不执行 native mode、DPI 或 attach 回退；清理仍持有原令牌。
- V2 不再自动将 DPI 改成 200%；窗口布局继续按实际快照计算。Legacy 的原生模式设置、DPI 及附加行为保留，并由单独的兼容测试覆盖。
- Legacy 原生写入前的复查仅接受无令牌且协议仍为 Legacy 的会话。即使分流后另一个会话才把 V2 令牌写入共享归属记录，也会拒绝继续原生写入，不能用新的 `SnugCheck` 成功结果授权旧分支。
- `driver/atomic-update.md` 已同步归属查询用途和功能边界；驱动已具备原子 owner 校验，新增生产源码提取回归验证成功查询后的 connector 复用也不能被旧 owner 替换。

## 验证结果

- 先运行新增的 3 个 owned 模式回归，旧实现 3 项均失败；修复后全部通过。它们覆盖最后一刻连接器复用、保留实测 175% DPI、Windows 不采用目标模式时禁止 native 回退。
- 新增未自动启用输出的回归，确认 `NeverOnline`、零原生附加/分辨率/DPI 写入且保留凭据。
- `cargo test -p snug-win --all-targets --offline --target-dir target/owner-race-20261126`：单元测试 418 通过、1 忽略；几何集成测试 3 通过；SHA256 集成测试 1 通过；示例编译通过。
- `driver/tests/run-lifecycle-regressions.ps1 -ArtifactDirectory target/owner-race-driver-20261126`：从当前生产驱动源码提取的 debug 和 release 测试均 38/38 通过，包括 owner check 后 connector 被新 owner 重建时旧令牌写入无副作用。
- `cargo clippy -p snug-win --all-targets --offline --target-dir target/owner-race-20261126 -- -D clippy::correctness -D clippy::suspicious`：通过；仍有已有的风格/简化类警告。
- 本次范围 `git diff --check` 通过。`rustfmt --check --edition 2024 crates/snug-win/src/display.rs` 因该文件已有的广泛未格式化内容失败（从 imports 即开始）；未通过整文件格式重写扩大差异。
- 最终复核补充了“分流后磁盘出现 V2 令牌”的确定性回归，旧复查函数失败、锁定 Legacy 后通过；随后 `snug-win --all-targets` 为 419 通过、1 忽略，几何测试 3 通过、系统 SHA256 测试 1 通过。

## 未执行的硬件验收

未安装、签名或加载候选驱动，未改变真实显示拓扑。上述测试证明应用不会沿 V2 路径写入可复用的原生标识、现有驱动拒绝旧 owner 的替换；不证明真实 Windows 一定会自动启用输出或采用唯一目标模式。这两项应在独立硬件环境验收，失败时应用的确定行为是报告失败并保留清理凭据。
