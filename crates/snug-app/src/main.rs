//! snug —— SnugWin 的 Rust 重写（托盘程序 + 手机屏界面）。
//!
//! 阶段：核心逻辑层与虚拟屏管道已移植；显示配置、搬窗、钩子等平台层由 `snug-win` 逐步补齐。
//! 当前这个二进制已经能：加载配置与机型库、单实例、写日志、把手机屏界面按机型比例显示出来。

mod controller;
mod logging;
mod settings;
mod tray;
mod ui;

use std::fs::File;

use eframe::egui;
use snug_core::{AppConfig, Paths, PhoneCatalog, ScreenFit};

fn main() -> eframe::Result<()> {
    let paths = Paths::default();
    if let Err(err) = paths.ensure_dirs() {
        eprintln!("无法创建数据目录 {}：{err}", paths.data_dir().display());
    }
    logging::init(paths.log_directory());
    // 平台层（snug-win）是库，没有日志后端：不接这一行，它的告警只会进 stderr，
    // 而 GUI 程序没有控制台，等于把这些告警丢掉。
    snug_win::set_warn_sink(|message| crate::warn!("{message}"));
    crate::info!("SnugWin (Rust) 启动 v{}", env!("CARGO_PKG_VERSION"));

    let _instance_guard = match acquire_single_instance(&paths) {
        Some(guard) => guard,
        None => {
            crate::warn!("已有实例在运行，本次退出");
            logging::shutdown();
            return Ok(());
        }
    };

    crate::debug!("数据目录 {}", paths.data_dir().display());

    let config = load_config(&paths);
    let catalog = load_catalog();
    crate::info!(
        "配置：手机屏={} 手势={} 机型={:?} 档位基准={}",
        config.phone_screen,
        config.gestures,
        config.phone.model,
        ScreenFit::from_phone(&config.phone.screen.to_fit_spec()).base_width()
    );

    let pipe_ready = snug_win::vdd::is_driver_pipe_available(snug_win::vdd::DEFAULT_PIPE_NAME);
    crate::info!(
        "虚拟屏驱动管道：{}",
        if pipe_ready { "就绪" } else { "未检测到（手机屏功能需要驱动）" }
    );

    let fit = ScreenFit::from_phone(&config.phone.screen.to_fit_spec());
    let logical = fit.size(fit.base_width());

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Snug 手机屏")
            .with_decorations(false)
            .with_inner_size([logical.width as f32, logical.height as f32])
            .with_min_inner_size([540.0, 800.0]),
        ..Default::default()
    };

    let result = eframe::run_native(
        "Snug",
        options,
        Box::new(move |cc| {
            install_cjk_font(&cc.egui_ctx);
            let repaint_ctx = cc.egui_ctx.clone();
            let controller = controller::Controller::start(
                &config,
                &paths,
                move || repaint_ctx.request_repaint(),
                |line: &str| crate::info!("desk: {line}"),
            );
            crate::info!("控制器已启动（手机屏 {}）", if pipe_ready { "可用" } else { "不可用" });
            Ok(Box::new(ui::PhoneHomeApp::new(
                config,
                catalog,
                paths.clone(),
                pipe_ready,
                controller,
                logical.width as f32,
            )))
        }),
    );

    if let Err(err) = &result {
        crate::error!("界面异常退出：{err}");
    }
    logging::shutdown();
    result
}

fn load_config(paths: &Paths) -> AppConfig {
    let file = paths.config_file();
    match std::fs::read_to_string(&file) {
        Ok(json) => {
            let cfg = AppConfig::from_json_or_default(&json);
            crate::info!("已读取配置 {}", file.display());
            cfg
        }
        Err(_) => {
            crate::info!("首次运行：使用默认配置");
            AppConfig::default()
        }
    }
}

fn load_catalog() -> Option<PhoneCatalog> {
    let candidates = [
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("assets").join("phone-models.json"))),
        Some(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/phone-models.json")),
    ];
    for path in candidates.into_iter().flatten() {
        if !path.exists() {
            continue;
        }
        match PhoneCatalog::load_from_file(&path) {
            Ok(catalog) => {
                crate::info!(
                    "机型库已加载：{} 个品牌 / {} 款机型",
                    catalog.brands.len(),
                    catalog.models().count()
                );
                return Some(catalog);
            }
            Err(err) => crate::warn!("机型库读取失败 {}：{err}", path.display()),
        }
    }
    crate::warn!("未找到机型库 phone-models.json");
    None
}

/// 用 Win32 系统字体渲染中文：egui 自带字体没有 CJK。
fn install_cjk_font(ctx: &egui::Context) {
    let candidates = [
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\msyhl.ttc",
        r"C:\Windows\Fonts\simhei.ttf",
        r"C:\Windows\Fonts\simsun.ttc",
        r"C:\Windows\Fonts\Deng.ttf",
    ];
    for path in candidates {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let mut fonts = egui::FontDefinitions::default();
        fonts
            .font_data
            .insert("cjk".to_string(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts.families.entry(family).or_default().insert(0, "cjk".to_string());
        }
        ctx.set_fonts(fonts);
        crate::info!("已加载中文字体：{path}");
        return;
    }
    crate::warn!("没有找到可用的中文字体，界面中文可能显示为方块");
}

/// 单实例：持有数据目录下的锁文件句柄。
fn acquire_single_instance(paths: &Paths) -> Option<File> {
    let lock_path = paths.data_dir().join("snug.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .ok()?;
    match file.try_lock() {
        Ok(()) => Some(file),
        Err(_) => None,
    }
}
