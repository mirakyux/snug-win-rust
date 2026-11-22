# Snug for Windows

Snug 是配合网易 UU 远程使用的 Windows 工具。它在电脑上创建虚拟手机屏，把选中的窗口放到屏上，方便在手机上操作。

这是独立维护的 Rust 重写版，从 **v1.0.0** 开始发布，交付含驱动的单个 EXE。它源自 macOS 的
[Snug](https://github.com/zephyr4123/snug) 和 Windows 的 C# 移植版
[SnugWin](https://github.com/nicocart/snug-win)，保留两者的来源与版权声明。版本变更见 [更新记录](CHANGELOG.md)。

## 下载

从 [GitHub Releases](https://github.com/mirakyux/snug-win-rust/releases/latest) 下载 `snug.exe`。
把文件放在当前用户可写的目录，双击即可启动。程序内嵌虚拟显示器驱动安装包、安装脚本和机型库。

主界面以普通权限运行。缺少虚拟显示器驱动时，首次安装会单独请求管理员权限。已安装的驱动若只允许管理员访问，首次建屏时程序会请求提升驱动代理的权限。

## 更新

在设置的「关于与更新」页点击「检查更新」，或开启「启动时自动检查新版本」。程序默认查询本仓库的 GitHub Releases。
找到更高版本后，可以从程序内下载并重启更新。自动安装需要普通权限且 EXE 所在目录可写。

如果曾运行本地开发版 `1.3.0`，请先退出旧进程，再用发布版 `v1.0.0` 的 `snug.exe` 替换原文件。
自动更新只识别更高的版本号，无法用旧开发版切换到独立发布序列。

C# 版与 Rust 版各有自己的更新渠道。两版共用默认数据目录和开机启动任务，请退出其中一个版本后再运行另一个。

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

v1.0.3 本地版本采用手机触控布局：底部应用导航、多窗口选择及整页设置，
并修复 UU Qt 控制端识别、后台队列断管崩溃和撤屏失败后的善后。
桌面可运行 `snug.exe --preview` 查看只读界面；实际手机屏按物理短边统一缩放，
不会把 Windows 上的桌面字号直接缩小后塞到手机上。预览不能替代真实远控验收。

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

在 GitHub Actions 中运行 `Release`。工作流依据 `Cargo.toml` 的版本构建和打包，通过检查后创建 Release 草稿。
验收同一份 EXE 后，运行 `Publish verified draft` 校验并公开草稿。发布页提供单个含驱动的 EXE，以及应用内更新所需的 ZIP 和校验和。

本地等价流程与资产命名规则见 [docs/release.md](docs/release.md)。

## 文档

- [docs/acceptance.md](docs/acceptance.md) — 真机与更新验收
- [docs/release.md](docs/release.md) — 发版流程（版本号、门禁、资产名、更新源）
- [docs/real-machine-test.md](docs/real-machine-test.md) — 真机测试记录
- [docs/display-parity.md](docs/display-parity.md) — 虚拟屏链路与原版逐行对拍
- [docs/ui-parity.md](docs/ui-parity.md) — 界面与原版对照
- [docs/size-budget.md](docs/size-budget.md) — 体积预算与已做的取舍
- [docs/quality.md](docs/quality.md) — 质量门禁（rustc 零警告、clippy 怎么用）

## 许可

MIT，见 [LICENSE](LICENSE) 与 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
