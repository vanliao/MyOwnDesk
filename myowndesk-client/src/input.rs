//! 输入注入模块——定义 `InputBackend` trait 并提供 `DesktopInput` 实现。
//!
//! `DesktopInput` 在用户桌面上调用 Win32 `SendInput` 注入键盘和鼠标事件。
//! Ticket 11 将新增 `ServiceInput`，在 Winlogon 安全桌面下注入。

use myowndesk_protocol::MouseButton;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEINPUT,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_WHEEL, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

// ============================================================
// InputBackend trait
// ============================================================

/// 输入注入后端抽象。
pub trait InputBackend: Send {
    fn send_key(&mut self, key_code: i32, pressed: bool, extended: bool) -> anyhow::Result<()>;
    fn send_mouse_move(&mut self, x: i32, y: i32) -> anyhow::Result<()>;
    fn send_mouse_button(&mut self, button: MouseButton, pressed: bool) -> anyhow::Result<()>;
    fn send_mouse_wheel(&mut self, delta: i32) -> anyhow::Result<()>;
}

// ============================================================
// DesktopInput
// ============================================================

/// 用户桌面输入注入——在当前活动桌面上调用 Win32 `SendInput`。
pub struct DesktopInput {
    screen_width: u32,
    screen_height: u32,
}

impl DesktopInput {
    pub fn new() -> Self {
        let screen_width = unsafe { GetSystemMetrics(SM_CXSCREEN) } as u32;
        let screen_height = unsafe { GetSystemMetrics(SM_CYSCREEN) } as u32;
        tracing::info!("DesktopInput 初始化: {}x{}", screen_width, screen_height);
        Self { screen_width, screen_height }
    }

    /// 将被控端桌面像素坐标归一化为 SendInput 所需的 0..=65535 范围。
    fn normalize_coords(&self, x: i32, y: i32) -> (u32, u32) {
        let nx = if self.screen_width > 0 {
            ((x as u32).saturating_mul(65535) / self.screen_width).min(65535)
        } else {
            0
        };
        let ny = if self.screen_height > 0 {
            ((y as u32).saturating_mul(65535) / self.screen_height).min(65535)
        } else {
            0
        };
        (nx, ny)
    }

    fn send_single_input(&self, inp: INPUT) -> anyhow::Result<()> {
        let inputs = [inp];
        let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent == 0 {
            return Err(anyhow::anyhow!(
                "SendInput 失败: GetLastError={}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }
}

impl InputBackend for DesktopInput {
    fn send_key(&mut self, key_code: i32, pressed: bool, extended: bool) -> anyhow::Result<()> {
        let mut flags = KEYBD_EVENT_FLAGS::default();
        if !pressed {
            flags |= KEYEVENTF_KEYUP;
        }
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        flags |= KEYEVENTF_SCANCODE;

        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(key_code as u16),
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        self.send_single_input(input)
    }

    fn send_mouse_move(&mut self, x: i32, y: i32) -> anyhow::Result<()> {
        let (nx, ny) = self.normalize_coords(x, y);
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
                mi: MOUSEINPUT {
                    dx: nx as i32,
                    dy: ny as i32,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        self.send_single_input(input)
    }

    fn send_mouse_button(&mut self, button: MouseButton, pressed: bool) -> anyhow::Result<()> {
        let flags = match (button, pressed) {
            (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
            (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
            (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
            (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
            (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
            (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
        };
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        self.send_single_input(input)
    }

    fn send_mouse_wheel(&mut self, delta: i32) -> anyhow::Result<()> {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: (delta * 120) as u32, // WHEEL_DELTA = 120
                    dwFlags: MOUSEEVENTF_WHEEL,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        self.send_single_input(input)
    }
}

impl Default for DesktopInput {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_desktop_input_creation() {
        let di = DesktopInput::new();
        assert!(di.screen_width > 0);
        assert!(di.screen_height > 0);
    }

    #[test]
    fn test_normalize_coords() {
        let di = DesktopInput { screen_width: 1920, screen_height: 1080 };
        let (x, y) = di.normalize_coords(0, 0);
        assert_eq!((x, y), (0, 0));
        let (x, y) = di.normalize_coords(1920, 1080);
        assert_eq!((x, y), (65535, 65535));
    }

    #[test]
    fn test_normalize_coords_zero_screen() {
        let di = DesktopInput { screen_width: 0, screen_height: 0 };
        let (x, y) = di.normalize_coords(100, 200);
        assert_eq!((x, y), (0, 0));
    }

    #[test]
    fn test_clamp_overflow() {
        let di = DesktopInput { screen_width: 1920, screen_height: 1080 };
        let (x, y) = di.normalize_coords(5000, 3000);
        assert_eq!((x, y), (65535, 65535));
    }
}
