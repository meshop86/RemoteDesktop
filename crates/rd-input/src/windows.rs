//! Bơm sự kiện qua `SendInput`.
//!
//! `SendInput` chèn sự kiện vào hàng đợi input của hệ thống, ngay sau driver —
//! chỗ tương đương `HIDEventTap` bên macOS. Mọi ứng dụng thường đều nhận được.
//!
//! Hai giới hạn không vượt qua được ở tầng này, cần biết trước:
//!
//! 1. **UIPI.** Tiến trình không nâng quyền không bơm được vào cửa sổ đã nâng
//!    quyền (Task Manager, hộp thoại của trình cài đặt...). `SendInput` trả về
//!    0 kèm `ERROR_ACCESS_DENIED`; ta đổi thành [`InputError::PermissionDenied`]
//!    để người dùng biết phải chạy host với quyền admin.
//! 2. **Secure desktop.** Màn hình UAC và màn hình đăng nhập nằm trên desktop
//!    riêng mà tiến trình người dùng không với tới. Muốn điều khiển qua được
//!    thì host phải chạy như một Windows service dưới tài khoản SYSTEM và tự
//!    chuyển desktop — việc của một crate khác, không phải của file này.

use rd_protocol::{InputEvent, KeyCode, MouseButton};
use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, GetLastError};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE,
    MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

use crate::keymap_windows::{PhysicalKey, physical_key};
use crate::{HeldState, InputError, InputInjector, Result, ScreenGeometry, WheelAccumulator};

/// Nút phụ thứ nhất và thứ hai của chuột, đặt vào `mouseData` của `MOUSEINPUT`.
const XBUTTON1: i32 = 0x0001;
const XBUTTON2: i32 = 0x0002;

/// Một "nấc" bánh xe chuột, theo `WHEEL_DELTA` của Win32.
const WHEEL_DELTA: f32 = 120.0;

/// Số dòng một nấc cuộn đi được, theo mặc định của Windows (`SPI_GETWHEELSCROLLLINES`).
const LINES_PER_NOTCH: f32 = 3.0;

/// Điểm trên một dòng, khớp `POINTS_PER_LINE` bên viewer — đơn vị mà
/// [`InputEvent::Scroll`] mang trên dây.
const POINTS_PER_LINE: f32 = 50.0;

/// Đổi lượng cuộn từ điểm sang đơn vị bánh xe.
const WHEEL_UNITS_PER_POINT: f32 = WHEEL_DELTA / (LINES_PER_NOTCH * POINTS_PER_LINE);

/// Toạ độ tuyệt đối của `SendInput` luôn nằm trong 0..=65535, không phụ thuộc
/// phân giải hay DPI.
const ABSOLUTE_MAX: f64 = 65535.0;

pub struct SendInputInjector {
    geometry: ScreenGeometry,
    held: HeldState,
    /// Phần lẻ của cuộn chưa đủ một đơn vị bánh xe.
    wheel: WheelAccumulator,
}

impl SendInputInjector {
    /// Đổi toạ độ chuẩn hoá sang hệ 0..=65535 mà `MOUSEEVENTF_ABSOLUTE` dùng.
    ///
    /// Đi vòng qua [`ScreenGeometry::to_pixels`] chứ không nhân thẳng, để con
    /// trỏ rơi đúng vào tâm pixel như bên macOS — và để cùng một chỗ lo việc ép
    /// toạ độ lố về trong màn hình.
    fn absolute(&self, x: f32, y: f32) -> (i32, i32) {
        let (px, py) = self.geometry.to_pixels(x, y);
        // Trừ 1 vì `to_pixels` trả về chỉ số pixel cuối cùng, không phải bề
        // rộng. `max(1.0)` chặn chia cho 0 khi màn hình rộng đúng 1 pixel.
        let span_x = (self.geometry.width as f64 - 1.0).max(1.0);
        let span_y = (self.geometry.height as f64 - 1.0).max(1.0);
        let dx = (px / span_x * ABSOLUTE_MAX).round() as i32;
        let dy = (py / span_y * ABSOLUTE_MAX).round() as i32;
        (dx, dy)
    }

