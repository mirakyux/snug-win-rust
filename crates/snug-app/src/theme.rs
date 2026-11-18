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
        bg: egui::Color32::from_rgb(24, 26, 30),
        card: egui::Color32::from_rgb(46, 50, 58),
        card_hover: egui::Color32::from_rgb(58, 62, 72),
        text: egui::Color32::from_rgb(238, 238, 238),
        text_weak: egui::Color32::from_rgb(150, 152, 158),
        accent: egui::Color32::from_rgb(140, 200, 255),
        warn: egui::Color32::from_rgb(255, 196, 120),
        home_bar: egui::Color32::from_rgb(24, 26, 30),
        dark: true,
    };

    pub const LIGHT: Palette = Palette {
        bg: egui::Color32::from_rgb(246, 247, 250),
        card: egui::Color32::from_rgb(255, 255, 255),
        card_hover: egui::Color32::from_rgb(232, 236, 243),
        text: egui::Color32::from_rgb(28, 30, 36),
        text_weak: egui::Color32::from_rgb(108, 113, 124),
        accent: egui::Color32::from_rgb(37, 99, 235),
        warn: egui::Color32::from_rgb(176, 106, 0),
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
    // 选中态用强调色，和原版页签的蓝一致。
    visuals.selection.bg_fill = if dark {
        egui::Color32::from_rgb(40, 78, 120)
    } else {
        egui::Color32::from_rgb(206, 224, 255)
    };
    visuals.selection.stroke = egui::Stroke::new(1.0_f32, p.accent);
    ctx.set_visuals(visuals);
    // 圆角：原版卡片是 8~12px，egui 默认 2px 太硬。
    let mut style = (*ctx.style()).clone();
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
}
