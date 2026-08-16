//! Ánh xạ [`KeyCode`] sang mã phím ảo của macOS.
//!
//! Các số này là `kVK_*` trong `Carbon/HIToolbox/Events.h`. Chúng chỉ vị trí
//! **vật lý** trên bàn phím chứ không phải ký tự — `kVK_ANSI_A` = 0 là phím
//! ngoài cùng bên trái hàng giữa, dù layout của người dùng in chữ gì lên đó.
//! Nhờ vậy host bấm đúng phím kể cả khi hai máy dùng layout khác nhau.

use rd_protocol::KeyCode;

/// Trả về `None` với phím macOS không có (PrintScreen, ScrollLock, Pause,
/// ContextMenu, Insert, NumLock trên bàn phím Apple).
pub fn virtual_key(code: KeyCode) -> Option<u16> {
    use KeyCode::*;
    Some(match code {
        A => 0,
        S => 1,
        D => 2,
        F => 3,
        H => 4,
        G => 5,
        Z => 6,
        X => 7,
        C => 8,
        V => 9,
        B => 11,
        Q => 12,
        W => 13,
        E => 14,
        R => 15,
        Y => 16,
        T => 17,
        Digit1 => 18,
        Digit2 => 19,
        Digit3 => 20,
        Digit4 => 21,
        Digit6 => 22,
        Digit5 => 23,
        Equal => 24,
        Digit9 => 25,
        Digit7 => 26,
        Minus => 27,
        Digit8 => 28,
        Digit0 => 29,
        BracketRight => 30,
        O => 31,
        U => 32,
        BracketLeft => 33,
        I => 34,
        P => 35,
        Enter => 36,
        L => 37,
        J => 38,
        Quote => 39,
        K => 40,
        Semicolon => 41,
        Backslash => 42,
        Comma => 43,
        Slash => 44,
        N => 45,
        M => 46,
        Period => 47,
        Tab => 48,
        Space => 49,
        Backquote => 50,
        Backspace => 51,
        Escape => 53,
        MetaLeft => 55,
        ShiftLeft => 56,
        CapsLock => 57,
        AltLeft => 58,
        ControlLeft => 59,
        MetaRight => 54,
        ShiftRight => 60,
        AltRight => 61,
        ControlRight => 62,

        NumpadDecimal => 65,
        NumpadMultiply => 67,
        NumpadAdd => 69,
        // macOS không có NumLock; phím cùng vị trí là Clear.
        NumLock => 71,
        NumpadDivide => 75,
        NumpadEnter => 76,
        NumpadSubtract => 78,
        Numpad0 => 82,
        Numpad1 => 83,
        Numpad2 => 84,
        Numpad3 => 85,
        Numpad4 => 86,
        Numpad5 => 87,
        Numpad6 => 88,
        Numpad7 => 89,
        Numpad8 => 91,
        Numpad9 => 92,

        F1 => 122,
        F2 => 120,
        F3 => 99,
        F4 => 118,
        F5 => 96,
        F6 => 97,
        F7 => 98,
        F8 => 100,
        F9 => 101,
        F10 => 109,
        F11 => 103,
        F12 => 111,

        Home => 115,
        PageUp => 116,
        Delete => 117,
        End => 119,
        PageDown => 121,
        ArrowLeft => 123,
        ArrowRight => 124,
        ArrowDown => 125,
        ArrowUp => 126,

        Insert | PrintScreen | ScrollLock | Pause | ContextMenu => return None,
    })
}

/// Phím có phải modifier không, và nếu có thì ứng với cờ nào.
///
/// macOS không tự suy ra cờ từ việc phím đang xuống: mỗi sự kiện phải tự mang
/// cờ đi kèm, nếu không Cmd+C sẽ được host hiểu thành C.
pub fn modifier_flag(code: KeyCode) -> Option<u64> {
    use KeyCode::*;
    // Giá trị lấy từ kCGEventFlagMask*.
    Some(match code {
        ShiftLeft | ShiftRight => 131072,
        ControlLeft | ControlRight => 262144,
        AltLeft | AltRight => 524288,
        MetaLeft | MetaRight => 1048576,
        CapsLock => 65536,
        _ => return None,
    })
}

