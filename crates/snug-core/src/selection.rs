//! 选择浮层（截图选区旁的"复制 / 粘贴 / 删除"气泡）发什么快捷键。
//!
//! 移植自 C# 文件 `SnugWin.Core/Touch/SelectionActions.cs`：
//! `SelectionBubbleAction`、`SelectionKeystroke`、`SelectionShortcutResolver`。
//! 终端窗口里复制/粘贴走 `Ctrl+Shift`，普通窗口走 `Ctrl`。

/// 气泡上的动作（对应 C# `SelectionBubbleAction`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionBubbleAction {
    Copy,
    Paste,
    Delete,
}

/// 要发的一次按键（对应 C# `SelectionKeystroke`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionKeystroke {
    pub virtual_key: u16,
    pub with_ctrl: bool,
    pub with_shift: bool,
}

impl SelectionKeystroke {
    pub fn new(virtual_key: u16, with_ctrl: bool, with_shift: bool) -> Self {
        Self { virtual_key, with_ctrl, with_shift }
    }
}

/// 退格键。
pub const VK_BACK: u16 = 0x08;
/// `C`。
pub const VK_C: u16 = 0x43;
/// `V`。
pub const VK_V: u16 = 0x56;

/// 是不是新版 Windows Terminal（对应 C# `IsModernTerminal`）。
pub fn is_modern_terminal(window_class: Option<&str>, exe_name: Option<&str>) -> bool {
    window_class == Some("CASCADIA_HOSTING_WINDOW_CLASS")
        || exe_name.is_some_and(|name| name.eq_ignore_ascii_case("WindowsTerminal.exe"))
}

/// 是不是终端类窗口（对应 C# `IsTerminal`）。
pub fn is_terminal(window_class: Option<&str>, exe_name: Option<&str>) -> bool {
    is_modern_terminal(window_class, exe_name)
        || window_class == Some("ConsoleWindowClass")
        || exe_name.is_some_and(|name| {
            name.eq_ignore_ascii_case("OpenConsole.exe")
                || name.eq_ignore_ascii_case("powershell.exe")
                || name.eq_ignore_ascii_case("pwsh.exe")
                || name.eq_ignore_ascii_case("cmd.exe")
        })
}

/// 解析出该动作要发的按键（对应 C# `SelectionShortcutResolver.Resolve`）。
pub fn resolve(
    action: SelectionBubbleAction,
    window_class: Option<&str>,
    exe_name: Option<&str>,
) -> SelectionKeystroke {
    match action {
        SelectionBubbleAction::Copy => {
            SelectionKeystroke::new(VK_C, true, is_terminal(window_class, exe_name))
        }
        SelectionBubbleAction::Paste => {
            SelectionKeystroke::new(VK_V, true, is_modern_terminal(window_class, exe_name))
        }
        SelectionBubbleAction::Delete => SelectionKeystroke::new(VK_BACK, false, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_uses_ctrl_shift_in_terminals() {
        // 新版终端：Ctrl+Shift+C。
        assert_eq!(
            resolve(SelectionBubbleAction::Copy, Some("CASCADIA_HOSTING_WINDOW_CLASS"), None),
            SelectionKeystroke::new(VK_C, true, true)
        );
        // 老控制台 / PowerShell / cmd：也算终端。
        for exe in ["powershell.exe", "pwsh.exe", "cmd.exe", "OpenConsole.exe"] {
            assert_eq!(
                resolve(SelectionBubbleAction::Copy, None, Some(exe)),
                SelectionKeystroke::new(VK_C, true, true),
                "{exe} 应走 Ctrl+Shift+C"
            );
        }
        // 普通窗口：Ctrl+C。
        assert_eq!(
            resolve(SelectionBubbleAction::Copy, Some("Notepad"), Some("notepad.exe")),
            SelectionKeystroke::new(VK_C, true, false)
        );
    }

    #[test]
    fn paste_only_uses_shift_in_modern_terminals() {
        assert_eq!(
            resolve(SelectionBubbleAction::Paste, Some("CASCADIA_HOSTING_WINDOW_CLASS"), None),
            SelectionKeystroke::new(VK_V, true, true)
        );
        // 老控制台不需要 Shift（SendInput 之外由控制台自己粘贴）。
        assert_eq!(
            resolve(SelectionBubbleAction::Paste, Some("ConsoleWindowClass"), None),
            SelectionKeystroke::new(VK_V, true, false)
        );
        assert_eq!(
            resolve(SelectionBubbleAction::Paste, None, Some("explorer.exe")),
            SelectionKeystroke::new(VK_V, true, false)
        );
    }

    #[test]
    fn delete_sends_a_bare_backspace() {
        assert_eq!(
            resolve(SelectionBubbleAction::Delete, Some("CASCADIA_HOSTING_WINDOW_CLASS"), None),
            SelectionKeystroke::new(VK_BACK, false, false)
        );
    }

    #[test]
    fn exe_matching_ignores_case_but_window_class_does_not() {
        assert!(is_modern_terminal(None, Some("windowsterminal.EXE")));
        assert!(!is_modern_terminal(Some("cascadia_hosting_window_class"), None));
        assert!(is_terminal(None, None) == false);
    }
}
