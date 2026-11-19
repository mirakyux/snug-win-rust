# 代码质量门禁

## 硬门禁：`rustc` 零警告

```powershell
cargo test --workspace --offline     # 486 绿、0 warning
cargo build --release -p snug-app --offline
```

编译器警告一条都不许留。历史上这里踩过：`#[allow(dead_code)]` 掩盖了生产路径误调
测试辅助函数的隐患，最后是用 `#[cfg(test)]` 把 `idle()` / `drain()` 关进测试才根治
（`8f9c169`）。所以**不用 `allow` 让警告消失**，改代码或改可见性。

## 参考门禁：`clippy`

```powershell
cargo clippy --workspace --all-targets --offline
```

**它不当发版门禁**，原因说清楚而不是假装干净：

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

改完这两处后 `cargo test --workspace` 仍 486 绿、0 warning。

## 手工门禁：发版前

```powershell
pwsh -File scripts\preflight.ps1 -Exe target\release-check\release\snug.exe
```

版本/体积/清单/依赖/发行包，见 `docs/release.md`。行为验收见 `docs/acceptance.md`。
