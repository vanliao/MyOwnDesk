//! minifb `Key` → Windows 虚拟键码（VK_*）映射表。
//!
//! 将 minifb 的按键枚举映射为 Windows `SendInput` 所需的虚拟键码。
//! 全覆盖：字母、数字、功能键、编辑键、修饰键、小键盘等。

use minifb::Key;

/// 将 minifb `Key` 映射为 Windows 虚拟键码（VK_*）。
pub fn minifb_key_to_vk(key: Key) -> Option<i32> {
    let vk = match key {
        // A-Z (VK_A=0x41 .. VK_Z=0x5A)
        Key::A => 0x41, Key::B => 0x42, Key::C => 0x43,
        Key::D => 0x44, Key::E => 0x45, Key::F => 0x46,
        Key::G => 0x47, Key::H => 0x48, Key::I => 0x49,
        Key::J => 0x4A, Key::K => 0x4B, Key::L => 0x4C,
        Key::M => 0x4D, Key::N => 0x4E, Key::O => 0x4F,
        Key::P => 0x50, Key::Q => 0x51, Key::R => 0x52,
        Key::S => 0x53, Key::T => 0x54, Key::U => 0x55,
        Key::V => 0x56, Key::W => 0x57, Key::X => 0x58,
        Key::Y => 0x59, Key::Z => 0x5A,

        // 0-9 (VK_0=0x30 .. VK_9=0x39)
        Key::Key0 => 0x30, Key::Key1 => 0x31, Key::Key2 => 0x32,
        Key::Key3 => 0x33, Key::Key4 => 0x34, Key::Key5 => 0x35,
        Key::Key6 => 0x36, Key::Key7 => 0x37, Key::Key8 => 0x38,
        Key::Key9 => 0x39,

        // F1-F12 (VK_F1=0x70 .. VK_F12=0x7B)
        Key::F1 => 0x70, Key::F2 => 0x71, Key::F3 => 0x72,
        Key::F4 => 0x73, Key::F5 => 0x74, Key::F6 => 0x75,
        Key::F7 => 0x76, Key::F8 => 0x77, Key::F9 => 0x78,
        Key::F10 => 0x79, Key::F11 => 0x7A, Key::F12 => 0x7B,
        Key::F13 => 0x7C, Key::F14 => 0x7D, Key::F15 => 0x7E,

        // 方向键
        Key::Up => 0x26, Key::Down => 0x28,
        Key::Left => 0x25, Key::Right => 0x27,

        // 编辑键
        Key::Enter => 0x0D, Key::Escape => 0x1B,
        Key::Tab => 0x09, Key::Backspace => 0x08,
        Key::Delete => 0x2E, Key::Insert => 0x2D,
        Key::Home => 0x24, Key::End => 0x23,
        Key::PageUp => 0x21, Key::PageDown => 0x22,
        Key::Space => 0x20,

        // 符号键
        Key::Comma => 0xBC, Key::Period => 0xBE,
        Key::Slash => 0xBF, Key::Semicolon => 0xBA,
        Key::Apostrophe => 0xDE,
        Key::LeftBracket => 0xDB, Key::RightBracket => 0xDD,
        Key::Backslash => 0xDC,
        Key::Minus => 0xBD, Key::Equal => 0xBB,

        // 修饰键
        Key::LeftCtrl => 0xA2, Key::RightCtrl => 0xA3,
        Key::LeftAlt => 0xA4, Key::RightAlt => 0xA5,
        Key::LeftShift => 0xA0, Key::RightShift => 0xA1,

        // Windows 键
        Key::LeftSuper => 0x5B, Key::RightSuper => 0x5C,

        // 锁定键
        Key::CapsLock => 0x14, Key::NumLock => 0x90,
        Key::ScrollLock => 0x91,

        // 杂项
        Key::Pause => 0x13, Key::Menu => 0x5D,

        // 小键盘
        Key::NumPad0 => 0x60, Key::NumPad1 => 0x61,
        Key::NumPad2 => 0x62, Key::NumPad3 => 0x63,
        Key::NumPad4 => 0x64, Key::NumPad5 => 0x65,
        Key::NumPad6 => 0x66, Key::NumPad7 => 0x67,
        Key::NumPad8 => 0x68, Key::NumPad9 => 0x69,
        Key::NumPadDot => 0x6E, Key::NumPadEnter => 0x0D,
        Key::NumPadPlus => 0x6B, Key::NumPadMinus => 0x6D,
        Key::NumPadSlash => 0x6F, Key::NumPadAsterisk => 0x6A,

        _ => return None,
    };
    Some(vk)
}

/// 检查是否为扩展键（需要 KEYEVENTF_EXTENDEDKEY 标志）。
pub fn is_extended_key(key: Key) -> bool {
    matches!(
        key,
        Key::RightCtrl | Key::RightAlt
            | Key::Up | Key::Down | Key::Left | Key::Right
            | Key::Insert | Key::Delete | Key::Home | Key::End
            | Key::PageUp | Key::PageDown
            | Key::NumPadEnter | Key::NumPadSlash
            | Key::LeftSuper | Key::RightSuper | Key::Menu
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alpha_keys() {
        assert_eq!(minifb_key_to_vk(Key::A), Some(0x41));
        assert_eq!(minifb_key_to_vk(Key::Z), Some(0x5A));
    }

    #[test]
    fn test_digit_keys() {
        assert_eq!(minifb_key_to_vk(Key::Key0), Some(0x30));
        assert_eq!(minifb_key_to_vk(Key::Key9), Some(0x39));
    }

    #[test]
    fn test_function_keys() {
        assert_eq!(minifb_key_to_vk(Key::F1), Some(0x70));
        assert_eq!(minifb_key_to_vk(Key::F12), Some(0x7B));
    }

    #[test]
    fn test_modifier_keys() {
        assert_eq!(minifb_key_to_vk(Key::LeftCtrl), Some(0xA2));
        assert_eq!(minifb_key_to_vk(Key::LeftAlt), Some(0xA4));
        assert_eq!(minifb_key_to_vk(Key::LeftShift), Some(0xA0));
    }

    #[test]
    fn test_extended_keys() {
        assert!(is_extended_key(Key::RightCtrl));
        assert!(is_extended_key(Key::Up));
        assert!(!is_extended_key(Key::A));
    }

    #[test]
    fn test_numpad() {
        assert_eq!(minifb_key_to_vk(Key::NumPad0), Some(0x60));
        assert_eq!(minifb_key_to_vk(Key::NumPadPlus), Some(0x6B));
    }
}
