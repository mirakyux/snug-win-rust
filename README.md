# snug-rs

SnugWin 的 Rust 重写。原项目是 [zephyr4123/snug](https://github.com/zephyr4123/snug)（macOS，Swift）
与社区移植版 [nicocart/snug-win](https://github.com/nicocart/snug-win)（Windows，C# / .NET 10 / WPF），
两者均为 MIT 许可。本仓库保留原作者的版权声明，只把 Windows 侧实现换成 Rust，
目标是一个**单 exe、零运行时依赖**的版本。

当前版本 **1.3.0**，对齐 C# 原版 v1.3.0 的功能面。改动明细见 [CHANGELOG.md](CHANGELOG.md)。

## 下载

到 [Releases](../../releases) 下最新的 `Snug-v1.3.0-win-x64-selfcontained.zip`
（6.19 MiB），解压得到：

```text
snug.exe                                    # 6.64 MiB 单文件，不需要 .NET / VC++ 运行库
driver\Install-SnugDriver.ps1               # 一键安装/修复虚拟屏驱动
driver\install-driver.cmd                   # 上面的批处理入口（双击即可）
driver\virtual-desktop-driver-installer-x64.zip   # 官方驱动安装包（SHA-256 已核对）
```

双击 `snug.exe`（允许 UAC），如果还没装过虚拟屏驱动，在设置页点「安装驱动」即可——
安装脚本会核对安装包与证书的 SHA-256，用的是
[virtual-display-rs](https://github.com/MolotovCherry/virtual-display-rs) 0.3.1。

## 为什么重写

- `.NET` 桌面 UI（WPF）不支持 NativeAOT，自包含发布要 180 MB 磁盘；Rust 版目标是 10 MB 级别。
  实际产物：**6.64 MiB 单 exe**（发布 zip 3.08 MiB）。
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

全仓 **490 条单测**（core 221 / win 217 / app 48 / 几何对拍 3 / SHA-256 对拍 1），
`rustc` 零警告是硬门禁，见 [docs/quality.md](docs/quality.md)。

与原版的差别与取舍理由集中在 [`docs/upstream-watch.md`](docs/upstream-watch.md)；
多屏形态（单窗口 + 顶部切换）见 [`docs/multi-screen.md`](docs/multi-screen.md)；
线程与锁纪律见 [`docs/desk-threading.md`](docs/desk-threading.md)。

## 构建

需要 Rust（stable 1.89+，edition 2024）+ MSVC 工具链 + Windows SDK。

```powershell
cargo test --workspace
cargo build --release -p snug-app
pwsh -File scripts/build-release.ps1    # 打 dist/ 里的 zip + checksums.txt
```

`target\release\snug.exe` 正在运行时无法被覆盖，验证构建可用独立目录：

```powershell
cargo build --release -p snug-app --offline --target-dir target\release-check
```

## 发版

**手动触发**：GitHub → Actions → `Release` → Run workflow。
工作流会用根 `Cargo.toml` 的版本推导 tag（也可手填），依次跑全仓单测、
release 构建、`scripts/preflight.ps1` 静态门禁、`scripts/build-release.ps1` 打包，
然后建 GitHub Release 并上传 zip 与 `checksums.txt`。

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
