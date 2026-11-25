use super::{AppConfig, SettingsTab, SettingsWindow, egui};

mod general;
mod screens;

impl SettingsWindow {
    pub(super) fn desktop_layout(&mut self, ctx: &egui::Context, cfg: &mut AppConfig) {
        let palette = crate::theme::palette(ctx.style().visuals.dark_mode);
        egui::SidePanel::left("desktop-settings-navigation")
            .exact_width(144.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(palette.card)
                    .inner_margin(egui::Margin::same(12)),
            )
            .show(ctx, |ui| {
                compact_controls(ui);
                for tab in SettingsTab::ALL {
                    let button = egui::Button::new(tab.nav_title())
                        .selected(self.tab == tab)
                        .min_size(egui::vec2(ui.available_width(), 30.0));
                    if ui.add(button).clicked() && self.tab != tab {
                        self.select_tab(tab);
                    }
                }
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(palette.bg)
                    .inner_margin(egui::Margin::same(18)),
            )
            .show(ctx, |ui| {
                compact_controls(ui);
                ui.heading(self.tab.page_title());
                ui.add_space(6.0);
                let mut scroll = egui::ScrollArea::vertical()
                    .id_salt(("desktop-settings-page", self.tab.index()))
                    .auto_shrink([false, false]);
                if std::mem::take(&mut self.reset_scroll) {
                    scroll = scroll.vertical_scroll_offset(0.0);
                }
                scroll.show(ui, |ui| match self.tab {
                    SettingsTab::Screens => self.desktop_screens(ui, cfg),
                    SettingsTab::Gestures => self.desktop_gestures(ui, cfg),
                    SettingsTab::Home | SettingsTab::Diagnostics | SettingsTab::About => {
                        self.desktop_general(ui, cfg);
                    }
                });
            });
    }

    fn desktop_gestures(&mut self, ui: &mut egui::Ui, cfg: &mut AppConfig) {
        ui.checkbox(&mut cfg.gestures, "翻译手机屏手势");
        ui.add_enabled_ui(cfg.gestures, |ui| {
            ui.indent("desktop-gesture-options", |ui| {
                ui.checkbox(&mut cfg.gesture_scroll, "单指滑动与惯性滚动");
                ui.checkbox(&mut cfg.gesture_long_press_right_click, "长按松手触发右键");
                ui.checkbox(&mut cfg.copy_after_selection, "划选后显示复制、粘贴和删除");
                ui.checkbox(&mut cfg.gestures_injected_only, "仅处理远控注入的输入");
            });
        });
        ui.add_space(8.0);
        ui.separator();
        ui.checkbox(&mut cfg.screenshot, "手机屏底栏显示截图按钮");
    }
}

fn compact_controls(ui: &mut egui::Ui) {
    let style = ui.style_mut();
    style.spacing.interact_size = egui::vec2(32.0, 24.0);
    style.spacing.button_padding = egui::vec2(8.0, 4.0);
    style.spacing.item_spacing = egui::vec2(8.0, 7.0);
    style.spacing.icon_width = 16.0;
    style.spacing.icon_width_inner = 12.0;
    for (kind, size) in [
        (egui::TextStyle::Body, 14.0),
        (egui::TextStyle::Button, 14.0),
        (egui::TextStyle::Small, 12.0),
    ] {
        style
            .text_styles
            .insert(kind, egui::FontId::proportional(size));
    }
}
