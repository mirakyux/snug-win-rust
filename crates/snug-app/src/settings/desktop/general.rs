use super::super::*;
use snug_win::VddProtocol;
use snug_win::driver_health::PipeAccess;

impl SettingsWindow {
    pub(super) fn desktop_general(&mut self, ui: &mut egui::Ui, cfg: &mut AppConfig) {
        ui.horizontal_wrapped(|ui| {
            if ui
                .selectable_label(!self.show_diagnostics && !self.show_updates, "偏好")
                .clicked()
            {
                self.show_diagnostics = false;
                self.show_updates = false;
            }
            if ui
                .selectable_label(self.show_diagnostics, "驱动与日志")
                .clicked()
            {
                self.show_diagnostics = true;
                self.show_updates = false;
            }
            if ui
                .selectable_label(self.show_updates, "更新与关于")
                .clicked()
            {
                self.show_diagnostics = false;
                self.show_updates = true;
            }
        });
        ui.separator();

        if self.show_diagnostics {
            self.desktop_diagnostics(ui);
        } else if self.show_updates {
            self.desktop_updates(ui, cfg);
        } else {
            self.desktop_preferences(ui, cfg);
        }
    }

    fn desktop_preferences(&mut self, ui: &mut egui::Ui, cfg: &mut AppConfig) {
        let theme = match cfg.theme {
            AppThemeMode::Light => "浅色",
            AppThemeMode::Dark => "深色",
            AppThemeMode::Auto => "跟随系统",
        };
        egui::Grid::new("desktop-appearance")
            .num_columns(2)
            .spacing([20.0, 8.0])
            .show(ui, |ui| {
                ui.label("外观");
                egui::ComboBox::from_id_salt("desktop-theme")
                    .selected_text(theme)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut cfg.theme, AppThemeMode::Light, "浅色");
                        ui.selectable_value(&mut cfg.theme, AppThemeMode::Dark, "深色");
                        ui.selectable_value(&mut cfg.theme, AppThemeMode::Auto, "跟随系统");
                    });
                ui.end_row();

