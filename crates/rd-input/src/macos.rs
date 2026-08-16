//! Bơm sự kiện qua CoreGraphics (`CGEvent`).
//!
//! Sự kiện được đẩy vào `HIDEventTap` — chỗ thấp nhất trong chuỗi xử lý của hệ
//! điều hành, ngay sau driver phần cứng. Đẩy vào đó thì mọi ứng dụng đều nhận
//! được, kể cả những ứng dụng đọc input ở mức thấp như game hay máy ảo.
//!
//! Đổi lại, macOS coi đây là hành vi nhạy cảm và đòi quyền Accessibility.

use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventType,
    CGMouseButton, CGScrollEventUnit,
};
use rd_protocol::{InputEvent, KeyCode, MouseButton};

use crate::keymap_macos::{is_numpad, modifier_flag, virtual_key};
use crate::{HeldState, InputError, InputInjector, Result, ScreenGeometry};

/// Kiểm tra quyền Accessibility mà không bật hộp thoại xin quyền.
///
/// Gọi trước khi bơm để báo lỗi rõ ràng: nếu không, `CGEvent::post` **im lặng
/// không làm gì cả** — không lỗi, không trả về gì — và người dùng chỉ thấy điều
/// khiển từ xa "bị đơ" mà không hiểu vì sao.
pub fn is_trusted() -> bool {
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
    }
    unsafe { AXIsProcessTrusted() }
}

pub struct CgInjector {
    source: objc2_core_foundation::CFRetained<CGEventSource>,
    geometry: ScreenGeometry,
    held: HeldState,
    /// Cờ modifier hiện tại, dựng lại từ `held` mỗi lần đổi. Mọi sự kiện đều
    /// phải mang cờ này theo.
    flags: CGEventFlags,
    /// Vị trí chuột gần nhất. Sự kiện nhấn nút cũng phải mang toạ độ, và giữa
    /// hai lần di chuyển ta cần biết chuột đang ở đâu để phát sự kiện kéo.
    cursor: CGPoint,
}

// CGEvent và CGEventSource không gắn với luồng nào; chuyển sang luồng khác được.
// Chỉ `Send` chứ không `Sync` vì mọi thao tác đều đi qua `&mut self`.
unsafe impl Send for CgInjector {}

impl CgInjector {
    fn refresh_flags(&mut self) {
        let mut flags = CGEventFlags::empty();
        for code in self.held.keys() {
            if let Some(bit) = modifier_flag(*code) {
                flags |= CGEventFlags::from_bits_retain(bit);
            }
        }
        self.flags = flags;
    }

    /// Kiểu sự kiện chuột tương ứng với nút và trạng thái nhấn/nhả.
    fn mouse_type(button: MouseButton, pressed: bool) -> CGEventType {
        match (button, pressed) {
            (MouseButton::Left, true) => CGEventType::LeftMouseDown,
            (MouseButton::Left, false) => CGEventType::LeftMouseUp,
            (MouseButton::Right, true) => CGEventType::RightMouseDown,
            (MouseButton::Right, false) => CGEventType::RightMouseUp,
            (_, true) => CGEventType::OtherMouseDown,
            (_, false) => CGEventType::OtherMouseUp,
        }
    }

    /// Số hiệu nút mà macOS dùng. Back/Forward không có hằng số riêng, chúng chỉ
    /// là nút thứ 3 và 4 của chuột nhiều nút.
    fn mouse_button(button: MouseButton) -> CGMouseButton {
        match button {
            MouseButton::Left => CGMouseButton::Left,
            MouseButton::Right => CGMouseButton::Right,
            MouseButton::Middle => CGMouseButton::Center,
            MouseButton::Back => CGMouseButton(3),
            MouseButton::Forward => CGMouseButton(4),
        }
    }

    /// Kiểu sự kiện khi chuột di chuyển: đang giữ nút thì là kéo, không thì là
    /// di chuyển thường. Gửi sai kiểu sẽ làm hỏng thao tác kéo-thả và bôi đen.
    fn move_type(&self) -> CGEventType {
        match self.held.buttons().first() {
            Some(MouseButton::Left) => CGEventType::LeftMouseDragged,
            Some(MouseButton::Right) => CGEventType::RightMouseDragged,
            Some(_) => CGEventType::OtherMouseDragged,
            None => CGEventType::MouseMoved,
        }
    }

    fn post_mouse(
        &self,
        event_type: CGEventType,
        button: CGMouseButton,
        point: CGPoint,
    ) -> Result<()> {
        let event = CGEvent::new_mouse_event(Some(&self.source), event_type, point, button)
            .ok_or_else(|| InputError::Platform("không tạo được sự kiện chuột".into()))?;
        CGEvent::set_flags(Some(&event), self.flags);
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
        Ok(())
    }

    fn post_key(&self, code: KeyCode, pressed: bool) -> Result<()> {
        let vk = virtual_key(code).ok_or(InputError::UnsupportedKey(code))?;
        let event = CGEvent::new_keyboard_event(Some(&self.source), vk, pressed)
            .ok_or_else(|| InputError::Platform("không tạo được sự kiện phím".into()))?;

        let mut flags = self.flags;
        if is_numpad(code) {
            flags |= CGEventFlags::MaskNumericPad;
        }
        CGEvent::set_flags(Some(&event), flags);
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
        Ok(())
    }

