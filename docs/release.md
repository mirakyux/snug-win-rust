# 发版

独立发布从 `v1.0.0` 开始。根 `Cargo.toml` 的 `[workspace.package] version` 同时决定 EXE 资源版本、
更新器比较用的版本号和默认发布标签。旧开发版 `1.3.0` 曾沿用 C# 上游的编号，需手动替换为 `v1.0.0` 的 EXE。

## 1. 定版本、构建、门禁、打包

```powershell
# 1) 改 Cargo.toml 的 [workspace.package] version
# 2) 编译前取官方驱动包（3 MB，核对 SHA-256；已存在且哈希正确就跳过）
pwsh -File scripts\fetch-driver.ps1

# 3) 构建（target\release\snug.exe 正在运行时占用文件，用独立目录）
cargo build --release -p snug-app --offline --target-dir target\release-check

# 4) 生成 dist\snug.exe、更新 ZIP 与 checksums.txt，并核对更新源
pwsh -File scripts\build-release.ps1 -Exe target\release-check\release\snug.exe -ReleasesRepo mirakyux/snug-win-rust

# 5) 核对版本、体积、清单、依赖、发行包和更新源
pwsh -File scripts\preflight.ps1 -Exe target\release-check\release\snug.exe -ReleasesRepo mirakyux/snug-win-rust
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
默认安装包来自 [virtual-display-rs v0.3.1](https://github.com/MolotovCherry/virtual-display-rs/releases/tag/v0.3.1)，
版本与三个哈希写死在 `scripts/fetch-driver.ps1` 和 `driver/Install-SnugDriver.ps1` 两处，
升级驱动要同时改这两处，更新 `driver.rs` 的固定哈希回归断言，并重跑真机验收。

仓库内的补丁版驱动另行构建和签名，见 [驱动构建与验收](../driver/README.md)。更换内嵌驱动时，需同时核对新版安装包的安装、升级和卸载行为，以及发布产物的签名与哈希。

`build-release.ps1` 会用 Rust 侧的 `sha256_hex` 再算一遍 exe 与 zip，和 `Get-FileHash`
比对——两套独立实现，发布链最容易在这里悄悄分叉。

## 2. 发布 GitHub Release

### 2a. 手动触发（默认路径）

在 GitHub 选择 **Actions → Release → Run workflow**。`tag` 留空时，工作流会在根 `Cargo.toml` 的版本前加 `v`，
例如 `v1.0.0`。若手动填写，标签必须和 `Cargo.toml` 的版本一致。工作流先创建草稿。

工作流（`.github/workflows/release.yml`）按顺序做：

1. `scripts/fetch-driver.ps1`（编译前下载官方驱动安装包，核对 SHA-256）；
2. 全仓 Debug 与 Release 两种 profile 的 `cargo test --workspace`；
3. 正确性/可疑用法 Clippy 门禁、显式管理员 staging ACL 测试，以及 `cargo build --release -p snug-app`；
4. `scripts/build-release.ps1`（`dist/` 里的单 exe、更新 zip 与 `checksums.txt`）；
5. `scripts/preflight.ps1`（版本 / 体积 / 清单 / 依赖 / 发行包结构与校验和）；
6. 用 `.github/release-notes-template.md` 生成说明，附上提交记录和校验和；
7. 在单独的写权限 job 建草稿并上传 `snug.exe`、`Snug-v<版本>-win-x64-selfcontained.zip` 与 `checksums.txt`。

下载草稿附件，在 Windows 上完成 `docs/acceptance.md`，记录实际 EXE 的 SHA-256。然后运行 **Publish verified draft**，填入草稿 tag 与该哈希。它验证草稿文件、两个校验和，以及 ZIP 内唯一 EXE 的内容，再公开同一份附件，**不会重新构建**。哈希不匹配、校验和缺失或附件已改变，均拒绝公开。

**只保留手动触发**：参考项目还支持推 tag 自动发，但本项目的发版前提是真机验收
（`docs/acceptance.md`）走完，一次 push 不该直接把包发出去。

### 2b. 本地打包（不发 release）

见第 1 节。直接交付用户只需 `dist/snug.exe`；手工发布到 GitHub 时同时上传
exe、自动更新 zip 与 `checksums.txt`。

### 2c. 命名规则

- 标签采用 `v<版本>` 格式，例如 `v1.0.0`。
- 单 exe 供直接下载，zip 与 `checksums.txt` 供自动更新，发布时三者都上传。
- **资产名不能改**。更新器按 `update::release_asset_name(tag, kind)` 精确匹配
  （如 `Snug-v1.0.0-win-x64-selfcontained.zip`），名字对不上就无法通过应用安装更新。

C# 版与 Rust 版各自维护版本号、更新仓库和资产命名。C# 版使用 `SnugWin-v…`，Rust 版使用 `Snug-v…`。

## 3. 检查 GitHub 更新

程序默认查询 [本仓库的最新正式发布](https://github.com/mirakyux/snug-win-rust/releases/latest)。
更新器比较正式发布的标签和当前 EXE 的版本号。草稿、预发布和单独推送的 Git 标签不会成为正式更新。

自定义构建可以用编译期环境变量 `SNUG_RELEASES_REPO` 指定另一个 `owner/name`。Release 工作流默认绑定当前仓库，
可用同名仓库变量覆盖。打包和预检需校验实际编入 EXE 的更新源；自定义构建时，将 `-ReleasesRepo` 改成对应的 `owner/name`。

## 4. 发版后自检

| 操作 | 期望 |
| --- | --- |
| 运行新版，在设置的「关于与更新」页点击「检查更新」 | 显示「已是最新版本」 |
| 在隔离目录运行使用同一更新源的更低版本 | 显示新版本号、更新说明及安装入口 |
| 断网后检查更新 | 显示检查失败，应用仍可操作 |

自动更新会下载 zip → 用 `checksums.txt` 校验 → 解压到 staging → 写
`apply-update.ps1`（等本进程退出 → 覆盖 → 重启 → 删临时目录）。
自动更新的下载、暂存、替换与重启都只在普通权限下执行，不提供管理员更新入口。暂存目录在当前 EXE 同级原子创建，避免跨卷复制进运行文件；既有哈希校验、原子替换和失败回滚保留。管理员兼容模式需先退出并普通启动再更新；受保护安装目录会明确报错，请手动替换 EXE。这样无需让高权限进程按用户可改的目标路径写入。

自动测试会在隔离目录中实际执行替换、校验失败、锁定、超时和回滚。真实发布文件的 UAC、受保护安装目录和驱动效果仍须按验收单验证。