                ui.label("应用字号");
                egui::ComboBox::from_id_salt("desktop-zoom")
                    .selected_text(zoom_label(cfg.zoom_steps))
                    .show_ui(ui, |ui| {
                        for steps in 0..=4 {
                            ui.selectable_value(&mut cfg.zoom_steps, steps, zoom_label(steps));
                        }
                    })
                    .response
                    .on_hover_text("对支持缩放的手机、平板应用窗口生效");
                ui.end_row();
            });

        ui.separator();
        let mut enabled = self.status.autostart.unwrap_or(false);
        if ui
            .add_enabled(self.autostart_editable(), egui::Checkbox::new(&mut enabled, self.autostart_label()))
            .changed()
        {
            self.actions.push(SettingsAction::SetAutoStart(enabled));
        }
        ui.weak("登录后自动启动 Snug 并连接虚拟屏驱动。首次开启可能需要管理员确认。");
        self.show_autostart_error(ui);
        ui.add_space(6.0);
        ui.checkbox(&mut cfg.crossdesk_enabled, "启用 CrossDesk（实验性）");
        ui.weak("尚未经过真机远控测试。手机端请使用绝对鼠标。");
    }

    fn desktop_diagnostics(&mut self, ui: &mut egui::Ui) {
        ui.label(elevation_text(self.status.elevated));
        egui::Grid::new("desktop-driver-status")
            .num_columns(2)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                ui.label("驱动管道");
                ui.label(pipe_text(self.status.pipe_available));
                ui.end_row();
                if let Some(health) = &self.status.driver_health {
                    ui.label("驱动版本");
                    ui.label(match &health.version {
                        Some(version) => version.as_str(),
                        None if health.installed => "已安装，版本读取失败",
                        None => "未检测到设备",
                    });
                    ui.end_row();
                    ui.label("连接方式");
                    ui.label(match health.access {
                        PipeAccess::Direct(VddProtocol::V2) => "直接连接（双向协议）",
                        PipeAccess::Direct(VddProtocol::Legacy) => "直接连接（旧版协议）",
                        PipeAccess::RequiresElevation if self.status.driver_broker_connected => "已通过驱动代理连接",
                        PipeAccess::RequiresElevation => "需要管理员授权",
                        PipeAccess::ElevatedAccessDenied => "驱动拒绝已提权的 Snug 连接",
                        PipeAccess::Busy => "管道繁忙",
                        PipeAccess::Missing => "管道尚未出现",
                        PipeAccess::Unavailable => "管道暂不可访问",
                    });
                    ui.end_row();
                    if let Some(code) = health.problem_code {
                        ui.label("设备问题代码");
                        ui.label(code.to_string());
                        ui.end_row();
                    } else if health.started == Some(false) {
                        ui.label("设备状态");
                        ui.label("Windows 设备尚未启动");
                        ui.end_row();
                    }
                }
            });
        if let Some(result) = &self.status.driver_connection_result {
            ui.label(result);
        }
        ui.horizontal_wrapped(|ui| {
            let retry_label = if self.status.driver_reconnecting {
                "正在连接…"
            } else {
                "重试连接"
            };
            if ui
                .add_enabled(
                    !self.status.driver_reconnecting,
                    egui::Button::new(retry_label),
                )
                .clicked()
            {
                self.actions.push(SettingsAction::RetryDriverConnection);
            }
            if ui.button("安装或修复驱动").clicked() {
                self.actions.push(SettingsAction::InstallDriver);
            }
            if ui.button("打开日志目录").clicked() {
                self.actions.push(SettingsAction::OpenLogDirectory);
            }
        });

        if !self.status.elevated {
            ui.collapsing("高级与权限", |ui| {
                if ui.button("以管理员身份重启").clicked() {
                    self.actions.push(SettingsAction::RestartElevated);
                }
            });
        }
        ui.separator();
        ui.label("运行日志");
        let mut logs = self.logs.as_str();
        egui::ScrollArea::vertical()
            .id_salt("desktop-runtime-log")
            .stick_to_bottom(true)
            .max_height(176.0)
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut logs)
                        .id_salt("desktop-log-text")
                        .font(egui::TextStyle::Monospace)
                        .desired_rows(8)
                        .desired_width(ui.available_width()),
                );
            });
    }

    fn desktop_updates(&mut self, ui: &mut egui::Ui, cfg: &mut AppConfig) {
        ui.strong(version_line(
            &self.status.current_version,
            self.status.self_contained,
        ));
        ui.label(self.update.status_text(&self.status.current_version));
        if let Some(progress) = self.update.progress() {
            ui.add(egui::ProgressBar::new(progress).desired_width(ui.available_width().min(320.0)));
        }
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(self.update.check_enabled(), egui::Button::new("检查更新"))
                .clicked()
            {
                self.actions.push(SettingsAction::CheckUpdate);
            }
            if ui
                .add_enabled(
                    self.update.install_enabled(),
                    egui::Button::new("下载并重启更新"),
                )
                .clicked()
            {
                self.actions.push(SettingsAction::InstallUpdate);
            }
        });
        ui.checkbox(&mut cfg.auto_check_updates, "启动时自动检查新版本")
            .on_hover_text("自动下载前校对 SHA-256 校验和。");

        ui.separator();
        ui.strong("开源信息");
        ui.label("MIT License。参考 nicocart/snug-win 与 zephyr4123/snug。");
        ui.label("虚拟显示驱动采用 MolotovCherry/virtual-display-rs 0.3.1。");
        if ui.button("打开 GitHub 仓库").clicked() {
            self.actions.push(SettingsAction::OpenRepository);
        }
        ui.collapsing("随附字体许可与版权", |ui| {
            for (name, license) in [
                (
                    "Hack-Regular",
                    include_str!("../../../assets/fonts/licenses/Hack-Regular.txt"),
                ),
                (
                    "Ubuntu-Light",
                    include_str!("../../../assets/fonts/licenses/UFL.txt"),
                ),
                (
                    "Google Noto Emoji — Copyright and attribution",
                    include_str!("../../../assets/fonts/licenses/Noto-Emoji-attribution.txt"),
                ),
                (
                    "Google Noto Emoji — SIL Open Font License 1.1",
                    include_str!("../../../assets/fonts/licenses/OFL.txt"),
                ),
                (
                    "emoji-icon-font",
                    include_str!("../../../assets/fonts/licenses/emoji-icon-font-mit-license.txt"),
                ),
            ] {
                ui.collapsing(name, |ui| {
                    ui.add(egui::Label::new(egui::RichText::new(license).monospace()).wrap());
                });
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_startup_is_changed_by_one_control_in_preferences() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, false);
        let mut settings = SettingsWindow::new();
        let mut cfg = AppConfig::default();
        settings.select_tab(SettingsTab::Home);
        settings.open();

        let mut frame = |settings: &mut SettingsWindow, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(720.0, 620.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    settings.show_desktop(ctx, &mut cfg);
                },
            )
        };
        let button_pos = |output: &egui::FullOutput, label| {
            output.shapes.iter().find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == label => {
                    Some(text.galley.rect.translate(text.pos.to_vec2()).center())
                }
                _ => None,
            })
        };

        for enabled in [false, true] {
            settings.status.autostart = Some(enabled);
            settings.status.autostart_changing = false;
            let output = frame(&mut settings, Vec::new());
            assert!(settings.take_actions().is_empty());
            let pos = button_pos(&output, "登录后后台启动").expect("login startup control is visible");
            for pressed in [true, false] {
                frame(
                    &mut settings,
                    vec![
                        egui::Event::PointerMoved(pos),
                        egui::Event::PointerButton {
                            pos,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
            assert_eq!(
                settings.take_actions(),
                vec![SettingsAction::SetAutoStart(!enabled)],
            );
        }

        settings.status.autostart_changing = true;
        let output = frame(&mut settings, Vec::new());
        assert!(button_pos(&output, "正在保存登录自启设置…").is_some());
        assert!(!settings.autostart_editable());

        settings.select_tab(SettingsTab::Diagnostics);
        let output = frame(&mut settings, Vec::new());
        assert!(button_pos(&output, "登录后自动连接驱动").is_none());
    }
}
