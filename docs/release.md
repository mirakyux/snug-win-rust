# 发版

版本号只有一个来源：根 `Cargo.toml` 的 `[workspace.package] version`。
build.rs 把它写进 exe 的 `FILEVERSION` / 字符串表，`update::APP_VERSION` 也取它——
所以**改了它，exe 资源版本、更新器比较用的版本一起变**，不需要改第二处。

## 1. 定版本、构建、门禁、打包

```powershell
# 1) 改 Cargo.toml 的 [workspace.package] version
# 2) 编译前取官方驱动包（3 MB，核对 SHA-256；已存在且哈希正确就跳过）
pwsh -File scripts\fetch-driver.ps1

# 3) 构建（target\release\snug.exe 正在运行时占用文件，用独立目录）
cargo build --release -p snug-app --offline --target-dir target\release-check

# 4) 生成 dist\snug.exe、更新 zip 与 checksums.txt（读 exe 的版本资源）
pwsh -File scripts\build-release.ps1 -Exe target\release-check\release\snug.exe

# 5) 静态门禁：版本/体积/清单/依赖/发行包，全 PASS 再往下
pwsh -File scripts\preflight.ps1 -Exe target\release-check\release\snug.exe
```

驱动 ZIP 必须在编译前就绪，Rust 会把它和安装脚本原样嵌入 exe。
打包必须在门禁前完成，门禁会核对 `dist` 里的文件和校验和。

产物：

```text
dist\snug.exe                                用户直接运行的单文件，内含驱动
dist\Snug-v<版本>-win-x64-selfcontained.zip    自动更新资产，根目录仅 snug.exe
dist\checksums.txt                           exe 与 zip 的 SHA-256（ASCII + LF + 无 BOM）
```

自动更新 zip 内仅保留一个条目，兼容更新器原有的资产名和根目录约定：

```text
snug.exe
```

