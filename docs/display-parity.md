# display 模块与 C# 版对拍摘要

范围：`crates/snug-win/src/display.rs` ↔ `references/ref-snug-win/src/SnugWin.Windows/Display/`
（`DisplayConfigService.cs` + `PhoneDisplayService.cs`）。只读核对，结论按方法逐条记录在
2026-11-18 的对拍报告里；这里留下需要长期记住的偏离与共同风险。

## 有意偏离 C#（保留）

| 项 | 内容 | 位置 |
|---|---|---|
| 固定手机屏 | v1.0.4 手机与平板使用固定分辨率，应用最小窗口宽度、返回列表和切换应用均不能自动扩屏。超宽窗口显示尺寸受限提示，不自动减小字号；仅用户明确更改屏幕设置时变更模式 | `show_list` / `wanted_width` / `start_width` / `fill_steps` |
| 手机底栏 | v1.0.1 按物理短边 / 400 × 60 留出底栏，配合手机 UI 缩放容纳 48 点按钮和上下留白；不再使用 C# 28 点底栏。极矮工作区优先保留应用区，底栏不越界。原几何 TSV 保留，旧 BAR 值明确排除等价声明，新尺寸单独回归 | `home_bar_height_of` / `window_area` |
| 稳定 ID | C# `HashCode.Combine` 是进程内随机化；我们用确定性 FNV-1a，跨进程稳定。**若将来持久化 display_id**，两版编号不通用，需与 C# 对齐或明确「不跨版本通用」 | `stable_id` |
| 查询加锁 | C# 的 PhoneDisplayService 是 async、等待时让出线程；Rust 无运行时，拆成「ScreenMaps 短锁 + ops 慢操作锁」。查询（界面每帧调 `home_bar_area`/`phone_bounds`）不经过 ops 锁，不会被 4s/8s/10s 的轮询挡住 | `WinPhoneDisplays` |
| CCD 重试 | **有意偏离 C#**：C# 的 `QueryDisplayConfig` 只查一次，热插拔窗口内会整次失败（所有屏 adapter/source 归零、DPI 掉回 100）。我们在 183/87 上重取 buffer sizes 重试 2 次，每次记 warn，仍失败返回空表 | `query_active_paths` |
| 舍入 | C# `Math.Round` 默认银行家舍入；Home Bar 像素、光标救援坐标、SetResolution 刷新率三处用 `round_ties_even`。`WidthOf` 兜底保持远离零（C# 那里明确 AwayFromZero） | `window_area` / `cursor_rescue_point` / `set_resolution` |
| 日志 | 补了 C# 的四处 Warn：SetResolution 返回码、建屏 8s 超时、撤屏 remove 失败、SetDpi 失败。查询路径不加日志 | 见源码 `crate::warn` |
| 取消 | C# 每个等待循环都有 CancellationToken；Rust `PhoneDisplays` 接口没有 token，退出时最长等 8s/4s/10s。**留债**，见 `impl PhoneDisplays for WinPhoneDisplays` 的 `TODO(取消)` | trait impl |

## 显示模式确认与归属

- 建屏和改分辨率时，读回 Windows 的实际宽高、刷新率后才报告成功。建屏模式失败进入撤屏与布局恢复流程，修改模式失败会尝试恢复上次确认的模式。
- 建屏认领验证 Cherry 显示器接口、PnP 父适配器硬件 ID 与 EDID 序列号。写入手机屏模式前复核绑定，GDI 名称变化时按唯一 EDID 重新绑定。通用的实体屏模式恢复只接受唯一的非虚拟目标。
- 撤屏未确认时保留归属和设备映射，由已有善后流程重试。建屏超时且未确认归属的编号会跳过。
- `ChangeDisplaySettingsEx(CDS_NORESET)` 成功但后续应用失败时，Windows 仍可能保留挂起模式。热插拔与克隆拓扑需在实际系统上验证。

## 与 C# 完全等价、但实现不同的地方

- `GetActiveMonitors` 先收 HMONITOR 再循环（顺序/过滤/默认值一致）。
- 字典用「小写键」代替 `StringComparer.OrdinalIgnoreCase`（设备名是 ASCII）。
- DPI 包（-3/-4、min/cur/max/scaleRel）与 C# 逐字段一致。
- 所有等待节奏（8/10/4s，150/250/100/300/200ms）一致。
- `TryAttachVirtualDisplay` 的 DEVMODE 字段、两次 ChangeDisplaySettingsEx 的 flags、
  失败后的 `SDC_APPLY|SDC_TOPOLOGY_EXTEND` 兜底一致。
- `MainArea`/`OtherDisplays`/`WidthOf` 的公式与过滤一致；窗口与底栏区域的触控尺寸偏离见上表。
