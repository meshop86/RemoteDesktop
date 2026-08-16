//! Ánh xạ [`KeyCode`] sang phím vật lý của Windows.
//!
//! Ta gửi **scan code** chứ không phải mã phím ảo (VK). Lý do giống bên macOS:
//! [`KeyCode`] mô tả *vị trí vật lý* trên bàn phím, còn VK thì phụ thuộc layout
//! — trên bàn phím AZERTY, phím ở vị trí `Q` của QWERTY gửi `VK_A`. Đưa scan
//! code cho `SendInput` thì Windows tự dịch qua layout của host, nên bấm đúng
//! phím ở đúng vị trí dù hai máy khác layout.
//!
//! Các số dưới đây là bảng **PS/2 scan code Set 1**, thứ mà bàn phím USB hiện
//! đại vẫn giả lập và Windows vẫn nhận.

use rd_protocol::KeyCode;

/// Cách gửi một phím xuống `SendInput`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysicalKey {
    /// Theo vị trí vật lý. `extended` là tiền tố `E0` — thứ phân biệt hai phím
    /// dùng chung một scan code, ví dụ Enter (`0x1C`) với Enter trên numpad
    /// (`E0 1C`), hay mũi tên với số trên numpad khi tắt NumLock.
    Scan { code: u16, extended: bool },
    /// Theo mã phím ảo. Chỉ dành cho Pause: scan code của nó là chuỗi ba byte
    /// `E1 1D 45`, không diễn tả được bằng một sự kiện `SendInput`.
    Virtual(u16),
}

/// `VK_PAUSE`.
const VK_PAUSE: u16 = 0x13;

