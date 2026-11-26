# Windows 字号输入结果修复计划

1. 先增加回归测试，区分完整输入、首次拒绝、部分输入和已执行一步后失焦/拒绝。
2. 保留 `WindowControl::zoom -> bool` 和 `WindowApi::send_ctrl_key`，增加结果明确的入口，供核心层保留不确定操作的恢复标记。
3. 检查 `SendInput` 返回的成功事件数量：完整计数为 `Applied`，零计数为 `NotApplied`，非零部分计数为 `Uncertain`；组合步骤只要可能已执行就不得退回 `NotApplied`。
4. 使用假窗口与假输入完成测试，不向真实应用发送按键；运行局部测试、格式检查和适用的静态检查。

## 约束与验收边界

- 修改范围仅为本文和 `crates/snug-win/src/window.rs`；核心层事务由主代理实现。
- 不增加依赖，不改驱动，不提交现有工作区改动。
- 完整 `SendInput` 计数证明事件已进入输入流，不证明目标应用已经按预期改变字号。跨进程相对快捷键没有应用级确认，崩溃恢复必须保留不确定性，不能自动重放。
- 官方语义依据：[Microsoft Learn 的 `SendInput` 文档](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-sendinput)（`winuser.h`），返回值章节和备注章节，检查于 2026-11-26。

## 手动恢复界面补充计划

1. 复用现有应用库通知行，显示字号操作中断后的不确定状态和明确恢复顺序。
2. 仅增加“已手动还原字号”动作，通过控制器转交当前屏幕的核心恢复入口；不提供“稍后”来清除该安全阻挡。
3. 在现有显示说明位置补充归属保护模式保留 Windows 当前缩放比例的提示，保持显式分辨率应用流程。
4. 在 `mobile_library.rs`、`controller.rs` 的新增方法和 `ui.rs` 动作分发局部内修改，保留现有工作区改动；运行可用的应用测试与静态检查。

## 验证结果

- `cargo test -p snug-win --lib --offline window::tests --target-dir target/zoom-outcome-20261126`：37 项通过。
- `cargo test -p snug-app --bin snug --offline mobile_library::tests --target-dir target/zoom-outcome-20261126`：13 项通过，涵盖新通知的明暗主题与横竖屏交互。
- `cargo test -p snug-app --bin snug --offline --target-dir target/zoom-outcome-20261126`：完整应用测试成功退出（128 项）。
- `cargo clippy` 对 `snug-win --lib` 和 `snug-app --bin snug` 的 `correctness`、`suspicious` 检查均成功退出，仍有原有风格类警告。
- 局部 `git diff --check` 通过；`rustfmt --emit stdout` 语法解析通过，没有全文件重排。
- 本次未向真实应用发送按键；输入失败用假实现覆盖。完整输入计数与应用实际字号的语义边界仍按前述约束处理。

## 集成审查追加计划

1. 在多屏共享记录测试中复现应用级字号输入部分成功后，同一次归还扫描仍消费来源记录的问题。
2. 先验证旧实现失败，再在应用级字号归还循环和后续归还入口增加局部阻断，保护等待手动确认的日志与来源。
3. 检查循环是否会继续清除其他共享债务；增加独立回归，不改变正常完整输入路径。
4. 使用现有假平台验证，不发送真实按键；通过定向测试及核心测试后交由主代理运行最终集成检查。

## 集成审查追加结果

- 新增 7 项回归，均先在缺陷实现上运行至断言失败，再修复生产路径。两个布局回归覆盖不确定输入后主动重新铺满、显式改比例；两个摆放回归覆盖旧记录移交后同次调用继续消费共享来源，以及显式拉入丢失未偿还的应用级字号债务。
- `take_back_returned` 在应用级归还循环后重新检查日志，循环本身一旦遇到未决日志就停止；`take_back` 的入口检查日志，且应用级字号债务未清零时保留共享来源。
- `refill` 和 `reshape` 在读取摆放快照或修改显示布局前检查日志，避免使用旧的内存字号和新几何覆盖来源。其他 `fill` 调用者 `place_prepared` 已有入口检查。
- `put_back_leftover` 在移交产生未决日志时返回阻挡结果；`place_prepared` 拒绝消费尚未偿还的应用级字号来源，等待正常归还扫描成功后再重试摆放。
- `cargo test -p snug-core --lib --offline --target-dir target/zoom-outcome-20261126`：340 项通过，0 失败；日志为 `target/zoom-core-integration-final-20261126.log`。
- `cargo clippy -p snug-core --lib --offline --target-dir target/zoom-outcome-20261126 -- -D clippy::correctness -D clippy::suspicious`：成功退出，仍有 16 项原有风格类警告；局部 `git diff --check` 通过。
- 旧实现红灯证据：`target/zoom-shared-pending-red-20261126.log`、`target/zoom-shared-rejected-red-20261126.log`、`target/zoom-pending-layout-red-20261126.log`、`target/zoom-leftover-placement-red-20261126.log`、`target/zoom-explicit-placement-red-20261126.log`。最后一项随后将空列表下标断言改成显式长度断言，验证意图相同。
- 本次没有增加依赖或重构账本格式，没有向真实应用发按键；完整工作区 Debug、Release 与发布构建由主代理在同一最终源码状态下另行验收。
