# 多屏与设备

对应 C# v1.3.0 / 上游 2.6.0 的「一块屏对一台设备，最多 4 块」。

## 已经对齐原版的部分

| 能力 | 实现 |
| --- | --- |
| 屏配置（id / 名称 / 机型 / 方向 / 启用，上限 4） | `snug_core::ScreenConfig`，旧配置的 `phone` 自动迁移成 `screens[0]`（id = `LegacyPhoneId`） |
| 机型库 | 853 款，带 `DeviceKind`（手机 / 平板 / 电脑）与 `ScreenOrientation`（竖 / 横） |
| 比例与档位 | `ScreenFit::from_device`，六条分支（Phone/Tablet/Computer × 竖/横）逐行照 C# |
| 屏愿望 | `ScreenPlan::wishes(config, remote, preview)` |
| 多 desk 管理 | `snug_core::desk::DeskSet`：一块屏一个 desk、**共享一条串行队列**、每屏一份还原记录 |
| 跨屏搬窗口 | `DeskSet::pull` → 别的屏铺着同一窗口时 `detach_placement_for_transfer`，沿用原始还原坐标 |
| 退出 App 前归还 | `DeskSet::quit` 先把别的屏上这个 App 的窗口还回原位 |
| 电脑屏疏散 | 电脑屏（非触控）建好后，落在它矩形里的窗口搬到主屏居中 |
| 命名虚拟屏 | `PhoneDisplays::create_named`，每块屏在系统里名字不同 |
| 触控钩子多区域 | `TouchHook::set_regions`，按按下点选区域 |
| DPI | `ScreenFit::pixel_size` / `target_dpi_percent`（电脑屏大尺寸退回 1 倍 / 100%） |

## 我们和原版不一样的地方（有意）

**C# 是一块屏一个 `PhoneHomeWindow`（每块屏一个独立 OS 窗口，同时显示）。**

我们只有**一个**界面窗口，所以多屏是「单窗口 + 顶部屏切换」：同一时刻画面只属于一块屏，
状态、铺窗、底部指示条都跟着选中的那块走。

理由：

1. eframe/winit 在这个程序里对第二个视口很敏感——真机上撞到过"窗口 `Visible(false)` 之后
   一帧都不出，托盘点击和退出请求全部失效"（见 `upstream-watch.md` 的坑列表）。
   再开一个 OS 窗口去赌帧循环稳定，代价是用户每天在用的主屏可能被拖坏。
2. 用 UU 远程时人一次只看着**一台设备**，同时铺 4 块屏在"看"这件事上没有收益。
3. 状态层（`DeskSet`）已经把多屏完整实现了，以后要接多窗口，只换 UI 层，不用动状态机。

如果哪天要做真正的同时显示，改法是 `update()` 里对每块额外屏
`ctx.show_viewport_immediate(...)`，窗口标题按屏 id 区分，再用 `SetWindowPos` 摆到对应虚拟屏上；
优先级低于"主屏稳"。

## 配置怎么改

设置 → 多屏与设备：

- 每块屏一张卡：名称、`类型 · 方向` 徽标、规格（`宽 × 高 · DPI%`）、启用开关、
  预览界面、换机型（内嵌机型选择器）、删除。
- 添加区：手机 / 平板 / 电脑 + 竖屏 / 横屏 + 选型号 → 「选型号并添加」。
- 电脑屏强制横屏（与 C# `ScreenConfig.Sanitize` 一致）。
- 上限 4 块，到顶后按钮禁用并提示。

## 生效时机

与 C# 一致：屏愿望只在**远程会话中**（或开了"手动强制手机屏"）才算数。
`remote = InEpisode || ManualOverride` → `ScreenPlan::wishes` → `DeskSet::set_wishes`，
每秒一拍。会话结束 → 愿望清空 → 所有屏撤掉、窗口归位。
