//! 手机屏上的「App 资源库」界面（egui 实现）。
//!
//! 对应 C# 版 `SnugWin.App.PhoneHomeWindow`：顶部搜索、运行中/全部两页、
//! 底部指示条（返回资源库 / 相机键）。当前阶段是骨架 + 真实数据接口占位。

use eframe::egui;

use snug_core::{AppConfig, Paths, PhoneCatalog};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeTab {
    Running,
    All,
}

pub struct PhoneHomeApp {
    pub config: AppConfig,
    pub catalog: Option<PhoneCatalog>,
    pub paths: Paths,
    /// 虚拟屏驱动管道是否就绪（平台层尚未接通时只用于界面提示）。
    pub pipe_ready: bool,
    pub tab: HomeTab,
    pub search: String,
    /// 手机屏的逻辑宽度（点），用于把界面元素按屏宽等比放大。
    pub screen_width: f32,
}

impl PhoneHomeApp {
    pub fn new(
        config: AppConfig,
        catalog: Option<PhoneCatalog>,
        paths: Paths,
        pipe_ready: bool,
        screen_width: f32,
    ) -> Self {
        Self {
            config,
            catalog,
            paths,
            pipe_ready,
            tab: HomeTab::Running,
            search: String::new(),
            screen_width,
        }
    }

    /// 底部指示条高度，规则与上游一致：28 点 × (屏宽 / 540)。
    pub fn home_bar_height(&self) -> f32 {
        (28.0 * self.screen_width / 540.0).round()
    }

    /// 当前使用中的机型名（顶部标题栏用）。
    pub fn phone_label(&self) -> String {
        self.config
            .phone
            .model
            .clone()
            .unwrap_or_else(|| "自定义分辨率".to_string())
    }
}

impl eframe::App for PhoneHomeApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let file = self.paths.config_file();
        let json = self.config.to_json();
        match std::fs::write(&file, json) {
            Ok(()) => crate::info!("配置已保存 {}", file.display()),
            Err(err) => crate::warn!("配置保存失败 {}：{err}", file.display()),
        }
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::TopBottomPanel::top("search")
            .frame(egui::Frame::default().inner_margin(egui::Margin::symmetric(12, 8)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("‹");
                    ui.strong(self.phone_label());
                    if !self.pipe_ready {
                        ui.label(egui::RichText::new("驱动未就绪").color(egui::Color32::LIGHT_RED));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.search)
                                .hint_text("搜索 App 与窗口")
                                .desired_width(ui.available_width() - 40.0),
                        );
                    });
                });
            });

        egui::TopBottomPanel::bottom("home_bar")
            .exact_height(self.home_bar_height().max(20.0))
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    if self.config.screenshot {
                        ui.label("📷");
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("全部").clicked() {
                            self.tab = HomeTab::All;
                        }
                        if ui.small_button("运行中").clicked() {
                            self.tab = HomeTab::Running;
                        }
                    });
                });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            match self.tab {
                HomeTab::Running => {
                    ui.add_space(6.0);
                    if ui
                        .add_sized(
                            [ui.available_width(), 60.0],
                            egui::Button::new(egui::RichText::new("暂无运行中的窗口").size(15.0)),
                        )
                        .clicked()
                    {
                        // 平台层接上后：拉出 App 列表并放置窗口。
                    }
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new("窗口会在这里显示，点一下即铺满手机屏").weak());
                }
                HomeTab::All => {
                    ui.add_space(6.0);
                    let models = self
                        .catalog
                        .as_ref()
                        .map(|c| c.brands.len())
                        .unwrap_or(0);
                    if let Some(catalog) = &self.catalog {
                        let query = snug_core::phone_models::normalize(&self.search);
                        let brands: Vec<&snug_core::phone_models::Brand> = if query.is_empty() {
                            catalog.brands.iter().collect()
                        } else {
                            catalog
                                .brands
                                .iter()
                                .filter(|b| snug_core::phone_models::normalize(&b.name).contains(&query))
                                .collect()
                        };
                        if brands.is_empty() {
                            ui.label(egui::RichText::new("没有匹配的机型").weak());
                        }
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            for brand in brands {
                                ui.collapsing(format!("{}（{} 款）", brand.name, brand.models.len()), |ui| {
                                    for model in &brand.models {
                                        ui.label(format!("{}  {}×{}", model.name, model.screen.width, model.screen.height));
                                    }
                                });
                            }
                        });
                    } else {
                        ui.label(format!("机型库未加载（{models}）"));
                    }
                }
            }
        });
    }
}
