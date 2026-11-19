## Snug v{{VERSION}}

Windows 上的 **网易 UU远程** 伴侣程序：在电脑上开一块竖直的虚拟手机屏，把任意窗口摆进手机屏，
手机上就像原生的手机应用一样点按操作。Rust 重写版，**单 exe、零运行时依赖**（不需要 .NET / VC++ 运行库）。

承接 [zephyr4123/snug](https://github.com/zephyr4123/snug)（macOS，Swift）的产品体验与状态机，
行为细节对齐 [nicocart/snug-win](https://github.com/nicocart/snug-win)（Windows，C#/WPF）v{{VERSION}}。
两者均为 MIT 许可。

### 下载

| 资产 | 说明 |
| --- | --- |
| `Snug-v{{VERSION}}-win-x64-selfcontained.zip` | 绿色便携版：解压即用，无运行时依赖（不需要 .NET / VC++ 运行库），已内置虚拟显示器驱动安装包 |
| `checksums.txt` | 资产的 SHA-256 |

解压后：

```text
snug.exe                                         单文件程序
driver\Install-SnugDriver.ps1                    一键安装/修复驱动
driver\install-driver.cmd                        上面的批处理入口
driver\virtual-desktop-driver-installer-x64.zip  官方驱动安装包（SHA-256 已核对）
```

### 安装一步

```text
双击 snug.exe，允许管理员权限（UAC）。剩下的它自己来：
启动时发现没有虚拟显示器驱动，就在后台用包里的官方安装包装好（SHA-256 与证书指纹都核对），
装完直接建手机屏。已经装过同款驱动会原样保留，不覆盖你自装的版本。
```

设置页 → 驱动 里的「安装驱动」按钮留着做**修复**用（比如驱动装了一半、或想重装）。

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
