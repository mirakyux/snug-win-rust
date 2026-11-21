# 原版跟随清单

我们移植的两个原版项目、它们最近的改动、以及**哪些坑我们也踩**。
每次对原版做完一轮检查就更新这里，不靠记性。

## 两个原版

| 项目 | 地址 | 默认分支 | 本仓库参考位置 |
| --- | --- | --- | --- |
| C# / .NET 10 / WPF（主参考） | https://github.com/nicocart/snug-win | `main` | `references/ref-snug-win/`（1.2.0，git checkout）、`references/ref-snug-win-130/`（v1.3.0 快照） |
| macOS 原版（功能源头） | https://github.com/zephyr4123/snug | `main` | `references/ref-snug-swift/` |

C# 版的功能大多是从 macOS 原版同步过来的（`sync upstream v2.6.0`），所以**新功能先看 macOS 版，落地实现看 C# 版**。

### 怎么拉最新（本机 git fetch 会握手失败）

`git fetch` 走 schannel 在本机会 `SSL/TLS connection failed`，走代理也一样。可用做法：

```powershell
$ProgressPreference='SilentlyContinue'
Invoke-WebRequest -Uri "https://codeload.github.com/nicocart/snug-win/zip/refs/heads/main" `
  -OutFile "$env:TEMP\snug-win-main.zip" -Proxy "http://127.0.0.1:7890" -UseBasicParsing