pub fn physical_key(code: KeyCode) -> PhysicalKey {
    use KeyCode::*;

    // Phím mở rộng: cùng scan code với phím khác, phân biệt bằng tiền tố E0.
    let extended = match code {
        NumpadEnter => 0x1C,
        ControlRight => 0x1D,
        NumpadDivide => 0x35,
        PrintScreen => 0x37,
        AltRight => 0x38,
        Home => 0x47,
        ArrowUp => 0x48,
        PageUp => 0x49,
        ArrowLeft => 0x4B,
        ArrowRight => 0x4D,
        End => 0x4F,
        ArrowDown => 0x50,
        PageDown => 0x51,
        Insert => 0x52,
        Delete => 0x53,
        MetaLeft => 0x5B,
        MetaRight => 0x5C,
        ContextMenu => 0x5D,
        Pause => return PhysicalKey::Virtual(VK_PAUSE),
        _ => 0,
    };
    if extended != 0 {
        return PhysicalKey::Scan {
            code: extended,
            extended: true,
        };
    }

    let plain = match code {
        Escape => 0x01,
        Digit1 => 0x02,
        Digit2 => 0x03,
        Digit3 => 0x04,
        Digit4 => 0x05,
        Digit5 => 0x06,
        Digit6 => 0x07,
        Digit7 => 0x08,
        Digit8 => 0x09,
        Digit9 => 0x0A,
        Digit0 => 0x0B,
        Minus => 0x0C,
        Equal => 0x0D,
        Backspace => 0x0E,

        Tab => 0x0F,
        Q => 0x10,
        W => 0x11,
        E => 0x12,
        R => 0x13,
        T => 0x14,
        Y => 0x15,
        U => 0x16,
        I => 0x17,
        O => 0x18,
        P => 0x19,
        BracketLeft => 0x1A,
        BracketRight => 0x1B,
        Enter => 0x1C,

        ControlLeft => 0x1D,
        A => 0x1E,
        S => 0x1F,
        D => 0x20,
        F => 0x21,
        G => 0x22,
        H => 0x23,
        J => 0x24,
        K => 0x25,
        L => 0x26,
        Semicolon => 0x27,
        Quote => 0x28,
        Backquote => 0x29,

        ShiftLeft => 0x2A,
        Backslash => 0x2B,
        Z => 0x2C,
        X => 0x2D,
        C => 0x2E,
        V => 0x2F,
        B => 0x30,
        N => 0x31,
        M => 0x32,
        Comma => 0x33,
        Period => 0x34,
        Slash => 0x35,
        ShiftRight => 0x36,

        NumpadMultiply => 0x37,
        AltLeft => 0x38,
        Space => 0x39,
        CapsLock => 0x3A,

        F1 => 0x3B,
        F2 => 0x3C,
        F3 => 0x3D,
        F4 => 0x3E,
        F5 => 0x3F,
        F6 => 0x40,
        F7 => 0x41,
        F8 => 0x42,
        F9 => 0x43,
        F10 => 0x44,

        NumLock => 0x45,
        ScrollLock => 0x46,

        Numpad7 => 0x47,
        Numpad8 => 0x48,
        Numpad9 => 0x49,
        NumpadSubtract => 0x4A,
        Numpad4 => 0x4B,
        Numpad5 => 0x4C,
        Numpad6 => 0x4D,
        NumpadAdd => 0x4E,
        Numpad1 => 0x4F,
        Numpad2 => 0x50,
        Numpad3 => 0x51,
        Numpad0 => 0x52,
        NumpadDecimal => 0x53,

        F11 => 0x57,
        F12 => 0x58,

        // Đã xử lý ở bảng mở rộng phía trên.
        NumpadEnter | ControlRight | NumpadDivide | PrintScreen | AltRight | Home | ArrowUp
        | PageUp | ArrowLeft | ArrowRight | End | ArrowDown | PageDown | Insert | Delete
        | MetaLeft | MetaRight | ContextMenu | Pause => unreachable!(),
    };
    PhysicalKey::Scan {
        code: plain,
        extended: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mọi [`KeyCode`] trong giao thức. Giữ tay vì Rust không liệt kê biến thể
    /// enum được; thiếu một phím ở đây là phím đó không bao giờ được kiểm.
    const ALL: &[KeyCode] = &[
        KeyCode::A, KeyCode::B, KeyCode::C, KeyCode::D, KeyCode::E, KeyCode::F,
        KeyCode::G, KeyCode::H, KeyCode::I, KeyCode::J, KeyCode::K, KeyCode::L,
        KeyCode::M, KeyCode::N, KeyCode::O, KeyCode::P, KeyCode::Q, KeyCode::R,
        KeyCode::S, KeyCode::T, KeyCode::U, KeyCode::V, KeyCode::W, KeyCode::X,
        KeyCode::Y, KeyCode::Z,
        KeyCode::Digit0, KeyCode::Digit1, KeyCode::Digit2, KeyCode::Digit3,
        KeyCode::Digit4, KeyCode::Digit5, KeyCode::Digit6, KeyCode::Digit7,
        KeyCode::Digit8, KeyCode::Digit9,
        KeyCode::F1, KeyCode::F2, KeyCode::F3, KeyCode::F4, KeyCode::F5, KeyCode::F6,
        KeyCode::F7, KeyCode::F8, KeyCode::F9, KeyCode::F10, KeyCode::F11, KeyCode::F12,
        KeyCode::Escape, KeyCode::Tab, KeyCode::CapsLock, KeyCode::Space,
        KeyCode::Backspace, KeyCode::Enter,
        KeyCode::ShiftLeft, KeyCode::ShiftRight, KeyCode::ControlLeft,
        KeyCode::ControlRight, KeyCode::AltLeft, KeyCode::AltRight,
        KeyCode::MetaLeft, KeyCode::MetaRight,
        KeyCode::ArrowUp, KeyCode::ArrowDown, KeyCode::ArrowLeft, KeyCode::ArrowRight,
        KeyCode::Insert, KeyCode::Delete, KeyCode::Home, KeyCode::End,
        KeyCode::PageUp, KeyCode::PageDown,
        KeyCode::Minus, KeyCode::Equal, KeyCode::BracketLeft, KeyCode::BracketRight,
        KeyCode::Backslash, KeyCode::Semicolon, KeyCode::Quote, KeyCode::Backquote,
        KeyCode::Comma, KeyCode::Period, KeyCode::Slash,
        KeyCode::Numpad0, KeyCode::Numpad1, KeyCode::Numpad2, KeyCode::Numpad3,
        KeyCode::Numpad4, KeyCode::Numpad5, KeyCode::Numpad6, KeyCode::Numpad7,
        KeyCode::Numpad8, KeyCode::Numpad9,
        KeyCode::NumpadAdd, KeyCode::NumpadSubtract, KeyCode::NumpadMultiply,
        KeyCode::NumpadDivide, KeyCode::NumpadDecimal, KeyCode::NumpadEnter,
        KeyCode::NumLock,
        KeyCode::PrintScreen, KeyCode::ScrollLock, KeyCode::Pause, KeyCode::ContextMenu,
    ];

    /// Trùng mã nghĩa là bấm phím này ra phím kia — lỗi câm, rất khó tìm. Phím
    /// thường và phím mở rộng dùng hai không gian mã riêng nên xét riêng.
    #[test]
    fn khong_co_hai_phim_trung_ma() {
        let mut seen: Vec<(PhysicalKey, KeyCode)> = Vec::new();
        for code in ALL {
            let key = physical_key(*code);
            if let Some((_, other)) = seen.iter().find(|(existing, _)| *existing == key) {
                panic!("{code:?} và {other:?} cùng ra {key:?}");
            }
            seen.push((key, *code));
        }
        assert_eq!(seen.len(), ALL.len());
    }

    /// Windows phân biệt Enter với Enter-numpad, mũi tên với số numpad... chỉ
    /// bằng cờ mở rộng. Quên cờ này thì mũi tên lên thành số 8 khi bật NumLock.
    #[test]
    fn phim_mo_rong_dung_chung_ma_voi_phim_thuong() {
        let enter = physical_key(KeyCode::Enter);
        let numpad_enter = physical_key(KeyCode::NumpadEnter);
        assert_eq!(
            enter,
            PhysicalKey::Scan {
                code: 0x1C,
                extended: false
            }
        );
        assert_eq!(
            numpad_enter,
            PhysicalKey::Scan {
                code: 0x1C,
                extended: true
            }
        );

        // Mũi tên lên và Numpad8 cũng vậy.
        assert_eq!(
            physical_key(KeyCode::ArrowUp),
            PhysicalKey::Scan {
                code: 0x48,
                extended: true
            }
        );
        assert_eq!(
            physical_key(KeyCode::Numpad8),
            PhysicalKey::Scan {
                code: 0x48,
                extended: false
            }
        );
    }

    /// Modifier trái và phải phải ra hai phím khác nhau — nhiều phần mềm phân
    /// biệt được, và AltGr trên layout châu Âu chính là Alt phải.
    #[test]
    fn modifier_trai_phai_khac_nhau() {
        for (left, right) in [
            (KeyCode::ControlLeft, KeyCode::ControlRight),
            (KeyCode::AltLeft, KeyCode::AltRight),
            (KeyCode::ShiftLeft, KeyCode::ShiftRight),
            (KeyCode::MetaLeft, KeyCode::MetaRight),
        ] {
            assert_ne!(physical_key(left), physical_key(right), "{left:?}");
        }
    }

    #[test]
    fn pause_di_duong_ma_phim_ao() {
        assert_eq!(physical_key(KeyCode::Pause), PhysicalKey::Virtual(VK_PAUSE));
    }
}
