# display 四改验收单（真机逐项）

对象：`crates/snug-win/src/display.rs` 的四笔改动——A2 锁粒度、A4 排障日志、A5 银行家舍入、
C1 CCD 重试。目标不是"再测一遍功能"，而是**确认这四改没有把 C# 已对齐的行为改歪**。

前置（和 `docs/real-machine-test.md` 的阶段 B 相同）：

```powershell
Stop-Process -Name SnugWin.App -ErrorAction SilentlyContinue   # 退掉 C# 正式版
$env:SNUGWIN_HOME = "$env:USERPROFILE\snug-rs-test"
.\target\release\snug.exe
```

日志：`$env:SNUGWIN_HOME\logs\snug-YYYYMMDD.log`。行格式 `[HH:mm:ss] [Warn] …`，
平台层告警经 `snug_win::set_warn_sink` 落进来，不在 stderr。

判定原则：**每一项都先看"正常路径有没有多出日志"，再看好路径是否还成立**。
四项全部通过 → 勾掉本节末尾的表；任一项失败，附日志行与复现步骤。

---

## A2 锁粒度：查询不被慢操作挡住

改了什么：`ScreenMaps`（fits/device_names）短锁 + `ops` 慢操作锁。`create` / `reshape` /
`retire` / `set_width` / `set_mode` / `cleanup_orphans` 走 `ops`；`fit_of` / `is_online` /
`width_of` / `bounds_of` / `window_area_of` / `main_area` / `other_displays` / `home_bar_area_of`
只拿 `maps`，锁内不碰任何系统调用。

怎么验：

1. 打开设置窗，把"默认字号放大"从 +2 改成 +4（触发 `reshape`，最长 4s 轮询）。
2. 改完**立刻**：把设置窗拖一下、切到"驱动与日志"页再切回来、右键托盘打开菜单再关掉。
3. 再触发一次撤屏（关"手机屏"开关，`retire` 最长 10s），期间重复第 2 步。

通过：

- 整个过程中设置窗/托盘**始终能响应**，没有"点一下等好几秒"的卡顿；
- 日志时间戳连续，`reshape`/`retire` 期间仍有新的 `desk:` / UU 采样行；
- 手机屏窗口本身暂时不动是**预期**的（慢操作在等系统），不是卡死。

失败判定：

- 慢操作进行中界面冻结 → 说明查询又拿回了慢操作锁（A2 被改回去）；
- 进程卡死超过 15s → 先查加锁顺序：现在固定 **ops → maps**（慢操作持 `ops`，内部只短暂取
  `maps` 存结果），任何地方反过来（持 `maps` 再去拿 `ops`）都会死锁；
- 查询结果在慢操作结束前可能短暂陈旧（`fits`/设备名在最后一步才更新）——这是预期，不是卡顿。

参考单测（不替代真机，但能挡住大部分回退）：`display.rs` 里的 `window_area_*`、
`cursor_rescue_*`、`create_*` 系列都跑在假实现上；锁本身没有单测，真机这一步是唯一的证据。

---

## A4 排障日志：四处 Warn 该出现时才出现

正常建屏/换比例/撤屏**一条都不该出现**；出现即异常，按下表对照：

| 日志原文（`[Warn]` 之后） | 触发条件 | 正常路径期望 |
|---|---|---|
| `ChangeDisplaySettingsEx(<设备名>, <宽>x<高>) 返回 <码>` | 设置分辨率失败 | 不出现 |
| `手机屏（id=<n>）创建后 8s 内未在 Windows 上线，尝试备用 ID` | 驱动没吐出新屏 | 不出现 |
| `移除手机屏 #<id> 失败：<原因>` | 撤屏时驱动拒绝 | 不出现 |
| `设置 DPI 缩放 200% 失败（status=<码>）` | 设 DPI 档位失败 | 不出现 |

失败注入（只做一次，确认日志通路活着）：

1. **8s 超时**：停掉 virtual-display 驱动服务后再开"手机屏"（或直接拔驱动），
   期望日志出现上面第 2 行，且随后按候选 ID 换下一个、最终阶段 `Failed`。
2. **撤屏失败**：建好屏后停驱动服务，再关"手机屏"开关，期望第 3 行。
3. **分辨率/DPI 失败**：在机型里选一个明显越界的自定义分辨率（如 20000×20000），
   期望出现第 1 行（或建屏前就被拒绝）；DPI 那行可在驱动异常时观察。

通过：注入失败时对应 Warn 出现且只出现一次；恢复驱动后下一次建屏不再有残留 Warn。

---

## A5 银行家舍入：三处数值与手算一致

C# `Math.Round(double)` 默认 **ToEven**；三处已改成 `round_ties_even`：
`window_area`（Home Bar 像素）、`cursor_rescue_point`（光标救援坐标）、
`set_resolution`（刷新率取整）。

按当前公式手算的期望值（默认 200% DPI：手机屏模式宽 = 逻辑宽 × 2，
`window_area` = 工作区高度 − Home Bar 像素）：