    /// Gõ một chuỗi đã qua IME.
    ///
    /// Không tra bảng phím: ta gắn thẳng chuỗi Unicode vào một sự kiện phím
    /// rỗng. Đây là cách duy nhất gõ được tiếng Việt hay emoji, vì những ký tự
    /// đó không ứng với phím vật lý nào.
    fn post_text(&self, text: &str) -> Result<()> {
        let utf16: Vec<u16> = text.encode_utf16().collect();
        if utf16.is_empty() {
            return Ok(());
        }
        for pressed in [true, false] {
            let event = CGEvent::new_keyboard_event(Some(&self.source), 0, pressed)
                .ok_or_else(|| InputError::Platform("không tạo được sự kiện phím".into()))?;
            unsafe {
                CGEvent::keyboard_set_unicode_string(
                    Some(&event),
                    utf16.len() as u64,
                    utf16.as_ptr(),
                );
            }
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
        }
        Ok(())
    }
}

impl InputInjector for CgInjector {
    fn open(geometry: ScreenGeometry) -> Result<Self> {
        if !is_trusted() {
            return Err(InputError::PermissionDenied);
        }
        // HIDSystemState (không phải PrivateState): sự kiện được trộn vào đúng
        // trạng thái bàn phím/chuột thật của máy, nên phím modifier mà người
        // ngồi tại host đang giữ cũng được tính vào.
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .ok_or_else(|| InputError::Platform("không tạo được CGEventSource".into()))?;
        Ok(Self {
            source,
            geometry,
            held: HeldState::default(),
            flags: CGEventFlags::empty(),
            cursor: CGPoint { x: 0.0, y: 0.0 },
        })
    }

    fn set_geometry(&mut self, geometry: ScreenGeometry) {
        self.geometry = geometry;
    }

    fn inject(&mut self, event: &InputEvent) -> Result<()> {
        match event {
            InputEvent::MouseMove { x, y, .. } => {
                let (px, py) = self.geometry.to_pixels(*x, *y);
                self.cursor = CGPoint { x: px, y: py };
                let event_type = self.move_type();
                let button = self
                    .held
                    .buttons()
                    .first()
                    .map(|button| Self::mouse_button(*button))
                    .unwrap_or(CGMouseButton::Left);
                self.post_mouse(event_type, button, self.cursor)
            }
            InputEvent::MouseButton {
                button,
                pressed,
                x,
                y,
                ..
            } => {
                if !self.held.set_button(*button, *pressed) {
                    return Ok(());
                }
                let (px, py) = self.geometry.to_pixels(*x, *y);
                self.cursor = CGPoint { x: px, y: py };
                self.post_mouse(
                    Self::mouse_type(*button, *pressed),
                    Self::mouse_button(*button),
                    self.cursor,
                )
            }
            InputEvent::Scroll {
                delta_x, delta_y, ..
            } => {
                // Đơn vị Pixel chứ không phải Line: trackpad và chuột hiện đại
                // cuộn theo pixel, dùng Line sẽ giật từng nấc.
                let event = CGEvent::new_scroll_wheel_event2(
                    Some(&self.source),
                    CGScrollEventUnit::Pixel,
                    2,
                    delta_y.round() as i32,
                    delta_x.round() as i32,
                    0,
                )
                .ok_or_else(|| InputError::Platform("không tạo được sự kiện cuộn".into()))?;
                CGEvent::set_flags(Some(&event), self.flags);
                CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
                Ok(())
            }
            InputEvent::Key { code, pressed } => {
                if !self.held.set_key(*code, *pressed) {
                    return Ok(());
                }
                // Cờ phải cập nhật *trước* khi bơm phím nhấn (để Cmd+C mang cờ
                // Cmd) nhưng *sau* khi bơm phím nhả (để chính sự kiện nhả Cmd
                // vẫn còn mang cờ Cmd, đúng như bàn phím thật).
                if *pressed {
                    self.refresh_flags();
                    self.post_key(*code, true)
                } else {
                    let result = self.post_key(*code, false);
                    self.refresh_flags();
                    result
                }
            }
            InputEvent::Text { text } => self.post_text(text),
            InputEvent::ReleaseAll => self.release_all(),
        }
    }

    fn release_all(&mut self) -> Result<()> {
        let (keys, buttons) = self.held.drain();
        self.flags = CGEventFlags::empty();

        let mut first_error = None;
        for button in buttons {
            let result = self.post_mouse(
                Self::mouse_type(button, false),
                Self::mouse_button(button),
                self.cursor,
            );
            // Cố nhả hết mọi thứ rồi mới báo lỗi: bỏ dở giữa chừng đúng là để
            // lại phím kẹt — thứ mà hàm này sinh ra để tránh.
            if let Err(err) = result {
                first_error.get_or_insert(err);
            }
        }
        for code in keys {
            if let Err(err) = self.post_key(code, false) {
                first_error.get_or_insert(err);
            }
        }
        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

impl Drop for CgInjector {
    fn drop(&mut self) {
        if self.held.is_empty() {
            return;
        }
        if let Err(err) = self.release_all() {
            tracing::warn!(%err, "không nhả hết phím lúc đóng");
        }
    }
}

/// Kích thước màn hình chính, tính bằng pixel.
pub fn main_screen_geometry() -> Result<ScreenGeometry> {
    use objc2_core_graphics::{CGMainDisplayID, CGDisplayPixelsHigh, CGDisplayPixelsWide};
    let display = CGMainDisplayID();
    let width = CGDisplayPixelsWide(display) as f32;
    let height = CGDisplayPixelsHigh(display) as f32;
    if width <= 0.0 || height <= 0.0 {
        return Err(InputError::Platform("không đọc được kích thước màn hình".into()));
    }
    Ok(ScreenGeometry { width, height })
}
