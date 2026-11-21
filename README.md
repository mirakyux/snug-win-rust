# snug-rs

SnugWin 的 Rust 重写。原项目是 [zephyr4123/snug](https://github.com/zephyr4123/snug)（macOS，Swift）
与社区移植版 [nicocart/snug-win](https://github.com/nicocart/snug-win)（Windows，C# / .NET 10 / WPF），
两者均为 MIT 许可。本仓库保留原作者的版权声明，只把 Windows 侧实现换成 Rust，
目标是一个**单 exe、零运行时依赖**的版本。

当前版本 **1.3.0**，对齐 C# 原版 v1.3.0 的功能面。改动明细见 [CHANGELOG.md](CHANGELOG.md)。

## 下载

到 [Releases](../../releases) 下载 `snug.exe`（本次优化构建 8.45 MiB，8,862,720 B）。这是便携单文件，
可放在当前用户可写目录直接运行，不需安装器、旁置 `driver\` / `assets\`，也不会默认请求 UAC。
程序内含官方驱动安装包、安装脚本和完整机型库。

双击 `snug.exe` 即可启动普通权限 GUI。若虚拟显示器驱动缺失，首次驱动安装会单独请求一次 UAC；
已有驱动且管道可用时不会请求。驱动安装脚本校验安装包和证书哈希，保留已装的同款驱动；正常结束后清理临时释放文件。

## 为什么重写

- `.NET` 桌面 UI（WPF）不支持 NativeAOT，自包含发布要 180 MB 磁盘；Rust 版目标是 10 MB 级别。
  实际交付为**内含驱动的单 exe**，大小与取舍见 [体积预算](docs/size-budget.md)。
- 原版最值钱的是行为细节（窗口归位、字号记忆、手势判定的各种边界），
  这些都在 `SnugWin.Core` 里，可以逐文件对照移植，并有单测兜底。
- 平台层用 `windows` crate 直接调 Win32 / COM，省掉一层托管封装。

## 目录

```text
crates/
  snug-core/   纯逻辑层，零 Win32 依赖（geometry / screen_fit / touch / scrolling /
               phone_models / config / record / left_behind / crop / zoom / store /
               screen_plan / desk 状态机）
  snug-win/    Windows 平台层（虚拟屏 IPC + CCD、DWM 搬窗、底层鼠标钩子、UIA、
               shell 图标、截图、UU 连接探测、自动更新）
  snug-app/    托盘程序与手机屏界面（egui/eframe）
assets/
  phone-models.json   机型库（853 款：手机 / 平板 / 电脑），与 C# 版共用
  *.ico               应用与托盘图标
docs/                 移植对照、验收单、多屏与线程模型、体积预算
references/           上游源码的浅克隆，仅供对照，已在 .gitignore 中
tools/refdump/        C# 参考实现的对拍生成器（几何 / ScreenFit 真值 TSV）
```

## 功能

| 模块 | C# 源 | 状态 |
|---|---|---|
| 几何 / 屏幕适配 | `Geometry` `ScreenFit` | ✅ 已移植 + 单测 + C# 真值对拍 |
| 手势状态机 | `TouchTranslator` `Scrolling` | ✅ 已移植 + 单测 |
| 机型库 | `PhoneModels` | ✅ 已移植 + 单测（含真实 assets 校验） |
| 配置 / 路径 | `AppConfig` `SnugPaths` | ✅ 已移植 + 单测（1.2 配置自动迁移到多屏结构） |
| 记录 / 存储 | `PhoneRecord` `JsonFileStore` `LeftBehind` | ✅ 已移植 + 单测（按屏分文件） |
| 裁剪 / 字号 | `CropGeometry` `ZoomBook` | ✅ 已移植 + 单测 |
| 窗口会话状态机 | `PhoneDesk` | ✅ 已移植 + 单测（共享串行队列、多屏 `DeskSet`） |
| 拼音搜索 | `PinyinSearch` | ✅ 已移植 + 单测 |
| 应用枚举 / 会话判定 | `Apps` `Session/*` `ControllerTable` | ✅ 已移植 + 单测 |
| 平台层 | `SnugWin.Windows` | ✅ 已移植 + 单测（虚拟屏 IPC / CCD / DWM / 钩子 / UIA / 截图 / 更新） |
| 界面 | `PhoneHomeWindow` 等 WPF | ✅ egui 重写（控制中心设置、主题、重入胶囊、桌面预览） |

全仓单测覆盖核心状态机、Win32 适配器、应用与更新事务（数量以当前测试摘要为准），
`rustc` 零警告是硬门禁，见 [docs/quality.md](docs/quality.md)。

**开机自启**在设置里开（「开机静默自启动」）。它注册一条计划任务
（`SnugWinAutoStart`，登录触发、普通权限、参数 `--minimized`）：开机后后台常驻托盘，
**不弹窗、不抢焦点**，手机屏就绪了才把界面摆上虚拟屏。换 exe 目录后会自动把任务里的
路径改过去。

主界面默认使用普通权限；驱动安装通过同一 EXE 的一次性 UAC 任务执行。当前驱动管道要求管理员权限，首次建屏会请求受限 VDD 代理的 UAC；取消或失败后本次运行不再重复请求，需重启 GUI 后再试。自动更新仅在普通权限下执行，使用 EXE 同级的独占暂存目录；受保护安装目录请手动替换 EXE。需要管理提升权限的应用窗口时，可在设置里显式选择“以管理员重启”，更新前应退出并普通启动。

与原版的差别与取舍理由集中在 [`docs/upstream-watch.md`](docs/upstream-watch.md)；
多屏形态（单窗口 + 顶部切换）见 [`docs/multi-screen.md`](docs/multi-screen.md)；
线程与锁纪律见 [`docs/desk-threading.md`](docs/desk-threading.md)。
本轮响应优化、编译体积对照与验证边界见 [`docs/optimization-20261119.md`](docs/optimization-20261119.md)。

## 构建

需要 Rust（stable 1.89+，edition 2024）+ MSVC 工具链 + Windows SDK。

```powershell
pwsh -File scripts/fetch-driver.ps1    # 编译前准备内嵌驱动，核对 SHA-256
cargo test --workspace
cargo build --release -p snug-app
pwsh -File scripts/build-release.ps1    # dist/snug.exe；另保留自动更新 zip 与校验和
```

`target\release\snug.exe` 正在运行时无法被覆盖，验证构建可用独立目录：

```powershell
cargo build --release -p snug-app --offline --target-dir target\release-check
```

## 发版

**手动触发**：GitHub → Actions → `Release` → Run workflow。
工作流会用根 `Cargo.toml` 的版本推导 tag（也可手填），依次准备驱动、跑全仓单测、
release 构建、`scripts/build-release.ps1` 打包和 `scripts/preflight.ps1` 静态门禁，
然后建 GitHub Release，上传单 exe、自动更新 zip 与 `checksums.txt`。

本地等价流程与资产命名规则见 [docs/release.md](docs/release.md)。

## 文档

- [docs/acceptance.md](docs/acceptance.md) — 真机验收单（八组）
- [docs/release.md](docs/release.md) — 发版流程（版本号、门禁、资产名、更新源）
- [docs/real-machine-test.md](docs/real-machine-test.md) — 真机测试记录
- [docs/display-parity.md](docs/display-parity.md) — 虚拟屏链路与原版逐行对拍
- [docs/ui-parity.md](docs/ui-parity.md) — 界面与原版对照
- [docs/size-budget.md](docs/size-budget.md) — 体积预算与已做的取舍
- [docs/quality.md](docs/quality.md) — 质量门禁（rustc 零警告、clippy 怎么用）

## 许可

MIT，见 [LICENSE](LICENSE) 与 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