| 机型 | 逻辑尺寸 | 虚拟屏模式 | Home Bar | 手机屏窗口应为 |
|---|---|---|---|---|
| Apple iPhone 17 Pro Max（1320×2868） | 540×950 | 1080×1900 | 56 px | **1080×1844** |
| Apple iPhone 17e（1170×2532） | 540×917 | 1080×1834 | 56 px | **1080×1778** |
| Apple iPhone 18 Pro（1206×2622） | 540×930 | 1080×1860 | 56 px | **1080×1804** |
| Apple iPad mini (A20 Pro)（1512×2316） | 620×800 | 1240×1600 | 64 px | **1240×1536** |

（最后一行的逻辑宽不是 540，用来确认"档位算法 + 缩放后的 Home Bar"这条链路。）

怎么验：

1. 依次选上表四个机型，每换一次等手机屏 `Ready`。
2. 看日志里 `手机屏界面：铺满资源库，搬到 (x, y) W×H`（或"收成底部指示条"）的 `W×H`，
   与上表最后一列**逐像素相等**。
3. Windows 显示设置里虚拟屏的分辨率与"模式"列相等；刷新率 60 Hz 不能是 59/61。
4. 撤屏时把鼠标停到手机屏上，等撤屏完成后光标应落在主屏中心。

关于 `.5`：现有 800 个机型里没有一个会让逻辑高度正好落在 `.5`，真实显示器宽度也是偶数，
所以**真机这一步主要验"整数路径没被改坏"**；银行家舍入的 `.5` 语义由单测钉住
（`rounding_matches_dotnet_math_round`、`window_area_removes_home_bar_at_200_percent`、
`cursor_rescue_*`），不要指望真机上肉眼看出 300.5→300 这种差别。
同理，`set_resolution` 的刷新率取整在真机上只会看到 60，`.5` 只可能来自异常 EDID。

失败判定：窗口高度差 1 px（典型：1843/1845）、Home Bar 差 2 px（54/58）、刷新率 59/61
→ 说明某处又用回 `.round()`（远离零）；对照 `round_ties_even` 的三个调用点。

参考：`rounding_matches_dotnet_math_round`、`window_area_removes_home_bar_at_200_percent`、
`window_area_keeps_at_least_100_points_and_28_point_bar`。

---

## C1 CCD 重试：热插拔窗口内不再整次失败

改了什么（有偏离 C#）：`QueryDisplayConfig` 遇 `183`（INSUFFICIENT_BUFFER）或
`87`（INVALID_PARAMETER）时重取缓冲区，最多重试 **2 次**（即最多 3 次尝试），每次记一条：

```
QueryDisplayConfig 返回 183（拓扑可能刚变化），重取缓冲区重试 1/2
QueryDisplayConfig 返回 87（拓扑可能刚变化），重取缓冲区重试 2/2
```

其它返回码**不重试**（直接返回空表），这是有意的。

怎么验：

1. 建好手机屏后，插拔一台真实显示器（或反复开关手机屏模拟拓扑变化），连续触发 3–5 次。
2. 同时盯两处：日志里的 `QueryDisplayConfig` 重试行；`other_displays` 的消费方
   （设置窗状态、手机屏摆放）是否瞬间"所有屏消失 / DPI 掉回 100%"。

通过：

- 每次拓扑变化最多 2 条重试 Warn，且两条之间不超过 1 秒；之后恢复正常采样；
- 不出现"其他显示器的分辨率/DPI 归零"或手机屏被误判成离线；
- 没有重试 Warn 也算通过（说明那次刚好没错过窗口）。

失败判定：

- 一次事件刷出 >2 条重试（重试计数被改错）；
- 对非 183/87 的返回码也重试（会把一次普通失败拖成三倍耗时）；
- 重试后仍拿不到路径却继续建屏/搬窗（应返回空表、由上层降级）。

---

## 勾选表

| 项 | 结论 | 证据（日志行/截图/坐标） |
|---|---|---|
| A2 慢操作期间界面不冻结 | ☐ 通过 ☐ 失败 | |
| A4 正常路径零 Warn | ☐ 通过 ☐ 失败 | |
| A4 三类失败注入各有对应 Warn | ☐ 通过 ☐ 失败 | |
| A5 三个机型的窗口尺寸逐像素相符 | ☐ 通过 ☐ 失败 | |
| A5 刷新率 60 Hz 无 ±1 | ☐ 通过 ☐ 失败 | |
| C1 每次拓扑变化 ≤2 条重试、随后恢复 | ☐ 通过 ☐ 失败 | |
| 全程 C# 正式版未被启动、未串数据目录 | ☐ 是 ☐ 否 | |

## 附：看日志的常用命令

```powershell
$log = Get-ChildItem "$env:SNUGWIN_HOME\logs\snug-*.log" | Sort-Object LastWriteTime | Select-Object -Last 1
Select-String -Path $log -Pattern '\[Warn\]|手机屏界面已搬到|QueryDisplayConfig|desk:' | Select-Object -Last 80
```
