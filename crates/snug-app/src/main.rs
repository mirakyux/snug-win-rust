//! snug —— SnugWin 的 Rust 重写（托盘程序 + 手机屏界面）。
//!
//! 阶段：核心逻辑层与虚拟屏管道已移植；显示配置、搬窗、钩子等平台层由 `snug-win` 逐步补齐。
//! 当前这个二进制已经能：加载配置与机型库、单实例、写日志、把手机屏界面按机型比例显示出来。

mod bubble;
mod controller;
mod driver;
mod library;
mod logging;
mod settings;
mod theme;
mod shots;
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
    // 崩溃要留痕：GUI 进程没有控制台，默认的 panic 输出会直接丢掉。
    std::panic::set_hook(Box::new(|info| {
        crate::error!("未捕获的 panic：{info}");
    }));
    // 平台层（snug-win）是库，没有日志后端：不接这一行，它的告警只会进 stderr，
    // 而 GUI 程序没有控制台，等于把这些告警丢掉。
    snug_win::set_warn_sink(|message| crate::warn!("{message}"));
    // 信息通道同理：平台层的成功路径（如"已按归属记录清理残留手机屏"）也要进日志。
    snug_win::set_info_sink(|message| crate::info!("{message}"));
    // 主线程要调 shell COM（App 图标走 IShellItemImageFactory）：先初始化 STA。
    // 返回 true 表示这次是我们初始化的，退出前必须配对释放；false 表示线程早已初始化过。
    let com_owned = snug_win::init_sta();
    if !com_owned {
        crate::info!("主线程 COM 已由别处初始化，跳过 STA 初始化");
    }
    crate::info!("SnugWin (Rust) 启动 v{}", env!("CARGO_PKG_VERSION"));

    let _instance_guard = match acquire_single_instance(&paths) {
        Some(guard) => guard,
        None => {
            crate::warn!("已有实例在运行，本次退出");
            logging::shutdown();
            return Ok(());
        }
    };

    // `--minimized` 是开机自启那条计划任务的参数（C# `StartupTaskDefinition.MinimizedArg`）。
    // 以前这个参数没人解析，等于自启和手动启动长得一样；现在它决定"要不要闪一次窗"。
    let start_minimized = has_minimized_flag(std::env::args().skip(1));
    crate::info!(
        "启动形态：{}",
        if start_minimized {
            "开机自启（--minimized，不闪窗、不抢焦点）"
        } else {
            "手动启动"
        }
    );

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
    // 没有驱动就**自己装**：发布包里带着官方安装包，本进程清单是 requireAdministrator，
    // 不需要用户去别处下载、也不需要额外点一次 UAC。后台线程做，别挡启动；
    // 装完命名管道起来后，建屏的重试阶梯自然就把手机屏拉起来。
    if !pipe_ready {
        driver::spawn_ensure_installed();
    }

    let fit = ScreenFit::from_phone(&config.phone.screen.to_fit_spec());
    let logical = fit.size(fit.base_width());

    let mut viewport = egui::ViewportBuilder::default()
        .with_title(ui::WINDOW_TITLE)
        .with_decorations(false)
        // 不进任务栏：手机屏界面不是"应用程序窗口"，和 C# 的 HomeBar/PhoneHome 一样。
        .with_taskbar(false)
        .with_inner_size([logical.width as f32, logical.height as f32])
        .with_min_inner_size([540.0, 800.0]);
    if start_minimized {
        // 开机自启时**别在主屏上闪一下**：先建在屏幕外，等手机屏就绪由 `sync_window`
        // 摆到虚拟屏上。这里不用 `Visible(false)`——真机验证过，窗口一隐藏 eframe
        // 就不再出帧，托盘点击、退出请求、搬窗会全部失效。
        viewport = viewport.with_position([-30_000.0, -30_000.0]);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    let result = eframe::run_native(
        "Snug",
        options,
        Box::new(move |cc| {
            install_cjk_font(&cc.egui_ctx);
            let repaint_ctx = cc.egui_ctx.clone();
            // 心跳线程：eframe 在窗口长期没输入时会停在 Wait 上，而我们要靠 `update()`
            // 去轮询托盘事件、退出请求、搬窗。真机上撞到过"隐藏窗口后一帧都不出"，
            // 所以这里明确按节奏叫醒它，不依赖"窗口可见"这个前提。
            {
                let ctx = cc.egui_ctx.clone();
                std::thread::Builder::new()
                    .name("snug-ui-tick".to_string())
                    .spawn(move || loop {
                        std::thread::sleep(std::time::Duration::from_millis(250));
                        ctx.request_repaint();
                    })
                    .ok();
            }
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
                start_minimized,
                controller,
            )))
        }),
    );

    if let Err(err) = &result {
        crate::error!("界面异常退出：{err}");
    } else {
        crate::info!("事件循环已退出");
    }
    if com_owned {
        snug_win::uninit_com();
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
    let external = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("assets").join("phone-models.json")));
    load_catalog_from(external.as_deref())
}

