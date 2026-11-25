use super::super::{
    AppConfig, PHONE_SCREEN_MODES, PhoneChoice, PhoneScreenSpec, RESOLUTION_PRESETS, ResolutionDraft,
    ScreenConfig, ScreenResolution, SettingsAction, SettingsWindow, egui, phone_screen_mode_label,
    screen_spec_text,
};
use snug_core::{DeviceKind, ScreenOrientation};

impl SettingsWindow {
    pub(super) fn desktop_screens(&mut self, ui: &mut egui::Ui, cfg: &mut AppConfig) {
        if self.adding_screen {
            self.desktop_add_screen(ui, cfg);
            return;
        }

        let screens = cfg.effective_screens();
        self.desktop_screen_controls(ui, cfg);
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            ui.strong("屏幕列表");
            if ui
                .add_enabled(
                    screens.len() < ScreenConfig::LIMIT,
                    egui::Button::new("添加屏幕"),
                )
                .clicked()
            {
                self.adding_screen = true;
            }
        });
        if screens.is_empty() {
            ui.weak("还没有屏幕");
        }
        for screen in &screens {
            ui.push_id(("desktop-screen-row", &screen.id), |ui| {
                ui.horizontal_wrapped(|ui| {
                    let selected = self.screen_detail.as_deref() == Some(&screen.id);
                    if ui.selectable_label(selected, &screen.name).clicked() && !selected {
                        self.screen_detail = Some(screen.id.clone());
                        self.screen_name_draft.clone_from(&screen.name);
                        self.confirm_delete = false;
                    }
                    let status = if screen.enabled {
                        "已启用"
                    } else {
                        "已关闭"
                    };
                    ui.weak(format!(
                        "{} · {} · {status}",
                        screen.device.kind.display_name(),
                        screen_spec_text(screen)
                    ));
                });
            });
        }

        if let Some(screen) = screens
            .into_iter()
            .find(|screen| self.screen_detail.as_deref() == Some(&screen.id))
        {
            ui.separator();
            self.desktop_edit_screen(ui, cfg, screen);
        } else {
            self.screen_detail = None;
            self.confirm_delete = false;
        }
    }

    fn desktop_screen_controls(&mut self, ui: &mut egui::Ui, cfg: &mut AppConfig) {
        ui.horizontal_wrapped(|ui| {
            ui.strong(&self.status.status_summary);
            if ui.button("预览界面").clicked() {
                self.actions.push(SettingsAction::OpenPreview);
            }
            if ui.button("虚拟屏投影").clicked() {
                self.actions.push(SettingsAction::OpenProjection);
            }
            if !self.status.pipe_available && ui.button("安装 / 修复驱动").clicked() {
                self.actions.push(SettingsAction::InstallDriver);
            }
            if self.status.phone_screen_ready {
                if ui.button("收起手机屏").clicked() {
                    self.confirm_dismiss = true;
                }
            } else {
                self.confirm_dismiss = false;
                if self.status.can_reenter && ui.button("重新进入手机屏").clicked() {
                    self.actions.push(SettingsAction::ReenterScreen);
                }
            }
        });
        if !self.status.pipe_available {
            ui.weak("虚拟显示器驱动未就绪。");
        }
        if self.confirm_dismiss {
            ui.horizontal_wrapped(|ui| {
                ui.label("收起手机屏并还原窗口？");
                if ui.button("确认收起").clicked() {
                    self.actions.push(SettingsAction::DismissScreen);
                    self.confirm_dismiss = false;
                }
                if ui.button("取消").clicked() {
                    self.confirm_dismiss = false;
                }
            });
        }
        let current_mode = cfg.phone_screen_mode();
        let mut mode = current_mode;
        ui.horizontal_wrapped(|ui| {
            ui.label("手机屏显示");
            egui::ComboBox::from_id_salt("desktop-phone-screen-mode")
                .selected_text(phone_screen_mode_label(mode))
                .show_ui(ui, |ui| {
                    for option in PHONE_SCREEN_MODES {
                        ui.selectable_value(&mut mode, option, phone_screen_mode_label(option));
                    }
                });
        });
        if mode != current_mode {
            cfg.set_phone_screen_mode(mode);
        }
    }

    fn desktop_edit_screen(
        &mut self,
        ui: &mut egui::Ui,
        cfg: &mut AppConfig,
        mut screen: ScreenConfig,
    ) {
        let id = screen.id.clone();
        let mut changed = false;
        ui.push_id(("desktop-screen-editor", &id), |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("名称");
                let name = ui.add(
                    egui::TextEdit::singleline(&mut self.screen_name_draft).desired_width(180.0),
                );
                let trimmed = self.screen_name_draft.trim();
                let enter =
                    name.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
                let can_save = !trimmed.is_empty() && trimmed != screen.name;
                if ui
                    .add_enabled(can_save, egui::Button::new("保存名称"))
                    .clicked()
                    || can_save && enter
                {
                    screen.name = trimmed.to_string();
                    changed = true;
                }
            });
            changed |= ui.checkbox(&mut screen.enabled, "启用这块屏").changed();

            let draft = self
                .resolution_drafts
                .entry(id.clone())
                .or_insert_with(|| ResolutionDraft::from_screen(&screen));
            desktop_resolution(ui, draft);
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(
                        draft.resolution().is_some(),
                        egui::Button::new("应用分辨率"),
                    )
                    .clicked()
                {
                    if draft.apply(&mut screen) {
                        changed = true;
                    } else {
                        self.actions
                            .push(SettingsAction::RetryResolution(id.clone()));
                    }
                }
                if ui.button("恢复默认").clicked() {
                    let fit = snug_core::ScreenPlan::fit_of(&screen);
                    let (width, height) = fit.pixel_size(fit.base_width());
                    *draft = ResolutionDraft::from_resolution(ScreenResolution::default_for(
                        orientation_for(width, height),
                    ));
                }
            });
            if self.status.failed_resolution_screen.as_deref() == Some(&id) {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    "Windows 未确认目标分辨率，请重新应用。",
                );
            }
        });
        if changed {
            *cfg = cfg.with_screen_updated(&id, |_| screen.clone());
        }

        ui.add_space(4.0);
        if self.confirm_delete {
            ui.label("删除这块屏？屏幕会撤除，应用窗口归位。");
            ui.horizontal_wrapped(|ui| {
                if ui.button("确认删除").clicked() {
                    *cfg = cfg.with_screen_removed(&id);
                    self.resolution_drafts.remove(&id);
                    self.screen_detail = None;
                    self.confirm_delete = false;
                }
                if ui.button("取消").clicked() {
                    self.confirm_delete = false;
                }
            });
        } else if ui.button("删除这块屏").clicked() {
            self.confirm_delete = true;
        }
    }

    fn desktop_add_screen(&mut self, ui: &mut egui::Ui, cfg: &mut AppConfig) {
        ui.strong("添加屏幕");
        let previous_kind = self.new_screen_kind;
        ui.horizontal_wrapped(|ui| {
            ui.label("设备");
            egui::ComboBox::from_id_salt("desktop-new-device")
                .selected_text(self.new_screen_kind.display_name())
                .show_ui(ui, |ui| {
                    for kind in [DeviceKind::Phone, DeviceKind::Tablet, DeviceKind::Computer] {
                        ui.selectable_value(&mut self.new_screen_kind, kind, kind.display_name());
                    }
                });
        });
        if previous_kind != self.new_screen_kind
            && !self.new_screen_resolution.custom
            && self.new_screen_resolution.resolution().is_some_and(|size| {
                size == ScreenResolution::default_for(previous_kind.usual_orientation())
            })
        {
            self.new_screen_resolution = ResolutionDraft::from_resolution(
                ScreenResolution::default_for(self.new_screen_kind.usual_orientation()),
            );
        }
        ui.horizontal_wrapped(|ui| {
            ui.label("名称");
            ui.add(
                egui::TextEdit::singleline(&mut self.new_screen_name)
                    .desired_width(180.0)
                    .hint_text("可选"),
            );
        });
        desktop_resolution(ui, &mut self.new_screen_resolution);
        let resolution = self.new_screen_resolution.resolution();
        let mut screens = cfg.effective_screens();
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    screens.len() < ScreenConfig::LIMIT && resolution.is_some(),
                    egui::Button::new("确认添加"),
                )
                .clicked()
                && let Some(resolution) = resolution
            {
                let name = self.new_screen_name.trim();
                screens.push(
                    ScreenConfig {
                        id: String::new(),
                        orientation: orientation_for(resolution.width, resolution.height),
                        device: PhoneChoice {
                            model: None,
                            screen: PhoneScreenSpec::new(resolution.width, resolution.height, None),
                            kind: self.new_screen_kind,
                        },
                        enabled: true,
                        name: if name.is_empty() {
                            format!("屏幕 {}", screens.len() + 1)
                        } else {
                            name.to_string()
                        },
                        resolution: Some(resolution),
                    }
                    .sanitize(),
                );
                cfg.screens = Some(screens);
                *cfg = cfg.clone().sanitize();
                self.new_screen_name.clear();
                self.adding_screen = false;
            }
            if ui.button("取消").clicked() {
                self.adding_screen = false;
            }
        });
    }
}

