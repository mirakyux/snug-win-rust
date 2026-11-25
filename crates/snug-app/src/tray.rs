//! 托盘图标，移植自 C# 版 `App.xaml.cs` 的 `NotifyIcon` 部分。
//!
//! 托盘菜单：
//! 1. `状态：…`（禁用，随状态刷新）
//! 2. 分隔线
//! 3. `打开桌面设置...` → 实体屏上的设置窗
//! 4. `预览手机界面...` → 桌面预览窗（对应 C# `OpenPhonePreview`）
//! 5. `检查更新...` → 设置窗第 3 页（关于与更新）
//! 6. `重新进入手机屏` → 撤销"收起手机屏"（C# `_reentryMenuItem`）
//! 7. `手机屏显示` → 当前配置的三种显示模式
//! 8. 分隔线
//! 9. `退出 SnugWin（先还原窗口）`
//!
//! 一处**有意偏离**：C# 用 `Visible = false` 藏起第 6 项，`muda 0.19` 的
//! `MenuItem` 没有 `set_visible`，这里改成"不可用时禁用"（`set_enabled`）。
//!
//! 双击托盘 = 打开设置（C# `_trayIcon.DoubleClick`）。
//! 图标随连接状态切换（沿用 C# 的两张 ICO，预先转成 32×32 RGBA）。
//!
//! 平台细节：`tray-icon` 在创建它的线程上建一个隐藏窗口，消息由 winit/eframe
//! 的消息泵分发，所以**必须在主线程创建**（和 C# 里 NotifyIcon 属于 UI 线程同理）。

use snug_core::PhoneScreenMode;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::settings::{PHONE_SCREEN_MODES, phone_screen_mode_label};

const ICON_SIZE: u32 = 32;
const ICON_BYTES: usize = (ICON_SIZE * ICON_SIZE * 4) as usize;
/// 静态空闲图标（未连接），由 tools/generate_tray_icons.ps1 生成。
const IDLE_ICON: &[u8; ICON_BYTES] = include_bytes!("../assets/snugwin-idle-32x32.rgba");
/// 已连接图标，与旧版运行时 ICO 解码及缩放后的像素逐字节一致。
const CONNECTED_ICON: &[u8; ICON_BYTES] = include_bytes!("../assets/snugwin-connected-32x32.rgba");

/// 托盘上的一次用户操作。副作用由调用方落地（和 `settings` 的动作队列同风格）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayCommand {
    /// 打开设置窗并切到指定页（C# 的 `ShowSettings(index)`）。
    OpenSettings(usize),
    /// 桌面预览手机界面（C# `OpenPhonePreview`）。
    OpenPreview,
    /// 重新进入手机屏：撤销"收起"（C# `ReenterScreen`）。
    Reenter,
    /// 选择手机屏的显示模式。
    SetPhoneScreenMode(PhoneScreenMode),
    /// 退出（先还原窗口）。
    Quit,
}

/// 托盘本体。`None` 的语义是"这台机器没有托盘可用"，调用方如实降级即可。
pub struct Tray {
    _icon: TrayIcon,
    status_item: MenuItem,
    mode_items: [(PhoneScreenMode, CheckMenuItem); 3],
    reentry_item: MenuItem,
    settings_id: MenuId,
    update_id: MenuId,
    preview_id: MenuId,
    reentry_id: MenuId,
    quit_id: MenuId,
    connected: bool,
    last_summary: String,
}

