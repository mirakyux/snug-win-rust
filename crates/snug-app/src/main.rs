//! snug —— SnugWin 的 Rust 重写（托盘程序 + 手机屏界面）。
//!
//! 当前阶段：核心逻辑层已开始移植，平台层与界面尚未接上。

fn main() {
    let fit = snug_core::ScreenFit::standard();
    println!("snug-rs (Rust 重写) 骨架");
    println!("  标准机型档位: {fit}, 基准宽度 {} 点", fit.base_width());
    println!("  物理像素(540 点): {:?}", fit.pixel_size(540));
    println!("  平台层已实现: {}", snug_win::implemented());
}