/// exe 旁的资源可覆盖内置机型库；单 exe 分发时不需要任何运行时源码路径。
fn load_catalog_from(external: Option<&std::path::Path>) -> Option<PhoneCatalog> {
    if let Some(path) = external.filter(|path| path.exists()) {
        match PhoneCatalog::load_from_file(path) {
            Ok(catalog) => {
                crate::info!(
                    "外部机型库已加载：{} 个品牌 / {} 款机型",
                    catalog.brands.len(),
                    catalog.models().count()
                );
                return Some(catalog);
            }
            Err(err) => crate::warn!("机型库读取失败 {}：{err}，改用内置库", path.display()),
        }
    }

    match PhoneCatalog::from_json(include_str!("../../../assets/phone-models.json")) {
        Ok(catalog) => {
            crate::info!(
                "内置机型库已加载：{} 个品牌 / {} 款机型",
                catalog.brands.len(),
                catalog.models().count()
            );
            Some(catalog)
        }
        Err(err) => {
            crate::warn!("内置机型库读取失败：{err}");
            None
        }
    }
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

/// 命令行里有没有 `--minimized`。
///
/// C# 用 `StringComparison.OrdinalIgnoreCase` 比，这里也一样：计划任务里写的是小写，
/// 但有人手动拼 `--MINIMIZED` 也应当认。
fn has_minimized_flag(args: impl IntoIterator<Item = String>) -> bool {
    args.into_iter().any(|arg| arg.eq_ignore_ascii_case("--minimized"))
}

#[cfg(test)]
mod tests {
    use super::{has_minimized_flag, load_catalog_from};

    struct CatalogDirectory(std::path::PathBuf);

    impl CatalogDirectory {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "snug-catalog-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            ));
            std::fs::create_dir(&path).expect("isolated catalog directory");
            Self(path)
        }

        fn file(&self) -> std::path::PathBuf {
            self.0.join("phone-models.json")
        }
    }

    impl Drop for CatalogDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.file());
            let _ = std::fs::remove_dir(&self.0);
        }
    }

    #[test]
    fn catalog_embedded_is_available_without_external_files() {
        let directory = CatalogDirectory::new();
        let missing = directory.file();
        assert!(!missing.exists());
        for catalog in [load_catalog_from(None), load_catalog_from(Some(&missing))] {
            let catalog = catalog.expect("embedded catalog requires no runtime source path");
            assert_eq!(catalog.models().count(), 853);
            for kind in [
                snug_core::phone_models::DeviceKind::Phone,
                snug_core::phone_models::DeviceKind::Tablet,
                snug_core::phone_models::DeviceKind::Computer,
            ] {
                assert!(catalog.models().any(|model| model.kind == kind));
            }
        }
        assert!(std::fs::read_dir(&directory.0).expect("empty directory").next().is_none());
    }

    #[test]
    fn catalog_external_file_overrides_the_embedded_catalog() {
        let directory = CatalogDirectory::new();
        let path = directory.file();
        let json = r#"{"brands":[{"name":"Custom","models":[{"name":"Test phone","screen":[1080,1920]}]}]}"#;
        std::fs::write(&path, json).expect("external override");

        let catalog = load_catalog_from(Some(&path)).expect("external catalog");

        assert_eq!(catalog.models().count(), 1);
        assert!(catalog.find_model("Custom Test phone").is_some());
        assert_eq!(std::fs::read_to_string(path).expect("unchanged override"), json);
    }

    #[test]
    fn catalog_invalid_external_file_falls_back_to_embedded_catalog() {
        let directory = CatalogDirectory::new();
        let path = directory.file();
        for invalid in ["not JSON", "{}"] {
            std::fs::write(&path, invalid).expect("invalid external override");

            let catalog = load_catalog_from(Some(&path)).expect("embedded fallback");

            assert_eq!(catalog.models().count(), 853);
            assert_eq!(std::fs::read_to_string(&path).expect("unchanged override"), invalid);
        }
    }

    #[test]
    fn minimized_flag_is_recognized_regardless_of_case() {
        assert!(has_minimized_flag(["--minimized".to_string()]));
        assert!(has_minimized_flag(["--MINIMIZED".to_string()]));
        assert!(has_minimized_flag(["--Minimized".to_string()]));
    }

    #[test]
    fn minimized_flag_needs_other_args_to_be_ignored() {
        assert!(!has_minimized_flag(Vec::new()));
        assert!(!has_minimized_flag(["--other".to_string(), "minimized".to_string()]));
        assert!(has_minimized_flag([
            "--other".to_string(),
            "--minimized".to_string()
        ]));
    }
}
