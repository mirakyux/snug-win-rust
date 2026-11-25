//! 明暗主题（对应 C# v1.3.0 的 `Light` / `Dark` / `Auto` 三档）。
//!
//! C# 那边是三套 XAML 资源字典（`ThemeLight.xaml` / `ThemeDark.xaml` /
//! `DarkModern.xaml`），运行时换 `DynamicResource`。egui 这边等价物是
//! `Visuals` + 我们自己的一小份调色板：`Visuals` 管控件默认外观，调色板管界面里
//! 那些手写的颜色（资源库网格的格子、底部条、强调色）。
//!
//! `Auto` 跟随 Windows「设置 → 个性化 → 颜色 → 应用模式」，读的是
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize\AppsUseLightTheme`
//! （`snug_win::system_uses_light_theme`，读不到按浅色）。

use eframe::egui;
use snug_core::AppThemeMode;

/// 界面里手写颜色的来源。深色/浅色各一套，控件默认外观另外由 `Visuals` 管。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// 窗口底色（资源库列表、设置窗背景）。
    pub bg: egui::Color32,
    /// 卡片 / 图标格子的底色。
    pub card: egui::Color32,
    /// 悬停时的卡片底色。
    pub card_hover: egui::Color32,
    /// 正文色。
    pub text: egui::Color32,
    /// 次要文字（"N 个窗口"、说明行）。
    pub text_weak: egui::Color32,
    /// 强调色（可点的链接、当前页签）。
    pub accent: egui::Color32,
    /// 提示色（预览横幅、警告）。
    pub warn: egui::Color32,
    /// 底部指示条的底色。
    pub home_bar: egui::Color32,
    /// 是否深色（界面里少数几处要按这个分支）。
    pub dark: bool,
}

impl Palette {
    pub const DARK: Palette = Palette {
        bg: egui::Color32::from_rgb(23, 26, 32),
        card: egui::Color32::from_rgb(34, 38, 46),
        card_hover: egui::Color32::from_rgb(46, 52, 64),
        text: egui::Color32::from_rgb(238, 241, 247),
        text_weak: egui::Color32::from_rgb(170, 179, 194),
        accent: egui::Color32::from_rgb(168, 196, 255),
        warn: egui::Color32::from_rgb(255, 196, 120),
        home_bar: egui::Color32::from_rgb(34, 38, 46),
        dark: true,
    };

    pub const LIGHT: Palette = Palette {
        bg: egui::Color32::from_rgb(245, 246, 249),
        card: egui::Color32::from_rgb(255, 255, 255),
        card_hover: egui::Color32::from_rgb(233, 237, 244),
        text: egui::Color32::from_rgb(32, 38, 49),
        text_weak: egui::Color32::from_rgb(98, 107, 121),
        accent: egui::Color32::from_rgb(35, 91, 215),
        warn: egui::Color32::from_rgb(145, 78, 0),
        home_bar: egui::Color32::from_rgb(255, 255, 255),
        dark: false,
    };
}

/// 这一档主题最终是不是深色。`Auto` 去问系统。
pub fn resolve_dark(mode: AppThemeMode) -> bool {
    match mode {
        AppThemeMode::Light => false,
        AppThemeMode::Dark => true,
        AppThemeMode::Auto => !snug_win::system_uses_light_theme(),
    }
}

pub fn palette(dark: bool) -> Palette {
    if dark { Palette::DARK } else { Palette::LIGHT }
}

/// 把这一档主题应用到 egui（控件默认外观 + 圆角间距）。
pub fn apply(ctx: &egui::Context, dark: bool) {
    ctx.set_theme(if dark { egui::Theme::Dark } else { egui::Theme::Light });
    let mut visuals = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    let p = palette(dark);
    visuals.panel_fill = p.bg;
    visuals.window_fill = p.bg;
    visuals.extreme_bg_color = if dark {
        egui::Color32::from_rgb(16, 18, 21)
    } else {
        egui::Color32::from_rgb(238, 240, 245)
    };
    visuals.override_text_color = Some(p.text);
    // `.weak()` 也用实色，避免默认 alpha 混合后在卡片上低于可读对比度。
    visuals.weak_text_color = Some(p.text_weak);
    visuals.hyperlink_color = p.accent;
    visuals.warn_fg_color = p.warn;
    visuals.text_edit_bg_color = Some(p.card);
    let border = if dark {
        egui::Color32::from_rgb(58, 65, 79)
    } else {
        egui::Color32::from_rgb(221, 226, 235)
    };
    visuals.window_stroke = egui::Stroke::new(1.0_f32, border);
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, border);
    visuals.widgets.inactive.bg_fill = p.card;
    visuals.widgets.inactive.weak_bg_fill = p.card;
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, border);
    visuals.widgets.hovered.bg_fill = p.card_hover;
    visuals.widgets.hovered.weak_bg_fill = p.card_hover;
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, p.accent);
    // 选中态用强调色，和原版页签的蓝一致。
    visuals.selection.bg_fill = if dark {
        egui::Color32::from_rgb(41, 59, 92)
    } else {
        egui::Color32::from_rgb(232, 239, 255)
    };
    visuals.selection.stroke = egui::Stroke::new(1.0_f32, p.accent);
    ctx.set_visuals(visuals);
    // 圆角：原版卡片是 8~12px，egui 默认 2px 太硬。
    let mut style = (*ctx.style()).clone();
    style.spacing.interact_size = egui::vec2(48.0, 48.0);
    style.spacing.button_padding = egui::vec2(12.0, 10.0);
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.icon_width = 22.0;
    style.spacing.icon_width_inner = 14.0;
    style.text_styles.insert(egui::TextStyle::Body, egui::FontId::proportional(16.0));
    style.text_styles.insert(egui::TextStyle::Button, egui::FontId::proportional(16.0));
    style.text_styles.insert(egui::TextStyle::Small, egui::FontId::proportional(13.0));
    style.text_styles.insert(egui::TextStyle::Heading, egui::FontId::proportional(24.0));
    style.visuals.widgets.noninteractive.corner_radius = egui::CornerRadius::same(8);
    style.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(8);
    style.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(8);
    style.visuals.widgets.active.corner_radius = egui::CornerRadius::same(8);
    ctx.set_style(style);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_theme_keeps_the_same_visuals_on_subsequent_frames() {
        let ctx = egui::Context::default();
        for dark in [false, true, false] {
            apply(&ctx, dark);
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                assert_eq!(ctx.style().visuals.dark_mode, dark);
                assert_eq!(ctx.style().visuals.panel_fill, palette(dark).bg);
            });
        }
    }

    #[test]
    fn explicit_modes_do_not_consult_the_system() {
        assert!(!resolve_dark(AppThemeMode::Light));
        assert!(resolve_dark(AppThemeMode::Dark));
    }

    #[test]
    fn palettes_differ_in_contrast_direction() {
        // 深色底要暗、字要亮；浅色反过来。这条钉住"别把两套颜色抄反"。
        assert!(Palette::DARK.bg.r() < 64 && Palette::DARK.text.r() > 200);
        assert!(Palette::LIGHT.bg.r() > 200 && Palette::LIGHT.text.r() < 64);
        assert!(Palette::DARK.dark && !Palette::LIGHT.dark);
    }

    fn luminance(color: egui::Color32) -> f64 {
        let channel = |value: u8| {
            let value = f64::from(value) / 255.0;
            if value <= 0.04045 { value / 12.92 } else { ((value + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
    }

    #[test]
    fn both_themes_keep_body_and_secondary_text_readable_on_their_surfaces() {
        for palette in [Palette::LIGHT, Palette::DARK] {
            for foreground in [palette.text, palette.text_weak, palette.accent, palette.warn] {
                for background in [palette.bg, palette.card, palette.card_hover] {
                    let a = luminance(foreground);
                    let b = luminance(background);
                    let ratio = (a.max(b) + 0.05) / (a.min(b) + 0.05);
                    assert!(ratio >= 4.5, "{foreground:?} on {background:?}: {ratio:.2}");
                }
            }
        }
    }

    #[test]
    fn theme_gives_standard_buttons_and_checkboxes_touch_height() {
        let ctx = egui::Context::default();
        apply(&ctx, false);
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let mut checked = false;
                for response in [ui.button("设置"), ui.checkbox(&mut checked, "自动开屏")] {
                    assert!(response.rect.height() >= 48.0, "{:?}", response.rect);
                }
                assert_eq!(ui.visuals().weak_text_color(), Palette::LIGHT.text_weak);
            });
        });
    }
}
