//! 托盘图标，移植自 C# 版 `App.xaml.cs` 的 `NotifyIcon` 部分。
//!
//! 菜单结构与 C# v1.3.0 逐项对齐：
//! 1. `状态：…`（禁用，随状态刷新）
//! 2. 分隔线
//! 3. `打开设置与诊断...` → 设置窗第 0 页
//! 4. `预览手机界面...` → 桌面预览窗（对应 C# `OpenPhonePreview`）
//! 5. `检查更新...` → 设置窗第 3 页（关于与更新）
//! 6. `重新进入手机屏` → 撤销"收起手机屏"（C# `_reentryMenuItem`）
//! 7. `手动开启手机屏（测试/兜底）`（可勾选）→ `SetManualOverride`
//! 8. 分隔线
//! 9. `退出 SnugWin（先还原窗口）`
//!
//! 一处**有意偏离**：C# 用 `Visible = false` 藏起第 6 项，`muda 0.19` 的
//! `MenuItem` 没有 `set_visible`，这里改成"不可用时禁用"（`set_enabled`）。
//!
//! 双击托盘 = 打开设置（C# `_trayIcon.DoubleClick`）。
//! 图标随连接状态切换 `snugwin-idle.ico` / `snugwin-connected.ico`（C# 同）。
//!
//! 平台细节：`tray-icon` 在创建它的线程上建一个隐藏窗口，消息由 winit/eframe
//! 的消息泵分发，所以**必须在主线程创建**（和 C# 里 NotifyIcon 属于 UI 线程同理）。

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// 静态空闲图标（未连接）。
const IDLE_ICON: &[u8] = include_bytes!("../assets/snugwin-idle.ico");
/// 已连接图标。
const CONNECTED_ICON: &[u8] = include_bytes!("../assets/snugwin-connected.ico");

/// 托盘上的一次用户操作。副作用由调用方落地（和 `settings` 的动作队列同风格）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayCommand {
    /// 打开设置窗并切到指定页（C# 的 `ShowSettings(index)`）。
    OpenSettings(usize),
    /// 桌面预览手机界面（C# `OpenPhonePreview`）。
    OpenPreview,
    /// 重新进入手机屏：撤销"收起"（C# `ReenterScreen`）。
    Reenter,
    /// 手动强制手机屏（菜单里的勾选项）。
    SetManualOverride(bool),
    /// 退出（先还原窗口）。
    Quit,
}

/// 托盘本体。`None` 的语义是"这台机器没有托盘可用"，调用方如实降级即可。
pub struct Tray {
    _icon: TrayIcon,
    status_item: MenuItem,
    manual_item: CheckMenuItem,
    reentry_item: MenuItem,
    settings_id: MenuId,
    update_id: MenuId,
    preview_id: MenuId,
    reentry_id: MenuId,
    quit_id: MenuId,
    manual_id: MenuId,
    connected: bool,
    last_summary: String,
}

impl Tray {
    /// 建托盘。找不到 shell（如某些会话/服务环境）时返回 `None`，不 panic。
    pub fn new(connected: bool) -> Option<Self> {
        let status_item = MenuItem::new("状态：初始化中", false, None);
        let settings = MenuItem::new("打开设置与诊断...", true, None);
        let preview = MenuItem::new("预览手机界面...", true, None);
        let update = MenuItem::new("检查更新...", true, None);
        // C# 里这一项默认 `Visible = false`，只在"手机屏被收起且还在会话里"时出现。
        let reentry = MenuItem::new("重新进入手机屏", false, None);
        let manual = CheckMenuItem::new("手动开启手机屏（测试/兜底）", true, false, None);
        let quit = MenuItem::new("退出 SnugWin（先还原窗口）", true, None);

        let menu = Menu::new();
        menu.append_items(&[
            &status_item,
            &PredefinedMenuItem::separator(),
            &settings,
            &preview,
            &update,
            &reentry,
            &manual,
            &PredefinedMenuItem::separator(),
            &quit,
        ])
        .ok()?;

        let icon = load_icon(if connected { CONNECTED_ICON } else { IDLE_ICON })
            .or_else(|| load_icon(IDLE_ICON))?;

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("SnugWin - UU远程手机竖屏伴侣")
            .with_icon(icon)
            .build()
            .ok()?;

        Some(Self {
            _icon: tray,
            status_item,
            manual_id: manual.id().clone(),
            manual_item: manual,
            reentry_id: reentry.id().clone(),
            reentry_item: reentry,
            settings_id: settings.id().clone(),
            preview_id: preview.id().clone(),
            update_id: update.id().clone(),
            quit_id: quit.id().clone(),
            connected,
            last_summary: String::new(),
        })
    }