/// Phím numpad phải kèm cờ riêng, nếu không một số ứng dụng phân biệt được
/// "1 trên hàng số" với "1 trên numpad" sẽ nhận sai.
pub fn is_numpad(code: KeyCode) -> bool {
    use KeyCode::*;
    matches!(
        code,
        Numpad0
            | Numpad1
            | Numpad2
            | Numpad3
            | Numpad4
            | Numpad5
            | Numpad6
            | Numpad7
            | Numpad8
            | Numpad9
            | NumpadAdd
            | NumpadSubtract
            | NumpadMultiply
            | NumpadDivide
            | NumpadDecimal
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn khong_co_hai_phim_trung_ma() {
        // Trùng mã nghĩa là bấm phím này ra phím kia — lỗi câm, rất khó tìm.
        const ALL: &[KeyCode] = &[
            KeyCode::A, KeyCode::B, KeyCode::C, KeyCode::D, KeyCode::E, KeyCode::F,
            KeyCode::G, KeyCode::H, KeyCode::I, KeyCode::J, KeyCode::K, KeyCode::L,
            KeyCode::M, KeyCode::N, KeyCode::O, KeyCode::P, KeyCode::Q, KeyCode::R,
            KeyCode::S, KeyCode::T, KeyCode::U, KeyCode::V, KeyCode::W, KeyCode::X,
            KeyCode::Y, KeyCode::Z,
            KeyCode::Digit0, KeyCode::Digit1, KeyCode::Digit2, KeyCode::Digit3,
            KeyCode::Digit4, KeyCode::Digit5, KeyCode::Digit6, KeyCode::Digit7,
            KeyCode::Digit8, KeyCode::Digit9,
            KeyCode::F1, KeyCode::F2, KeyCode::F3, KeyCode::F4, KeyCode::F5,
            KeyCode::F6, KeyCode::F7, KeyCode::F8, KeyCode::F9, KeyCode::F10,
            KeyCode::F11, KeyCode::F12,
            KeyCode::Escape, KeyCode::Tab, KeyCode::CapsLock, KeyCode::Space,
            KeyCode::Backspace, KeyCode::Enter,
            KeyCode::ShiftLeft, KeyCode::ShiftRight, KeyCode::ControlLeft,
            KeyCode::ControlRight, KeyCode::AltLeft, KeyCode::AltRight,
            KeyCode::MetaLeft, KeyCode::MetaRight,
            KeyCode::ArrowUp, KeyCode::ArrowDown, KeyCode::ArrowLeft, KeyCode::ArrowRight,
            KeyCode::Delete, KeyCode::Home, KeyCode::End, KeyCode::PageUp, KeyCode::PageDown,
            KeyCode::Minus, KeyCode::Equal, KeyCode::BracketLeft, KeyCode::BracketRight,
            KeyCode::Backslash, KeyCode::Semicolon, KeyCode::Quote, KeyCode::Backquote,
            KeyCode::Comma, KeyCode::Period, KeyCode::Slash,
            KeyCode::Numpad0, KeyCode::Numpad1, KeyCode::Numpad2, KeyCode::Numpad3,
            KeyCode::Numpad4, KeyCode::Numpad5, KeyCode::Numpad6, KeyCode::Numpad7,
            KeyCode::Numpad8, KeyCode::Numpad9,
            KeyCode::NumpadAdd, KeyCode::NumpadSubtract, KeyCode::NumpadMultiply,
            KeyCode::NumpadDivide, KeyCode::NumpadDecimal, KeyCode::NumpadEnter,
            KeyCode::NumLock,
        ];

        let mut seen: Vec<(u16, KeyCode)> = Vec::new();
        for code in ALL {
            let Some(vk) = virtual_key(*code) else {
                panic!("{code:?} phải có mã phím trên macOS");
            };
            if let Some((_, other)) = seen.iter().find(|(existing, _)| *existing == vk) {
                panic!("{code:?} và {other:?} cùng ra mã {vk}");
            }
            seen.push((vk, *code));
        }
    }

    #[test]
    fn phim_macos_khong_co_thi_tra_none() {
        for code in [
            KeyCode::Insert,
            KeyCode::PrintScreen,
            KeyCode::ScrollLock,
            KeyCode::Pause,
            KeyCode::ContextMenu,
        ] {
            assert_eq!(virtual_key(code), None, "{code:?}");
        }
    }

    #[test]
    fn modifier_trai_phai_cung_mot_co() {
        assert_eq!(
            modifier_flag(KeyCode::ShiftLeft),
            modifier_flag(KeyCode::ShiftRight)
        );
        assert_eq!(
            modifier_flag(KeyCode::MetaLeft),
            modifier_flag(KeyCode::MetaRight)
        );
        assert_eq!(modifier_flag(KeyCode::A), None);
    }
}
