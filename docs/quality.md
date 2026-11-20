# 代码质量门禁

## 硬门禁：`rustc` 零警告

```powershell
pwsh -File scripts\fetch-driver.ps1  # 内嵌驱动必须在编译前就绪
cargo test --workspace --offline     # 全部测试通过、0 rustc warning；以当前运行摘要为准
cargo test --workspace --release --offline  # Release 同样必须全部通过
cargo build --release -p snug-app --offline
```

编译器警告一条都不许留。历史上这里踩过：`#[allow(dead_code)]` 掩盖了生产路径误调
测试辅助函数的隐患，最后是用 `#[cfg(test)]` 把 `idle()` / `drain()` 关进测试才根治
（`8f9c169`）。所以**不用 `allow` 让警告消失**，改代码或改可见性。

2026-11-19 在普通权限终端验证时，bin 单测仍带管理员清单，直接执行会报 Windows 错误 740。
当前 GUI 清单已改为 `asInvoker`，新构建的测试不再要求提升权限。以下兼容层仅用于旧缓存 harness；Release 的 Test 步骤保留这一兼容设置，它不会给普通进程提权：

```powershell
cmd /c 'set "__COMPAT_LAYER=RunAsInvoker" && cargo test --workspace --offline'
```

仓库现有部分 Rust 文件存在 `rustfmt --check --edition 2024` 格式差异。
修改代码时应对新增或触及的文件单独检查；不要用整仓格式化掩盖无关的大 diff。

## 参考门禁：`clippy`

```powershell
cargo clippy --workspace --all-targets --offline
cargo clippy --workspace --all-targets --offline -- -D clippy::correctness -D clippy::suspicious
```

**不把所有风格警告设为发布失败**，原因如下；正确性和可疑用法类别仍必须通过：

- 剩下的绝大多数是风格偏好，按建议改会**降低**可读性。典型是
  `type_complexity` 指着 `Option<Arc<dyn Fn(&DeskState) + Send + Sync>>` 这种回调类型——
  抽成 type alias 只是换个地方看同一串东西；`field_reassign_with_default` 指着 Win32 的
  `DEVMODEW` / `BITMAPINFO` 逐字段赋值，那是这个平台最自然的写法。
- 还有一批集中在测试代码里（`assert!(x == false)`、`Default::default()` 后逐字段赋值），
  表达的是"这里就是具体值"，改成 lint 喜欢的样子反而更难对拍 C#。
- 把这一堆 `allow` 掉等于关掉 clippy，不如老实说它只作参考。

**但它的真发现要修**，这一轮修掉的三类：

| 发现 | 问题 | 修法 |
| --- | --- | --- |
| `incompatible_msrv` | 声明 `rust-version = 1.85`，但单实例锁用了 `File::try_lock`（1.89 稳定）——别人按 1.85 装不上 | 提到 **1.89**，并写明原因 |
| `if_same_then_else`（两处） | `Math.Round` 的银行家舍入在 `display.rs` 与 `screenshot.rs` 各抄了一遍，两个分支返回同值 | 合并条件（语义不变），并让 `screenshot` re-export `display` 的实现，只留一份 |
| `items_after_test_module` 等结构项 | 目前无害 | 记着，动那个文件时顺手挪 |

Clippy 风格建议仍作参考；正确性和可疑用法类别升级为硬门禁，不通过就停止发布。测试与编译的 rustc warning 仍按上面的硬门禁执行。

## 权限边界与更新事务测试

普通权限测试包含真实的隔离 named pipe 往返、对端身份拒绝、帧大小与超时限制；不请求 UAC，不发送驱动命令。
ACL 测试在独占临时夹具上应用生产 DACL，读取实际继承结果，并在普通令牌下验证创建、覆盖与重设权限均被拒绝。
更新事务测试在独占临时目录执行生产 PowerShell 模板，覆盖替换成功、哈希错误、文件锁定、启动失败回滚与父进程超时；只启动复制的系统 `where.exe`，不更新当前 Snug。
生产下载与执行入口都拒绝管理员令牌，EXE 同级暂存脚本只由普通权限进程消费；受保护安装目录要求手动替换。管理员驱动安装仍使用 System32 的原子受限目录，不能与普通权限更新暂存混用。

受保护根目录的原子创建与新文件 ACL 继承测试默认 ignored，需在管理员终端单独执行；Release CI 明确运行这一步，不静默跳过：

```powershell
cargo test -p snug-win --lib --offline system_paths::tests::elevated_atomic_staging_inherits_production_acl -- --ignored --exact
```

该测试只创建并清理本次独占的 System32 子目录，不安装驱动、不建屏。本轮本地进程为普通权限，未执行这一项。

只读能力探测可在普通权限终端执行：

```powershell
cargo run -p snug-win --example standard-capabilities --offline
```

2026-11-20 本机结果：当前 CCD 配置验证返回 0；直接打开驱动管道返回错误 5（拒绝访问）。探测使用 `SDC_VALIDATE`，未应用显示配置、未发送驱动命令。这支持保留受限的管理员 VDD 代理，不能替代真实建屏与撤屏验收。

## 手工门禁：发版前

```powershell
pwsh -File scripts\preflight.ps1 -Exe target\release-check\release\snug.exe
```

版本/体积/清单/依赖/发行包，见 `docs/release.md`。行为验收见 `docs/acceptance.md`。

Release 工作流只创建草稿。公开发布通过独立的 `Publish verified draft` 工作流，提供完成真机验收的 `snug.exe` SHA-256；流程重新下载并校验草稿 EXE、ZIP 和校验和，再公开原草稿，绝不重新编译。构建与测试 job 只有仓库读取权限，写权限仅用于上传草稿和最终公开，不运行仓库构建脚本。哈希是验收声明与文件的绑定，不替代真实操作验收。
