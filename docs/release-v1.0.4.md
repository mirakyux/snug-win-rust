# v1.0.4 发布记录

2026-11-22 18:22（Asia/Shanghai）公开 [Snug v1.0.4](https://github.com/mirakyux/snug-win-rust/releases/tag/v1.0.4)。

源码提交与标签 `v1.0.4` 均指向 `1121324351335ce56d3c3493ff229b7a794311e5`。后续文档提交只补充本记录与准确字节数，没有更改运行代码或发布附件。

## 变化与简化

- `snug-app` 的 `settings.rs`、`main.rs`、`ui.rs`、`controller.rs`、`bubble.rs`：移除主流程的机型选择及必选条件，按分辨率草稿应用设置；持续原生唤醒，发布快照后再通知，离线时后台停钩子并清缓存。
- `snug-core` 的 `config.rs`、`screen_fit.rs`、`screen_plan.rs`、`desk/engine.rs`、`desk/model.rs`、`desk/test_support.rs`：迁移并保留旧屏精确像素，固定显示模式；应用导航不再扩缩屏，关闭按钮查询脱离串行操作队列。
- `snug-win` 的 `display.rs`、`touch.rs`、`window.rs`、`lib.rs` 及只读 `display-state` 示例：复用显示器快照、精确模式、物理鼠标快速放行、异步原生重绘和焦点处理。
- 同步 Cargo 版本、README、CHANGELOG、验收清单、显示差异说明、发布模板及三份本机修复记录。

复用原有状态机、心跳与控件，删除自动扩屏和输入钩子内的窗口/文件探测，没有新增依赖。

## 验证与发布链路

- 本地 Debug / Release 各 612 项通过，两种 profile 的 Clippy correctness / suspicious 门禁通过；Release 无 rustc 警告，18 项打包检查通过。
- 用户确认固定分辨率后的流畅度和字号合适，并确认最终文件两次真实连入、退出后，托盘状态正常变为“关”。
- [Release CI](https://github.com/mirakyux/snug-win-rust/actions/runs/45011491014) 对上述源码提交全部通过，包括 Debug、Release、Clippy、管理员 staging ACL、构建及打包检查。
- CI 创建草稿后，附件替换为已通过本机真实验收的原文件组合，同时重写发布说明中的校验和。
- [Publish verified draft](https://github.com/mirakyux/snug-win-rust/actions/runs/45012096071) 校验验收 EXE 哈希、两个校验和及 ZIP 内唯一 EXE，成功公开同一份附件，没有重新编译。
- 公开后在不携带凭据的请求中下载全部三项资产：裸 EXE 和 ZIP 内 EXE 均匹配验收哈希；确认 `releases/latest` 为 v1.0.4，远程标签指向上述源码提交。

## 公开文件

| 文件 | 字节数 | SHA-256 |
| --- | ---: | --- |
| `snug.exe` | 8,823,296 | `f583b81b282756b2141704cafc77808255fd66f415427c05d213ae045f987d26` |
| `Snug-v1.0.4-win-x64-selfcontained.zip` | 6,203,035 | `c58ab167a4d7e22354bb06b45cb21e6d87a51cc4af26429befc0d62d02ab7f73` |
| `checksums.txt` | 179 | `6e6129428fc55dd030b133d0cc14eb540b8c11c45f8a09058c1c03f0dbd76794` |

完整证据位于本机 `target/offline-status-20261122/`，不将原始运行日志和认证数据提交到公开仓库。

## 剩余限制

旧电脑屏迁移可能改变 DPI，便笺等单实例应用首次启动仍可能占用串行队列；两项均已写入公开发布说明。当前真机验收不覆盖所有 Windows、显卡和显示驱动组合。

仓库仍存在已有 rustfmt 差异，本轮未整仓格式化；`git diff --check` 通过。