Expand-Archive "$env:TEMP\snug-win-main.zip" -DestinationPath "$env:TEMP\snugwin"
```

GitHub API（`api.github.com`）直连可用，查提交列表用它最快。

## 最近一次检查

**2026-11-22**：核对 Windows C# v1.3.2 和 macOS v2.7.4。吸收了建屏时的 EDID 身份绑定、窗口跨屏搬移中转重试及旧机型名兼容。

- C# 版：最新 `8cc51835`（v1.3.2），包含社区 PR、macOS v2.7.4 同步、EDID 归属、
  UIA/会话与离线恢复调整。Rust 已按 VDD 的 EDID connector ID 精确认屏，身份缺失时不会猜选新显示器。
  尚未上线的屏通过 `EnumDisplayDevicesW` 的 monitor interface 查回 PnP 父适配器和 EDID，
  不将顶层硬件 ID 或子设备的注册表路径误当作 PnP 实例 ID。
- 窗口首次移动后仍落在目标屏外时，先经主屏中转，再重试一次；仍失败则沿用现有原位恢复流程。
  回归覆盖实际中转顺序、成功摆放和重复误放后的有限退出与原位恢复。
- macOS 版：最新 `5b411227`，最新功能 release 为 v2.7.4。
  v2.7.4 修复镜像屏的分辨率恢复；macOS 把镜像从属屏单独设模式/位置会破坏镜像布局。
  Windows 当前通过 CCD source/GDI 模式管理；尚无证据证明有同样的 target 级副作用，因此不照搬过滤规则。
- C# 的 `legacyNames` 兼容已为 Apple iPad 旧英文型号名登记，避免重载配置后机型名及设备类型过期。
- C# 的离线恢复依赖目标身份、远端独占判断与有界重建。Rust 目前没有等价生命周期与独占能力，
  仅复制重试循环可能改动错误的显示拓扑，暂不移植。
- 常驻主屏、远端设备分类建屏、固定原生像素和“立即结束”属于产品行为改动，本次没有纳入。
- 身份绑定保护针对建屏认领。已认领屏运行中被用户改成克隆拓扑时，现有按 GDI 名查屏的生命周期
  仍缺少持续身份校验；不把本次改动视为完整的克隆屏保护。在线建屏、热插拔和 Windows 克隆拓扑仍需真机验收。

**2026-11-18**：C# 版到 **v1.3.0**（`c223cd4`），macOS 版到 **2.6.1**（`c0ceccd`）。

### 本轮修改与验证（2026-11-22）

修改文件：`crates/snug-win/src/display.rs`、`crates/snug-win/Cargo.toml`、
`crates/snug-core/src/desk/engine.rs`、`crates/snug-core/src/desk/test_support.rs`、
`crates/snug-core/src/phone_models.rs`、`assets/phone-models.json`、
`docs/display-parity.md`、`docs/upstream-watch.md`。
删除了按显示名称、分辨率或“第一个新屏”猜测归属的兜底；窗口摆放失败复用既有原位恢复流程。
没有引入新 crate，Windows 绑定只增加现有 `windows` crate 的设备安装 API feature。

- Debug 和 Release 全仓测试各通过 557 项，跳过 1 项需管理员令牌的 staging ACL 测试。
- 两种 profile 的 Clippy correctness / suspicious 硬门禁通过；仍有风格建议，不宣称 Clippy 零警告。
- Release 构建无 rustc 警告；本地打包、Rust / PowerShell SHA-256 交叉校验及静态发布门禁通过（18 项，无跳过）。
- `git diff --check` 通过。四个触及的 Rust 文件仍有 rustfmt 格式差异，本轮未整文件或全仓格式化。
- 本地候选包：`dist/upstream-20261122/snug.exe`，8,883,712 B（8.47 MiB），资源版本仍为 1.0.0；未发布。
  SHA-256：`e6b7428a6e1475a817ba75ca9f4fb15f0e172829fe70b6b821610a8fb0603a7a`。
- 只读检查确认历史 Cherry EDID 和父适配器硬件 ID 符合校验链；当时无虚拟屏在线，不能据此证明在线认领、
  未上线屏扩展或真实窗口搬移已验收。未执行建屏、撤屏、驱动重启或真实远控操作。

## 功能对照

状态说明：✅ 已移植 / 🚧 进行中 / ⬜ 未开始 / ➖ 我们不需要

| 功能 | 来源 | 状态 | 备注 |
| --- | --- | --- | --- |
| 归属记录 `OwnedDisplayBook`（启动清理只拆自己建的屏） | C# 1.3.0 | ✅ `366b5fa` | 不做的后果是同机两版互拆 |
| 建屏超时不再误删未知设备 | C# 1.3.0 `6f548dd` | ✅ `056b501` | |
| 候选 display id 按已有虚拟屏数轮转 | C# 1.3.0 `6f548dd` | ✅ `056b501` | |
| 建屏按 `RegistrationOrder` 从宽到窄注册 | C# 1.3.0 | ✅ `5faa528` | |
| 多屏多机型（一块屏对一台设备，最多 4 块） | 上游 2.6.0 / C# 1.3.0 | ✅ 状态层 + 切换 UI | `DeskSet` `19dee61`，UI `652e02d`；与原版「一块屏一个窗口」的差别见 `docs/multi-screen.md` |
| 机型库扩充（853 款：手机/平板/电脑 + 横竖屏） | 上游 2.6.0 | ✅ `a0a2a4a` | |
| 横屏 6 列网格 | 上游 2.6.0 | ✅ `b155e4c` | 原版 `UpdateResponsiveLayout`：横屏 6 列、竖屏 4 列 |
| 主题 Light/Dark/Auto | C# 1.3.0 `2146d19` | ✅ `608e80c` | 调色板 + `Visuals`，`Auto` 读系统注册表 |
| 重入 pill（手机屏收起后同一会话里点一下重进） | C# 1.3.0 `6491a65` | ✅ `b155e4c` | 托盘项 + 主屏右下角胶囊（复用同一窗口，不新开视口） |
| 托盘「预览手机界面…」 | C# 1.3.0 | ✅ `6ede168` | |
| 桌面手机预览（没装驱动也能看界面） | C# 1.3.0 `f1d64e8` | ✅ `6ede168` | 只读预览窗，缩放走 `ScreenFit::compute_preview_layout` |
| 设置窗改成「控制中心」卡片式首页 | C# 1.3.0 `5e141b9` | ✅ `608e80c` | 五页签索引照 `SelectTab`；首页四张卡 + 多屏页 |
| 机型选择器里的**自定义分辨率** | C# 1.3.0 `PhoneModelWindow` / `CustomModelSection` | ✅ | 没在搜索或搜不到时才露出来；宽高都要填，横向填的自动摆成竖屏 |
| 添加屏时**自定名字** | C# 1.3.0 `AddScreenNameBox` | ✅ | 留空按机型与方向自动取名 |
| 设置/关于窗摆到"从哪块屏点开的"那块屏 | macOS 2.6.1 / C# 1.3.0 | ➖ | 我们的设置窗画在手机屏窗口内，天然就在那块屏上 |
| 应用内打包驱动 + 异步安装 | C# 1.3.0 `db9635d` | ✅ | `settings.rs` + `driver/` 脚本 |
| 关于窗口先定尺寸再居中 | macOS 2.6.1 `5bca1f6` | ➖ | 同上，没有独立关于窗 |
| 两块主屏互换算位置 | macOS 2.6.1 `22a4f65` | ➖ | 只适用于"C# 那样每块屏一个窗口"的形态；我们是单窗口 + 切换，没有两块窗互相换位这回事 |

## 坑（原版踩过 / 我们踩过）

**这一节的每一条都是真机上出过问题的**，改相关代码前先读一遍。

### 原版修过、我们也会遇到的

| 坑 | 症状 | 出处 |
| --- | --- | --- |
| 建屏超时 `Remove(候选 id)` | 把**别人的**虚拟屏拆掉；我们和 C# 版同机时必现 | C# `6f548dd`，我们 `056b501` |
| 候选 id 固定从 0 开始 | 已有虚拟屏时撞号，`wait_for_online` 认错屏或白等超时 | C# `6f548dd`，我们 `056b501` |
| 启动清理 `RemoveAll` | 同机两版互相拆屏 | C# `OwnedDisplayStore` |
| 重复的 DWM 标题栏按钮 | 自绘标题栏上又叠了系统的 最小化/最大化/关闭 | C# `2146d19`（去掉 `WS_SYSMENU`/`WS_MINIMIZEBOX`/`WS_MAXIMIZEBOX`） |

### 我们自己踩过、原版没有的

| 坑 | 症状 | 修法 | 提交 |
| --- | --- | --- | --- |
| `GetWindowTextW` 对同进程窗口同步发 `WM_GETTEXT` | 退出时 UI 线程等 desk 线程撤屏、desk 线程等 UI 线程回消息 → 互等，进程退不出去、虚拟屏残留 | 改 `SendMessageTimeoutW(WM_GETTEXT, 150ms)`，且跳过自己进程的窗口 | `dee0c10` |
| `SetWanted` 只在启动时设一次 | 退出远控后虚拟屏一直挂着，只有杀进程才消失 | 每拍按 `会话 InEpisode && 控制端` 重算，照 C# `OnTick` | `8d6197c` |
| 虚拟屏管道句柄缓存到进程结束 | `pnputil /restart-device` 之后写的是死管道，四个候选全失败且无日志 | 每次操作前 `PeekNamedPipe` 探测，写/读失败即丢弃重连 | `99770bb` |
| UU 推流日志增长单独判新会话 | 本地 GameViewer/游戏时 SLOG 也会增长，造成手机屏重复弹出 | 新会话由覆盖层/控制端准入；已确认会话仍用 SLOG 维持空闲与重连 | 2026-11-20 |
| `ViewportCommand::InnerSize` 与 `SetWindowPos` 抢尺寸 | 窗口 926×1629 比屏小一圈，右边留黑边 | 只用 `SetWindowPos`（物理像素），每帧用 `frame_of` 核对 | `115beb8` |
| 设置窗画在 1080×56 的指示条里 | 设置窗被裁成一条灰边，什么都点不到 | 打开设置时窗口恢复整屏 | `82268b5` |
| 判断"有没有铺窗口"只看 `placed.is_some()` | 点了「‹ 资源库」也永远停在指示条上，回不到列表 | 条件改成 `placed.is_some() && !list_visible` | `37ad208` |
| `Start-Process` 走 ShellExecute 起 exe | 弹「打开文件 - 安全警告」，或干脆不起进程 | 改 `ProcessStartInfo(UseShellExecute=$false)` | `5944a92` |
| 退出时正撞上加屏重试阶梯 | 收尾卡 3 分钟以上、进程 `Responding=False`，用户以为死机 | `Desk::shutdown` 先在队列外取消重试，再带 20 秒上限等撤屏；超时留日志、屏由归属记录兜底 | 2026-11-19 |

### 仍未知 / 未验证

- 托盘右键菜单与双击行为：UIA 能读到图标（`SnugWin - 手机屏：关`），`SendInput` 模拟点击
  始终没命中（DPI 虚拟化导致坐标偏差，`SetProcessDpiAwarenessContext` 无效）；
  2026-11-19 又试了 **UIA 的 `InvokePattern`**，日志没有任何反应（`tray-icon` 的按钮不响应
  UIA 调用）。结论：托盘/设置窗只能手点。用户实际点过是好的。
- 托盘 tooltip 在 UIA 里两段粘在一起（`SnugWin - UU远程手机竖屏伴侣 SnugWin - 手机屏：在线`）：
  **已确认不是 bug**。`tray-icon` 把初始 tooltip 同时用作隐藏窗口的窗口标题，之后
  `set_tooltip` 只改 `NOTIFYICONDATA.szTip`、不改标题，于是 UIA 的 Name = 标题 + szTip；
  鼠标悬停看到的是 szTip（干净的一行）。crate 没有改窗口标题的接口，保留现状（`tray.rs` 有注释）。

## C# 1.3.0 里其它小改动（逐文件数过）

这些不是新功能，是**多屏化必须的前置**或顺手修的问题：

| 改动 | 内容 | 对我们的影响 |
| --- | --- | --- |
| `SerialLane` 抽成独立文件 | 由 `DeskSet` 建**一条共享 lane** 传给所有 desk | 多屏时必须串行化，否则两块屏同时建屏会抢显示配置 |
| `PhoneDesk.ReenterScreen()` | `if (!_wanted \|\| !State.Dismissed) return;` → 清 `Dismissed` 与 `Notice` 再 reconcile | 我们托盘"重新进入"要实现同样的语义 |
| `PhoneDesk.UpdateWish(name, fit, behavior)` | 每拍按 `ScreenPlan.Wish` 更新屏名/比例/行为 | 多屏的驱动力在这里 |
| `EvacuateComputerScreenWindowsAsync` | 电脑屏（非触控）建好后，把落在它范围内的窗口搬到主屏居中 | 否则建一块"电脑屏"会把用户的窗口吞进去 |
| `DetachPlacementForTransfer` + `transferFromOtherScreen` | 把手机屏 A 上的窗口直接转到屏 B，沿用**原始还原坐标** | 多屏之间搬窗口不能丢还原位置 |
| `RecordFileForScreen(id)` | 每块屏一份 `screen-<id>.json` 还原记录 | 现在只有一份 `phone-record.json` |
| `IScreenService.CreateAsync(fit, displayName)` | 建虚拟屏时带名字 | 多块屏在系统里能区分开（现在没有名字参数） |
| `TouchHookService.SetRegions(...)` | 一份区域列表 + `_activeRegion` 按按下点选 | 多屏时钩子要同时管多块屏 |
| `UUSensor` 判定哲学改了 | TCP 与服务端可见窗降级为**纯诊断**；新连接和断线恢复要求 UIA 读到控制端，SLOG 只在最近控制端证据后的 15 秒内维持连接 | 见下 |

### 关于 UU 在场判定：我们和原版 1.3.0 不一样，是有意的

历史问题：`.slog` 本地刷新曾直接作为新会话证据，2026-11-18 日志显示无覆盖窗、控制端不可读时，
capture 心跳每几秒翻转，引发反复加屏/撤屏。2026-11-19 的只读采样也确认无手机连接时，
本地 GameViewerServer 仍会周期性增长 streamer 日志。TCP 不能简单补作唯一证据，因为真远程传输
可能走 UDP，空闲时 peer 会消失。

当前准入策略：开启**新连接**或从断开恢复，都需 UIA 在“本机被控中”控制表里读到非空控制端；服务端可见窗不参与准入，
因为目前只检查可见性和尺寸，不能确认它真是远控覆盖层。SLOG 心跳只维持最近一次控制端证据后的已有连接，最长 15 秒，不能从断开状态恢复。
只有找到“本机被控中”下的控制表，空结果才表示已确认无控制端；设置页或无该表的窗口表示不可读。已确认空表优先于 SLOG，避免断开后的本地日志掩盖断开；不可读则仍使用有界连续性。
代价是：如果会话刚开始时 UU 在托盘隐藏或控制端表读不到，自动开屏无法确认；用户可用设置里的手动强制模式。
`ADVANCE_PAIR_MAX_GAP = ACTIVE_HOLD_WINDOW = 15 秒` 只解决已有连接的短暂空闲续期，不能充当
开始或恢复连接的证据。