    /// Cờ và `mouseData` cho một nút chuột.
    fn button_flags(button: MouseButton, pressed: bool) -> (MOUSE_EVENT_FLAGS, i32) {
        match (button, pressed) {
            (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
            (MouseButton::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1),
            (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
            (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2),
        }
    }

    fn mouse(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: i32) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    // Win32 khai `mouseData` là DWORD nhưng lượng cuộn có dấu;
                    // ép bù hai là đúng cách hệ thống đọc lại nó.
                    mouseData: data as u32,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn keyboard(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn key_input(code: KeyCode, pressed: bool) -> INPUT {
        let up = if pressed {
            KEYBD_EVENT_FLAGS(0)
        } else {
            KEYEVENTF_KEYUP
        };
        match physical_key(code) {
            PhysicalKey::Scan {
                code: scan,
                extended,
            } => {
                let mut flags = KEYEVENTF_SCANCODE | up;
                if extended {
                    flags |= KEYEVENTF_EXTENDEDKEY;
                }
                // `wVk` phải là 0 khi gửi theo scan code, nếu không Windows ưu
                // tiên mã phím ảo và ta mất tính độc lập với layout.
                Self::keyboard(0, scan, flags)
            }
            PhysicalKey::Virtual(vk) => Self::keyboard(vk, 0, up),
        }
    }

    /// Gõ một chuỗi đã qua IME.
    ///
    /// `KEYEVENTF_UNICODE` gửi thẳng ký tự, không tra bảng phím — cách duy nhất
    /// gõ được tiếng Việt hay emoji, vì chúng không ứng với phím vật lý nào.
    /// Ký tự ngoài BMP chiếm hai code unit UTF-16 và phải đi thành hai sự kiện
    /// liền nhau; gửi cả chuỗi trong một lần `SendInput` nên chúng không bị
    /// input thật của người ngồi tại host chen vào giữa.
    fn text_inputs(text: &str) -> Vec<INPUT> {
        let mut inputs = Vec::new();
        for unit in text.encode_utf16() {
            inputs.push(Self::keyboard(0, unit, KEYEVENTF_UNICODE));
            inputs.push(Self::keyboard(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
        }
        inputs
    }
}

/// Đẩy cả loạt sự kiện xuống hệ thống trong một lần gọi.
fn send(inputs: &[INPUT]) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize == inputs.len() {
        return Ok(());
    }
    let error = unsafe { GetLastError() };
    if error == ERROR_ACCESS_DENIED {
        // UIPI: cửa sổ đang có focus chạy ở mức toàn vẹn cao hơn ta.
        return Err(InputError::PermissionDenied);
    }
    Err(InputError::Platform(format!(
        "SendInput chỉ nhận {sent}/{} sự kiện (mã lỗi {})",
        inputs.len(),
        error.0
    )))
}

impl InputInjector for SendInputInjector {
    /// Không có cổng quyền nào để kiểm trước: Windows chỉ từ chối lúc bơm thật,
    /// và chỉ với những cửa sổ nâng quyền. Xem ghi chú đầu file.
    fn open(geometry: ScreenGeometry) -> Result<Self> {
        Ok(Self {
            geometry,
            held: HeldState::default(),
            wheel: WheelAccumulator::default(),
        })
    }

    fn set_geometry(&mut self, geometry: ScreenGeometry) {
        self.geometry = geometry;
    }

    fn inject(&mut self, event: &InputEvent) -> Result<()> {
        match event {
            InputEvent::MouseMove { x, y, .. } => {
                let (dx, dy) = self.absolute(*x, *y);
                send(&[Self::mouse(
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                    dx,
                    dy,
                    0,
                )])
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
                let (dx, dy) = self.absolute(*x, *y);
                let (flags, data) = Self::button_flags(*button, *pressed);
                // Di chuyển và bấm gộp vào một sự kiện: tách đôi thì chuột thật
                // của người ngồi tại host có thể chen vào giữa và cú bấm rơi
                // sang chỗ khác.
                send(&[Self::mouse(
                    flags | MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                    dx,
                    dy,
                    data,
                )])
            }
            InputEvent::Scroll {
                delta_x, delta_y, ..
            } => {
                let (units_x, units_y) = self.wheel.push(
                    delta_x * WHEEL_UNITS_PER_POINT,
                    delta_y * WHEEL_UNITS_PER_POINT,
                );
                let mut inputs = Vec::new();
                if units_y != 0 {
                    inputs.push(Self::mouse(MOUSEEVENTF_WHEEL, 0, 0, units_y));
                }
                if units_x != 0 {
                    inputs.push(Self::mouse(MOUSEEVENTF_HWHEEL, 0, 0, units_x));
                }
                send(&inputs)
            }
            InputEvent::Key { code, pressed } => {
                if !self.held.set_key(*code, *pressed) {
                    return Ok(());
                }
                send(&[Self::key_input(*code, *pressed)])
            }
            InputEvent::Text { text } => send(&Self::text_inputs(text)),
            InputEvent::ReleaseAll => self.release_all(),
        }
    }

    fn release_all(&mut self) -> Result<()> {
        let (keys, buttons) = self.held.drain();
        let mut inputs = Vec::with_capacity(keys.len() + buttons.len());
        for button in buttons {
            let (flags, data) = Self::button_flags(button, false);
            inputs.push(Self::mouse(flags, 0, 0, data));
        }
        for code in keys {
            inputs.push(Self::key_input(code, false));
        }
        // Một lần gọi cho tất cả: `SendInput` hoặc nhận hết hoặc dừng ở sự kiện
        // hỏng, nên không có cảnh nhả được nửa chừng rồi bỏ dở như bên macOS.
        send(&inputs)
    }
}

impl Drop for SendInputInjector {
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
///
/// Con số này bị DPI scaling bóp lại nếu tiến trình không khai báo
/// per-monitor DPI awareness. Với chuột thì không sao — toạ độ tuyệt đối được
/// chuẩn hoá nên hệ số co lại triệt tiêu — nhưng phần capture cần kích thước
/// vật lý thật, nên tiến trình host phải tự khai DPI awareness lúc khởi động.
pub fn main_screen_geometry() -> Result<ScreenGeometry> {
    let width = unsafe { GetSystemMetrics(SM_CXSCREEN) } as f32;
    let height = unsafe { GetSystemMetrics(SM_CYSCREEN) } as f32;
    if width <= 0.0 || height <= 0.0 {
        return Err(InputError::Platform(
            "không đọc được kích thước màn hình".into(),
        ));
    }
    Ok(ScreenGeometry { width, height })
}
