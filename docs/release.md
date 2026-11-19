# 发版

版本号只有一个来源：根 `Cargo.toml` 的 `[workspace.package] version`。
build.rs 把它写进 exe 的 `FILEVERSION` / 字符串表，`update::APP_VERSION` 也取它——
所以**改了它，exe 资源版本、更新器比较用的版本一起变**，不需要改第二处。

## 1. 定版本、构建、门禁、打包

```powershell
# 1) 改 Cargo.toml 的 [workspace.package] version
# 2) 构建（target\release\snug.exe 正在运行时占用文件，用独立目录）
cargo build --release -p snug-app --offline --target-dir target\release-check

# 3) 静态门禁：版本/体积/清单/依赖/发行包，全 PASS 再往下
pwsh -File scripts\preflight.ps1 -Exe target\release-check\release\snug.exe

# 4) 打 dist\ 里的 zip + checksums.txt（脚本自己读 exe 的版本资源）
pwsh -File scripts\build-release.ps1 -Exe target\release-check\release\snug.exe
```

产物：

```text
dist\Snug-v<版本>-win-x64-selfcontained.zip   根目录只有 snug.exe
dist\checksums.txt                            <sha256>  <zip 文件名>（ASCII + LF + 无 BOM）
```

`build-release.ps1` 会用 Rust 侧的 `sha256_hex` 再算一遍 exe 与 zip，和 `Get-FileHash`
比对——两套独立实现，发布链最容易在这里悄悄分叉。

## 2. 发 GitHub release

### 2a. 手动触发（默认路径）

GitHub → **Actions → Release → Run workflow**，参数：

| 参数 | 说明 |
| --- | --- |
| `tag` | 留空则用根 `Cargo.toml` 的版本推导成 `v1.3.0`；手填则必须与 `Cargo.toml` 的版本一致，否则工作流直接失败（资产名按 exe 版本生成，对不上就发错包） |
| `draft` | 勾上则先建草稿，检查无误再手动发布 |

工作流（`.github/workflows/release.yml`）按顺序做：

1. 全仓 `cargo test --workspace`；
2. `cargo build --release -p snug-app`；
3. `scripts/preflight.ps1`（版本 / 体积 / 清单 / 依赖 / 发行包）；
4. `scripts/build-release.ps1`（`dist/` 里的 zip + `checksums.txt`）；
5. 用 `.github/release-notes-template.md` 生成说明（含自上一个 tag 以来的提交、校验和）；
6. 建 Release 并上传 `Snug-v<版本>-win-x64-selfcontained.zip` 与 `checksums.txt`。

**只保留手动触发**：参考项目还支持推 tag 自动发，但本项目的发版前提是真机验收
（`docs/acceptance.md`）走完，一次 push 不该直接把包发出去。

更新源由**仓库变量** `SNUG_RELEASES_REPO`（`owner/name`）决定，没配就不注入，
构建出来的 exe"检查更新"不可用。注意第 3 节那条：**先有 release 再配**。

### 2b. 本地打包（不发 release）

见第 1 节；产物在 `dist/`，手工上传时同样要传 zip 与 `checksums.txt` 两个资产。

### 2c. 命名规则

- tag：`v<版本>`（`v1.3.0`）。更新器解析 tag 时 `normalize()` 会去掉 `v` 前缀与后缀。
- 两个资产都要传：zip 与 `checksums.txt`。
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
这条链只在真机点过才知道通不通，见 `docs/acceptance.md` 之外的"仍未知"清单。