日常交付只需 `snug.exe`。驱动包和带 BOM 的安装脚本已嵌入程序，缺少驱动时在后台
释放到独占临时目录并调用原安装脚本；正常结束后清理临时目录。
设置页的安装/修复也使用同一内嵌包。机型库同时内嵌，无需带 `assets\`。
安装包来自 [virtual-display-rs](https://github.com/MolotovCherry/virtual-display-rs) 0.3.1，
版本与三个哈希写死在 `scripts/fetch-driver.ps1` 和 `driver/Install-SnugDriver.ps1` 两处，
升级驱动要同时改这两处，更新 `driver.rs` 的固定哈希回归断言，并重跑真机验收。

`build-release.ps1` 会用 Rust 侧的 `sha256_hex` 再算一遍 exe 与 zip，和 `Get-FileHash`
比对——两套独立实现，发布链最容易在这里悄悄分叉。

## 2. 发 GitHub release

### 2a. 手动触发（默认路径）

GitHub → **Actions → Release → Run workflow**，参数：

| 参数 | 说明 |
| --- | --- |
| `tag` | 留空则用根 `Cargo.toml` 的版本推导成 `v1.3.0`；手填则必须与 `Cargo.toml` 的版本一致，否则工作流直接失败（资产名按 exe 版本生成，对不上就发错包） |
| 草稿 | Release 工作流始终创建草稿，不直接公开 |

工作流（`.github/workflows/release.yml`）按顺序做：

1. `scripts/fetch-driver.ps1`（编译前下载官方驱动安装包，核对 SHA-256）；
2. 全仓 Debug 与 Release 两种 profile 的 `cargo test --workspace`；
3. 正确性/可疑用法 Clippy 门禁、显式管理员 staging ACL 测试，以及 `cargo build --release -p snug-app`；
4. `scripts/build-release.ps1`（`dist/` 里的单 exe、更新 zip 与 `checksums.txt`）；
5. `scripts/preflight.ps1`（版本 / 体积 / 清单 / 依赖 / 发行包结构与校验和）；
6. 用 `.github/release-notes-template.md` 生成说明（含自上一个 tag 以来的提交、校验和）；
7. 在单独的写权限 job 建草稿并上传 `snug.exe`、`Snug-v<版本>-win-x64-selfcontained.zip` 与 `checksums.txt`。

下载草稿附件，在 Windows 上完成 `docs/acceptance.md`，记录实际 EXE 的 SHA-256。然后运行 **Publish verified draft**，填入草稿 tag 与该哈希。它验证草稿文件、两个校验和，以及 ZIP 内唯一 EXE 的内容，再公开同一份附件，**不会重新构建**。哈希不匹配、校验和缺失或附件已改变，均拒绝公开。

**只保留手动触发**：参考项目还支持推 tag 自动发，但本项目的发版前提是真机验收
（`docs/acceptance.md`）走完，一次 push 不该直接把包发出去。

更新源由**仓库变量** `SNUG_RELEASES_REPO`（`owner/name`）决定，没配就不注入，
构建出来的 exe"检查更新"不可用。注意第 3 节那条：**先有 release 再配**。

### 2b. 本地打包（不发 release）

见第 1 节。直接交付用户只需 `dist/snug.exe`；手工发布到 GitHub 时同时上传
exe、自动更新 zip 与 `checksums.txt`。

### 2c. 命名规则

- tag：`v<版本>`（`v1.3.0`）。更新器解析 tag 时 `normalize()` 会去掉 `v` 前缀与后缀。
- 单 exe 供直接下载，zip 与 `checksums.txt` 供自动更新，发布时三者都上传。
- **资产名不能改**。更新器按 `update::release_asset_name(tag, kind)` 精确匹配
  （`Snug-v1.3.0-win-x64-selfcontained.zip`），名字对不上就当"这个 tag 没有我们的包"。

C# 原版的资产名是 `SnugWin-v…`，我们的是 `Snug-v…`：两版可以同机、同号共存，
更新器不会把对方当成自己的包。

## 3. 更新源（`SNUG_RELEASES_REPO`）

更新器**不会猜仓库**：仓库坐标来自编译期环境变量，没配时
`AppUpdateService::is_configured()` 为 `false`，"检查更新"直接不可用，一个请求都不发。

```powershell
$env:SNUG_RELEASES_REPO = "mirakyux/snug-win-rust"
cargo build --release -p snug-app --offline
```

想让它对本地所有构建都生效，可以写进 `.cargo/config.toml`：

```toml
[env]
SNUG_RELEASES_REPO = "mirakyux/snug-win-rust"
```

两点注意：

- **先有 release 再配**。仓库里还没有任何 release 时配了它，"检查更新"会报
  "无法获取最新版本信息"（`UpdateError`），比按钮不可用更让人困惑。
- 想临时不发更新，就不带这个变量构建；源码不用改。

## 4. 发版后自检

不需要真的点"更新"：

| 做法 | 期望 |
| --- | --- |
| 装上新版，设置 → 关于与更新 → 检查更新 | 显示"已是最新版本" |
| 把 `[workspace.package] version` 临时改成更小的号重编一次 | 显示新版本号与更新说明；**不要点更新**（会覆盖正在跑的 exe） |
| 断网后点检查更新 | 提示"无法获取最新版本信息"，不崩溃 |

自动更新会下载 zip → 用 `checksums.txt` 校验 → 解压到 staging → 写
`apply-update.ps1`（等本进程退出 → 覆盖 → 重启 → 删临时目录）。
自动更新的下载、暂存、替换与重启都只在普通权限下执行，不提供管理员更新入口。暂存目录在当前 EXE 同级原子创建，避免跨卷复制进运行文件；既有哈希校验、原子替换和失败回滚保留。管理员兼容模式需先退出并普通启动再更新；受保护安装目录会明确报错，请手动替换 EXE。这样无需让高权限进程按用户可改的目标路径写入。

自动测试会在隔离目录中实际执行替换、校验失败、锁定、超时和回滚。真实发布文件的 UAC、受保护安装目录和驱动效果仍须按验收单验证。
