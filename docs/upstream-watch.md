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

**2026-11-19**：两个上游自 v1.3.0 / 2.6.1 起没有新的功能提交，不需要跟。

- C# 版：最新仍是 `c223cd4`（`chore(release): prepare v1.3.0`，2026-11-18 15:24Z）。
- macOS 版：最新 `43c9e24`，2.6.1 之后只有 README 与团队记忆相关的提交
  （`b428f1ef` 加「自定义屏幕」小节、`d945aca2`/`43c9e24b` 是记忆笔记）。
  README 里「设备 / 型号 / 方向 / 名字」这四项对应 C# 的添加屏面板——见下面功能表最后两行，
  原先我们漏了「自定义分辨率」和「自定名字」，这轮补上。

**2026-11-18**：C# 版到 **v1.3.0**（`c223cd4`），macOS 版到 **2.6.1**（`c0ceccd`）。

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
| UU 推流日志增长单独判在场 | 空闲时 logger 约 3.05s flush 一次，正好压在 C# 的 `<= 3.0s` 阈值上 → 每几十秒误判一次"在远程"，屏自己弹出来 | 增长只作诊断字段，判在场要有对端/覆盖窗/控制端佐证 | `2702cf1` |
| `ViewportCommand::InnerSize` 与 `SetWindowPos` 抢尺寸 | 窗口 926×1629 比屏小一圈，右边留黑边 | 只用 `SetWindowPos`（物理像素），每帧用 `frame_of` 核对 | `115beb8` |
| 设置窗画在 1080×56 的指示条里 | 设置窗被裁成一条灰边，什么都点不到 | 打开设置时窗口恢复整屏 | `82268b5` |
| 判断"有没有铺窗口"只看 `placed.is_some()` | 点了「‹ 资源库」也永远停在指示条上，回不到列表 | 条件改成 `placed.is_some() && !list_visible` | `37ad208` |
| `Start-Process` 走 ShellExecute 起 exe | 弹「打开文件 - 安全警告」，或干脆不起进程 | 改 `ProcessStartInfo(UseShellExecute=$false)` | `5944a92` |

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
| `UUSensor` 判定哲学改了 | TCP 对端降级为**纯诊断**（`LastPeerTcpObserved`），在场只看 `StreamLogActivityTracker` | 见下 |

### 关于 UU 在场判定：我们和原版 1.3.0 不一样，是有意的

原版 1.3.0 把"对端 TCP"从判定里拿掉，只留推流日志增长（`StreamLogActivityTracker`：
两次增长间隔 ≤3s 且最近一次增长在 2s 内）。我们**保留**了"日志增长要有佐证"这条：

- 真机实测：空闲时这台机器的 logger 约 **3.05 秒** flush 一次、每次 148–933 字节；
  1 秒采样会把 3.05s 量化成 3.0s，正好压在原版 `<= 3.0s` 的阈值上。
- 回放 60 秒真实日志，原版算式在 60 个采样点里有 **2 个**误判成"在推流"。
- 后果很直接：人坐在电脑前，手机屏每隔几十秒自己弹出来一次。

所以我们的规则是：`capture_active` 必须有 `peer_active || overlay_visible || 控制端非空`
之一佐证；裸的日志增长只进 `log_growth` 诊断字段。见提交 `2702cf1`。
如果以后原版改了阈值，这里可以回退——但要先拿真机会话数据校准。
