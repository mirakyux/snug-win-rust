//! C# ↔ Rust 几何对拍。
//!
//! 真值由 `tools/refdump/geometry`（独立 dotnet 工程，直接编译参考版 C# 源码）生成，
//! 落到 `tools/geometry-ref/{screen_fit,crop,placement}.tsv`。这里读 TSV 逐条断言。
//!
//! 为什么要有这层：`screen_fit` / `crop` / `placement` 的边界（四舍五入、夹取、
//! 容差、1 像素）最容易“自己写测试自己过”——测试和实现是同一个人的两个假设。
//! 跟原版 C# 对拍才能证明移植没走样。
//!
//! 生成命令（在仓库根目录，需要 .NET 10 SDK）：
//!
//! ```text
//! dotnet run --project tools/refdump/geometry -c Release -- tools/geometry-ref
//! ```
//!
//! 生成物没提交时这组测试会打印提示并跳过（不假装通过）；提交后每次 `cargo test`
//! 都会真跑。placement 那组只验证“扩展样式”和“物理矩形取整”，因为
//! `SetWindowPos` 的 flags / z-order 在 C# 里没有可观察的纯函数出口。

use std::path::PathBuf;

use snug_core::crop::{self, CropHandle};
use snug_core::screen_fit::{PhoneSpec, ScreenFit};
use snug_core::{Point, Rect, Size};
use snug_win::placement;

const EPS: f64 = 1e-6;

fn reference_file(name: &str) -> Option<String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/geometry-ref")
        .join(name);
    match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(_) => {
            eprintln!(
                "对拍跳过：{} 不存在。先跑 `dotnet run --project tools/refdump/geometry -c Release -- tools/geometry-ref`",
                path.display()
            );
            None
        }
    }
}

fn number(field: &str) -> f64 {
    field.parse::<f64>().unwrap_or_else(|_| panic!("不是数字：{field:?}"))
}

fn integer(field: &str) -> i64 {
    field.parse::<i64>().unwrap_or_else(|_| panic!("不是整数：{field:?}"))
}

fn flag(field: &str) -> bool {
    match field {
        "1" => true,
        "0" => false,
        other => panic!("不是布尔：{other:?}"),
    }
}

fn close(actual: f64, expected: f64, what: &str) {
    let tolerance = EPS * expected.abs().max(1.0);
    assert!(
        (actual - expected).abs() <= tolerance,
        "{what}：Rust {actual} vs C# {expected}"
    );
}

fn rect(fields: &[&str]) -> Rect {
    Rect::new(number(fields[0]), number(fields[1]), number(fields[2]), number(fields[3]))
}

fn assert_rect(actual: Rect, fields: &[&str], what: &str) {
    for (index, value) in [actual.x, actual.y, actual.width, actual.height].iter().enumerate() {
        close(*value, number(fields[index]), &format!("{what}[{index}]"));
    }
}

fn crop_handle(index: i64) -> CropHandle {
    match index {
        0 => CropHandle::TopLeft,
        1 => CropHandle::Top,
        2 => CropHandle::TopRight,
        3 => CropHandle::Left,
        4 => CropHandle::Right,
        5 => CropHandle::BottomLeft,
        6 => CropHandle::Bottom,
        7 => CropHandle::BottomRight,
        other => panic!("未知的 CropHandle：{other}"),
    }
}

