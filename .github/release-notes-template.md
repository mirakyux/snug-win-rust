## Snug v{{VERSION}}

Windows 上的 **网易 UU远程** 伴侣程序：在电脑上开一块竖直的虚拟手机屏，把任意窗口摆进手机屏，
手机上就像原生的手机应用一样点按操作。Rust 重写版，**单 exe、零运行时依赖**（不需要 .NET / VC++ 运行库）。

承接 [zephyr4123/snug](https://github.com/zephyr4123/snug)（macOS，Swift）的产品体验与状态机，
行为细节对齐 [nicocart/snug-win](https://github.com/nicocart/snug-win)（Windows，C#/WPF）v{{VERSION}}。
两者均为 MIT 许可。

### 下载

| 资产 | 说明 |
| --- | --- |
| `Snug-v{{VERSION}}-win-x64-selfcontained.zip` | 绿色便携版：解压得到单文件 `snug.exe`，无运行时依赖，双击即用（会请求管理员权限） |
| `checksums.txt` | 资产的 SHA-256 |

### 安装前提

本项目**不含**虚拟显示器驱动，需要先装一次
[virtual-display-rs](https://github.com/MolotovCherry/virtual-display-rs)（0.3.x）。
装好后 `\\.\pipe\virtualdisplaydriver` 存在，Snug 才能建屏。

### 本版变化

{{COMMITS}}

完整差异：{{COMPARE}}

### 已知限制

- 仅在 Windows 11 x64 + UU远程 实测；Windows 10 2004+ 理论可用但未逐一验证
- 依赖 virtual-display-rs 的命名管道 `\\.\pipe\virtualdisplaydriver`
- 多屏最多同时 4 块虚拟屏（手机 / 平板 / 电脑），同一时刻界面只画一块、顶部切换
- 换机型比例后，UU 侧需重选一次屏幕，点按位置才准确

### 致谢与许可

- 上游 macOS 原版：[zephyr4123/snug](https://github.com/zephyr4123/snug)
- 虚拟显示器驱动：[MolotovCherry/virtual-display-rs](https://github.com/MolotovCherry/virtual-display-rs)
- 本项目：MIT License，第三方声明见 `THIRD_PARTY_NOTICES.md`