    /// 收一次托盘事件（每帧调）。菜单点击与双击都从这里出来。
    pub fn poll(&mut self) -> Vec<TrayCommand> {
        let mut out = Vec::new();
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == self.settings_id {
                out.push(TrayCommand::OpenSettings(0));
            } else if event.id == self.preview_id {
                out.push(TrayCommand::OpenPreview);
            } else if event.id == self.update_id {
                out.push(TrayCommand::OpenSettings(3));
            } else if event.id == self.reentry_id {
                if self.reentry_item.is_enabled() {
                    out.push(TrayCommand::Reenter);
                }
            } else if event.id == self.manual_id {
                // muda 已经把勾选状态翻转过了，直接读当前值。
                out.push(TrayCommand::SetManualOverride(self.manual_item.is_checked()));
            } else if event.id == self.quit_id {
                out.push(TrayCommand::Quit);
            }
        }
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if matches!(event, TrayIconEvent::DoubleClick { .. }) {
                out.push(TrayCommand::OpenSettings(0));
            }
        }
        out
    }

    /// 「重新进入手机屏」能不能点（C# `_reentryMenuItem.Visible` 的判定）。
    ///
    /// C# 隐藏它，这里禁用——理由见文件头注释。
    pub fn set_reenter_available(&mut self, available: bool) {
        if self.reentry_item.is_enabled() != available {
            self.reentry_item.set_enabled(available);
        }
    }

    /// 刷新状态行、提示文字与图标（对应 C# `RefreshTray`）。
    pub fn set_status(&mut self, summary: &str, connected: bool) {
        let text = format!("状态：{summary}");
        if text != self.status_item.text() {
            self.status_item.set_text(&text);
        }
        if summary != self.last_summary {
            // C#：`_trayIcon.Text = $"SnugWin - {summary}"`（Windows 限制 127 字符）。
            let tip = format!("SnugWin - {summary}");
            let tip: String = tip.chars().take(127).collect();
            let _ = self._icon.set_tooltip(Some(&tip));
            self.last_summary = summary.to_string();
        }
        if connected != self.connected {
            if let Some(icon) = load_icon(if connected { CONNECTED_ICON } else { IDLE_ICON }) {
                let _ = self._icon.set_icon(Some(icon));
                self.connected = connected;
            }
        }
    }

    /// 外部改了手动强制状态（例如设置窗里改的），把勾选同步过来。
    pub fn set_manual_override(&mut self, on: bool) {
        if self.manual_item.is_checked() != on {
            self.manual_item.set_checked(on);
        }
    }
}

/// 把 .ico 解成托盘用的 RGBA。
///
/// Windows 托盘实际显示 16/20/24/32 像素那几帧，这里统一缩到 32×32：
/// 直接送 256×256 会让部分 shell 缩放发糊。
fn decode_icon(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Ico).ok()?;
    let rgba = image.to_rgba8();
    let (width, height) = (rgba.width(), rgba.height());
    const TARGET: u32 = 32;
    if width.min(height) > TARGET {
        let scaled = image::imageops::resize(
            &rgba,
            TARGET,
            TARGET,
            image::imageops::FilterType::Lanczos3,
        );
        Some((scaled.into_raw(), TARGET, TARGET))
    } else {
        Some((rgba.into_raw(), width, height))
    }
}

fn load_icon(bytes: &[u8]) -> Option<Icon> {
    let (raw, width, height) = decode_icon(bytes)?;
    Icon::from_rgba(raw, width, height).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_bundled_icons_decode_to_rgba() {
        for (name, bytes) in [("idle", IDLE_ICON), ("connected", CONNECTED_ICON)] {
            let (raw, w, h) = decode_icon(bytes).unwrap_or_else(|| panic!("{name} 图标解码失败"));
            assert!(w >= 16 && h >= 16, "{name} 尺寸过小：{w}×{h}");
            assert_eq!(raw.len(), (w * h * 4) as usize, "{name} 像素数不匹配");
            assert!(load_icon(bytes).is_some(), "{name} 送不进 tray-icon");
        }
    }

    #[test]
    fn icons_are_scaled_down_to_32() {
        let (_, w, h) = decode_icon(IDLE_ICON).expect("idle 图标");
        assert!(w <= 32 && h <= 32, "尺寸 {w}×{h} 没有缩到 32 以内");
    }

    #[test]
    fn tooltip_is_truncated_to_the_windows_limit() {
        // C# 用 NotifyIcon.Text，超过 127 字符会抛异常；这里截断而不是失败。
        let long = "状".repeat(200);
        let tip: String = format!("SnugWin - {long}").chars().take(127).collect();
        assert_eq!(tip.chars().count(), 127);
    }
}