fn desktop_resolution(ui: &mut egui::Ui, draft: &mut ResolutionDraft) {
    ui.horizontal_wrapped(|ui| {
        ui.label("分辨率");
        egui::ComboBox::from_id_salt("desktop-resolution")
            .selected_text(draft.label())
            .show_ui(ui, |ui| {
                for (index, &(_, _, label)) in RESOLUTION_PRESETS.iter().enumerate() {
                    if ui.selectable_label(draft.label() == label, label).clicked() {
                        draft.select_preset(index);
                    }
                }
                if ui.selectable_label(draft.custom, "自定义").clicked() {
                    draft.custom = true;
                }
            });
    });
    if draft.custom {
        ui.horizontal_wrapped(|ui| {
            ui.label("宽");
            ui.add(egui::TextEdit::singleline(&mut draft.width).desired_width(65.0));
            ui.label("高");
            ui.add(egui::TextEdit::singleline(&mut draft.height).desired_width(65.0));
        });
        if draft.resolution().is_none() {
            ui.colored_label(
                ui.visuals().error_fg_color,
                "宽高须为 320–8192 之间的偶数。",
            );
        }
    }
}

fn orientation_for(width: i32, height: i32) -> ScreenOrientation {
    if width > height {
        ScreenOrientation::Landscape
    } else {
        ScreenOrientation::Portrait
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_resolution_changes_are_committed_only_on_apply() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, false);
        let mut cfg = AppConfig::default().sanitize();
        let screen = &mut cfg.screens.as_mut().unwrap()[0];
        screen.resolution = Some(ScreenResolution {
            width: 720,
            height: 1280,
        });
        let mut settings = SettingsWindow::new();
        settings.open();
        settings.screen_detail = Some(screen.id.clone());
        settings.screen_name_draft = screen.name.clone();

        fn frame(
            ctx: &egui::Context,
            settings: &mut SettingsWindow,
            cfg: &mut AppConfig,
            events: Vec<egui::Event>,
        ) -> egui::FullOutput {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(720.0, 620.0),
                )),
                events,
                ..Default::default()
            };
            ctx.run(input, |ctx| {
                settings.show_desktop(ctx, cfg);
            })
        }

        for (text, expected) in [
            (
                "恢复默认",
                ScreenResolution {
                    width: 720,
                    height: 1280,
                },
            ),
            ("应用分辨率", ScreenResolution::DEFAULT),
        ] {
            let output = frame(&ctx, &mut settings, &mut cfg, Vec::new());
            let pos = output
                .shapes
                .iter()
                .find_map(|clipped| match &clipped.shape {
                    egui::Shape::Text(shape) if shape.galley.text() == text => {
                        Some(shape.galley.rect.translate(shape.pos.to_vec2()).center())
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("missing desktop button {text}"));
            for pressed in [true, false] {
                frame(
                    &ctx,
                    &mut settings,
                    &mut cfg,
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
            assert_eq!(cfg.effective_screens()[0].resolution, Some(expected));
        }
    }
}
