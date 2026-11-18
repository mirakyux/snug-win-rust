# snug-rs

SnugWin 的 Rust 重写。原项目是 [zephyr4123/snug](https://github.com/zephyr4123/snug)（macOS，Swift）
与社区移植版 [nicocart/snug-win](https://github.com/nicocart/snug-win)（Windows，C# / .NET 10 / WPF），
两者均为 MIT 许可。本仓库保留原作者的版权声明，只把 Windows 侧实现换成 Rust，
目标是一个**单 exe、零运行时依赖**的版本。

## 为什么重写

- `.NET` 桌面 UI（WPF）不支持 NativeAOT，自包含发布要 180 MB 磁盘；Rust 版目标是 10 MB 级别。
- 原版最值钱的是行为细节（窗口归位、字号记忆、手势判定的各种边界），
  这些都在 `SnugWin.Core` 里，可以逐文件对照移植，并有单测兜底。
- 平台层用 `windows` crate 直接调 Win32 / COM，省掉一层托管封装。

## 目录

```text
crates/
  snug-core/   纯逻辑层，零 Win32 依赖（geometry / screen_fit / touch / scrolling /
               phone_models / config / record / left_behind / crop / zoom / store / paths）
  snug-win/    Windows 平台层（尚未实现）：虚拟屏 IPC、CCD、DWM 搬窗、底层鼠标钩子、
               UIA、shell 图标、截图、UU 连接探测
  snug-app/    托盘程序与手机屏界面
assets/
  phone-models.json   机型库（17 品牌 / 约 800 款），与 C# 版共用
references/           上游源码的浅克隆，仅供对照，已在 .gitignore 中
```

## 移植进度

| 模块 | C# 源 | 状态 |
|---|---|---|
| 几何 / 屏幕适配 | `Geometry` `ScreenFit` | ✅ 已移植 + 单测 |
| 手势状态机 | `TouchTranslator` `Scrolling` | ✅ 已移植 + 单测 |
| 机型库 | `PhoneModels` | ✅ 已移植 + 单测（含真实 assets 校验） |
| 配置 / 路径 | `AppConfig` `SnugPaths` | ✅ 已移植 + 单测 |
| 记录 / 存储 | `PhoneRecord` `JsonFileStore` `LeftBehind` | ✅ 已移植 + 单测 |
| 裁剪 / 字号 | `CropGeometry` `ZoomBook` | ✅ 已移植 + 单测 |
| 窗口会话状态机 | `PhoneDesk`（1349 行） | ⬜ 待移植 |
| 拼音搜索 | `PinyinSearch` | ⬜ 待移植 |
| 应用枚举 / 会话判定 | `Apps` `Session/*` `ControllerTable` | ⬜ 待移植 |
| 平台层 | `SnugWin.Windows` 全部 | ⬜ 待移植 |
| 界面 | `PhoneHomeWindow` 等 WPF | ⬜ 待实现（egui） |

## 构建

需要 Rust（stable）+ MSVC 工具链 + Windows SDK。

```powershell
cargo test --workspace
cargo run -p snug-app
```

## 许可

MIT，见 [LICENSE](LICENSE) 与 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