#[test]
fn screen_fit_matches_the_csharp_reference() {
    let Some(text) = reference_file("screen_fit.tsv") else { return };
    let mut checked = 0usize;

    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let fields: Vec<&str> = line.split('\t').collect();
        match fields[0] {
            "FIT" => {
                let points = if fields[3].is_empty() { None } else { Some(number(fields[3])) };
                let fit = ScreenFit::from_phone(&PhoneSpec::new(
                    number(fields[1]),
                    number(fields[2]),
                    points,
                ));
                close(fit.ratio(), number(fields[4]), "ratio");
                assert_eq!(fit.base_width(), integer(fields[5]) as i32, "base_width");
                let widths = fit.widths().iter().map(i32::to_string).collect::<Vec<_>>().join(",");
                assert_eq!(widths, fields[6], "widths");
            }
            "RATIO" => {
                let fit = ScreenFit::new(number(fields[1]));
                close(fit.ratio(), number(fields[2]), "ratio（夹取后）");
                assert_eq!(fit.base_width(), integer(fields[3]) as i32, "base_width");
                let widths = fit.widths().iter().map(i32::to_string).collect::<Vec<_>>().join(",");
                assert_eq!(widths, fields[4], "widths");
            }
            "SIZE" => {
                let points = if fields[3].is_empty() { None } else { Some(number(fields[3])) };
                let fit = ScreenFit::from_phone(&PhoneSpec::new(
                    number(fields[1]),
                    number(fields[2]),
                    points,
                ));
                let size = fit.size(integer(fields[4]) as i32);
                close(size.width, number(fields[5]), "size.width");
                close(size.height, number(fields[6]), "size.height");
            }
            "PIXEL" => {
                let points = if fields[3].is_empty() { None } else { Some(number(fields[3])) };
                let fit = ScreenFit::from_phone(&PhoneSpec::new(
                    number(fields[1]),
                    number(fields[2]),
                    points,
                ));
                let pixels = fit.pixel_size(integer(fields[4]) as i32);
                assert_eq!(
                    pixels,
                    (integer(fields[5]) as i32, integer(fields[6]) as i32),
                    "pixel_size({})",
                    fields[4]
                );
            }
            "FITTO" => {
                let points = if fields[3].is_empty() { None } else { Some(number(fields[3])) };
                let fit = ScreenFit::from_phone(&PhoneSpec::new(
                    number(fields[1]),
                    number(fields[2]),
                    points,
                ));
                let width = fit.width_to_fit(
                    Size::new(number(fields[4]), number(fields[5])),
                    Size::new(number(fields[6]), number(fields[7])),
                    integer(fields[8]) as i32,
                );
                assert_eq!(width, integer(fields[9]) as i32, "width_to_fit {line}");
            }
            "BAR" => {
                let screen_width = number(fields[1]);
                close(ScreenFit::scale(screen_width), number(fields[2]), "scale");
                close(
                    ScreenFit::home_bar_height(screen_width),
                    number(fields[3]),
                    "home_bar_height",
                );
            }
            other => panic!("未知记录：{other}"),
        }
        checked += 1;
    }

    assert!(checked > 50, "对拍记录太少：{checked}");
}

#[test]
fn crop_matches_the_csharp_reference() {
    let Some(text) = reference_file("crop.tsv") else { return };
    let mut checked = 0usize;

    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let f: Vec<&str> = line.split('\t').collect();
        match f[0] {
            "FITTED" => {
                let actual = crop::fitted_rect(Size::new(number(f[1]), number(f[2])), rect(&f[3..7]));
                assert_rect(actual, &f[7..11], "fitted_rect");
            }
            "UNIT" => {
                let actual = crop::unit_rect(rect(&f[1..5]), rect(&f[5..9]));
                assert_rect(actual, &f[9..13], "unit_rect");
            }
            "RECT" => {
                let actual = crop::rect_from_unit(rect(&f[1..5]), rect(&f[5..9]));
                assert_rect(actual, &f[9..13], "rect_from_unit");
            }
            "PIXEL" => {
                let actual = crop::pixel_rect(rect(&f[1..5]), Size::new(number(f[5]), number(f[6])));
                assert_rect(actual, &f[7..11], "pixel_rect");
            }
            "HANDLE" => {
                let actual = crop::handle_at(
                    Point::new(number(f[1]), number(f[2])),
                    rect(&f[3..7]),
                    number(f[7]),
                );
                let expected = if f[8] == "none" { None } else { Some(crop_handle(integer(f[8]))) };
                assert_eq!(actual, expected, "handle_at {line}");
            }
            "RESIZED" => {
                let actual = crop::resized(
                    rect(&f[1..5]),
                    crop_handle(integer(f[5])),
                    number(f[6]),
                    number(f[7]),
                    rect(&f[8..12]),
                    number(f[12]),
                );
                assert_rect(actual, &f[13..17], "resized");
            }
            other => panic!("未知记录：{other}"),
        }
        checked += 1;
    }

    assert!(checked > 40, "对拍记录太少：{checked}");
}

#[test]
fn placement_matches_the_csharp_reference() {
    let Some(text) = reference_file("placement.tsv") else { return };
    let mut styles = 0usize;
    let mut rects = 0usize;

    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let f: Vec<&str> = line.split('\t').collect();
        match f[0] {
            "STYLE" => {
                let actual = placement::desired_ex_style(
                    integer(f[1]) as i32,
                    flag(f[3]),
                    flag(f[4]),
                );
                assert_eq!(actual, integer(f[5]) as i32, "desired_ex_style {line}");
                styles += 1;
            }
            "RECT" => {
                let actual = placement::placement_rect(rect(&f[1..5]));
                assert_eq!(
                    actual,
                    (
                        integer(f[5]) as i32,
                        integer(f[6]) as i32,
                        integer(f[7]) as i32,
                        integer(f[8]) as i32,
                    ),
                    "placement_rect {line}"
                );
                rects += 1;
            }
            other => panic!("未知记录：{other}"),
        }
    }

    assert!(styles >= 40 && rects >= 40, "对拍记录太少：{styles} styles / {rects} rects");
}