impl Tray {
    /// 建托盘。找不到 shell（如某些会话/服务环境）时返回 `None`，不 panic。
    pub fn new(connected: bool, mode: PhoneScreenMode) -> Option<Self> {
        let status_item = MenuItem::new("状态：初始化中", false, None);
        let settings = MenuItem::new("打开桌面设置...", true, None);
        let preview = MenuItem::new("预览手机界面...", true, None);
        let update = MenuItem::new("检查更新...", true, None);
        // C# 里这一项默认 `Visible = false`，只在"手机屏被收起且还在会话里"时出现。
        let reentry = MenuItem::new("重新进入手机屏", false, None);
        let mode_items = PHONE_SCREEN_MODES.map(|option| {
            let item = CheckMenuItem::new(
                phone_screen_mode_label(option),
                true,
                option == mode,
                None,
            );
            (option, item)
        });
        let modes = Submenu::new("手机屏显示", true);
        for (_, item) in &mode_items {
            modes.append(item).ok()?;
        }
        let quit = MenuItem::new("退出 SnugWin（先还原窗口）", true, None);

        let menu = Menu::new();
        menu.append_items(&[
            &status_item,
            &PredefinedMenuItem::separator(),
            &settings,
            &preview,
            &update,
            &reentry,
            &modes,
            &PredefinedMenuItem::separator(),
            &quit,
        ])
        .ok()?;

        let icon = load_icon(if connected { CONNECTED_ICON } else { IDLE_ICON })
            .or_else(|| load_icon(IDLE_ICON))?;

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            // 这个初始值同时会成为 tray-icon 那个隐藏窗口的**窗口标题**，后续
            // `set_tooltip` 只改 szTip、不改标题。所以 UIA 里读到的是
            // "<初始值> <当前 szTip>" 两段（真机：`SnugWin - UU远程手机竖屏伴侣
            // SnugWin - 手机屏：关`）；鼠标悬停看到的是 szTip，是正确的。
            // crate 没有"改窗口标题"的接口，这里不折腾，保留产品名。
            .with_tooltip("SnugWin - UU远程手机竖屏伴侣")
            .with_icon(icon)
            .build()
            .ok()?;

        Some(Self {
            _icon: tray,
            status_item,
            mode_items,
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
            } else if event.id == self.quit_id {
                out.push(TrayCommand::Quit);
            } else if let Some((mode, _)) = self
                .mode_items
                .iter()
                .find(|(_, item)| item.id() == &event.id)
            {
                out.push(TrayCommand::SetPhoneScreenMode(*mode));
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

    /// 以已应用的配置为准，三个模式中只勾选当前一项。
    pub fn set_phone_screen_mode(&self, mode: PhoneScreenMode) {
        for (option, item) in &self.mode_items {
            let checked = *option == mode;
            if item.is_checked() != checked {
                item.set_checked(checked);
            }
        }
    }
}

/// 直接使用预生成的托盘 RGBA，不在运行时解码 ICO 或缩放。
///
/// Windows 托盘实际显示 16/20/24/32 像素那几帧，这里统一使用 32×32：
/// 直接送 256×256 会让部分 shell 缩放发糊。
fn load_icon(bytes: &[u8]) -> Option<Icon> {
    Icon::from_rgba(bytes.to_vec(), ICON_SIZE, ICON_SIZE).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_bundled_icons_load_at_32_pixels() {
        for (name, bytes) in [("idle", IDLE_ICON), ("connected", CONNECTED_ICON)] {
            assert_eq!(bytes.len(), 32 * 32 * 4, "{name} 像素数不匹配");
            assert!(load_icon(bytes).is_some(), "{name} 送不进 tray-icon");
        }
    }

    #[test]
    fn bundled_icons_keep_the_legacy_decoded_pixels() {
        // image 0.25.10（当前 Cargo.lock）ICO 解码 + Lanczos3 的完整 RGBA 真值。
        // tools/generate_tray_icons.ps1 -Check 还会逐字节对照原始 ICO 重算结果。
        for (name, bytes, expected) in [
            (
                "idle",
                IDLE_ICON,
                "A9DDFF8AF9FD7ADD6E390D28EE8196768B7C1D6367B0313C941E17B4CADD5770",
            ),
            (
                "connected",
                CONNECTED_ICON,
                "281C1C94B9EA8CA7FBA252956A519F4B8382860B468ED799E78D284EE5BEAFEF",
            ),
        ] {
            assert_eq!(
                snug_win::update::sha256_hex(bytes),
                expected,
                "{name} 像素改变"
            );
        }
    }

    #[test]
    fn tooltip_is_truncated_to_the_windows_limit() {
        // C# 用 NotifyIcon.Text，超过 127 字符会抛异常；这里截断而不是失败。
        let long = "状".repeat(200);
        let tip: String = format!("SnugWin - {long}").chars().take(127).collect();
        assert_eq!(tip.chars().count(), 127);
    }
}
