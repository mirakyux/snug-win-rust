## Snug v{{VERSION}}

Windows 上的 **网易 UU远程** 伴侣程序：在电脑上开一块竖直的虚拟手机屏，把任意窗口摆进手机屏，
手机上就像原生的手机应用一样点按操作。Rust 重写版，**单 exe、零运行时依赖**（不需要 .NET / VC++ 运行库）。

承接 [zephyr4123/snug](https://github.com/zephyr4123/snug)（macOS，Swift）的产品体验与状态机，
行为细节对齐 [nicocart/snug-win](https://github.com/nicocart/snug-win)（Windows，C#/WPF）v{{VERSION}}。
两者均为 MIT 许可。

### 下载

| 资产 | 说明 |
| --- | --- |
| `snug.exe` | 直接下载运行的单文件，已内嵌官方虚拟显示器驱动安装包与安装脚本，无需旁边放任何文件 |
| `Snug-v{{VERSION}}-win-x64-selfcontained.zip` | 自动更新器的兼容资产；包内只有同一个 `snug.exe` |
| `checksums.txt` | 资产的 SHA-256 |

直接下载 `snug.exe`；也可解压更新包得到同一个文件：

```text
snug.exe   单文件程序，已内嵌官方驱动安装包与安装脚本
```

### 安装一步

```text
双击 snug.exe，允许管理员权限（UAC）。剩下的它自己来：
启动时发现没有虚拟显示器驱动，就在后台释放内嵌的官方安装包并装好（SHA-256 与证书指纹都核对），
装完直接建手机屏。已经装过同款驱动会原样保留，不覆盖你自装的版本。
```

设置页 → 驱动 里的「安装驱动」按钮留着做**修复**用（比如驱动装了一半、或想重装）。

### 本版变化

{{COMMITS}}

完整差异：{{COMPARE}}

### 已知限制

- 仅在 Windows 11 x64 + UU远程 实测；Windows 10 2004+ 理论可用但未逐一验证
- 依赖 virtual-display-rs 的命名管道 `\\.\pipe\virtualdisplaydriver`
- 最多配置 4 块屏（手机 / 平板 / 电脑），同一时刻只建顶部选中的一块
- 换机型比例后，UU 侧需重选一次屏幕，点按位置才准确

### 致谢与许可

- 上游 macOS 原版：[zephyr4123/snug](https://github.com/zephyr4123/snug)
- 虚拟显示器驱动：[MolotovCherry/virtual-display-rs](https://github.com/MolotovCherry/virtual-display-rs)
- 本项目：MIT License，第三方声明见 `THIRD_PARTY_NOTICES.md`
