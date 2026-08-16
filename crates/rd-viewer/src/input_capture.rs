//! Đổi input của cửa sổ viewer thành [`InputEvent`] để gửi sang host.
//!
//! Ba chỗ dễ sai, đều nằm ở chi tiết của egui chứ không ở ý tưởng:
//!
//! 1. **Toạ độ.** Video được vẽ vừa khung theo tỉ lệ nên thường có viền đen hai
//!    bên. Toạ độ phải chuẩn hoá theo *ô video*, không phải theo cửa sổ, nếu
//!    không chuột trên host sẽ lệch đúng bằng bề rộng viền.
//! 2. **Gõ hai lần.** Một lần bấm phím sinh ra cả `Event::Key` lẫn
//!    `Event::Text`. Gửi cả hai thì host nhận hai ký tự.
//! 3. **Cmd+C bị nuốt.** egui-winit chặn Cmd+C/X/V và đổi thành
//!    `Event::Copy`/`Cut`/`Paste` — sự kiện phím **không** được phát ra. Không
//!    dựng lại thì viewer không sao chép được gì trên máy từ xa.

use egui::{Event, Key, MouseWheelUnit, Pos2, Rect};
use rd_protocol::{InputEvent, KeyCode, MouseButton};

/// Chỉ số màn hình host. Mới hỗ trợ một màn hình; khi nào chọn được màn hình
/// thì giá trị này lấy từ lựa chọn của người dùng.
const MONITOR: u8 = 0;

/// Quy đổi cuộn theo "dòng" ra điểm. Bằng đúng mặc định của egui, để cuộn
/// trong viewer đi được quãng như cuộn ngay trên máy host.
const POINTS_PER_LINE: f32 = 50.0;

/// Quy đổi cuộn theo "trang". Chuột thường không sinh đơn vị này, chỉ vài loại
/// bàn phím/thiết bị hỗ trợ; lấy xấp xỉ một màn hình.
const POINTS_PER_PAGE: f32 = 800.0;

/// Bộ dịch input, có nhớ trạng thái.
///
/// Cần nhớ vì hai quyết định phụ thuộc vào quá khứ: có đang kéo chuột không (để
/// biết sự kiện ra ngoài ô video là kéo hợp lệ hay là thao tác với giao diện
/// viewer), và con trỏ có đang nằm trong ô video không (để không cuộn nhầm).
#[derive(Debug, Default)]
pub struct InputCapture {
    held_buttons: Vec<MouseButton>,
    pointer_inside: bool,
}

impl InputCapture {
    pub fn new() -> Self {
        Self::default()
    }

    /// Dịch trọn gói sự kiện của một khung hình.
    ///
    /// Nhận `video` là ô đang vẽ video, tính bằng điểm trong cùng hệ toạ độ với
    /// sự kiện egui.
    pub fn translate(&mut self, events: &[Event], video: Rect) -> Vec<InputEvent> {
        let mut out = Vec::new();
        // Trong cùng một khung, egui đẩy `Event::Key` trước `Event::Text` của
        // cùng một lần bấm. Nhờ vậy chỉ cần một lá cờ chạy dọc theo vòng lặp là
        // đủ để bỏ bản `Text` thừa mà vẫn giữ nguyên thứ tự các sự kiện khác.
        let mut key_pressed = false;

        for event in events {
            match event {
                Event::PointerMoved(pos) => {
                    self.pointer_inside = inside(*pos, video);
                    // Đang kéo thì vẫn gửi dù con trỏ ra ngoài ô video: người
                    // dùng bôi đen hay kéo cửa sổ thường lia quá mép.
                    let point = if self.held_buttons.is_empty() {
                        self.pointer_inside.then(|| clamp_to(*pos, video)).flatten()
                    } else {
                        clamp_to(*pos, video)
                    };
                    if let Some((x, y)) = point {
                        out.push(InputEvent::MouseMove {
                            monitor: MONITOR,
                            x,
                            y,
                        });
                    }
                }

                Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    ..
                } => {
                    let Some(button) = map_button(*button) else {
                        continue;
                    };
                    let Some((x, y)) = clamp_to(*pos, video) else {
                        continue;
                    };
                    if *pressed {
                        // Bấm ngoài ô video là bấm vào phần giao diện của
                        // viewer (viền đen, HUD) — không phải lệnh cho host.
                        if !inside(*pos, video) {
                            continue;
                        }
                        if !self.held_buttons.contains(&button) {
                            self.held_buttons.push(button);
                        }
                    } else if let Some(index) =
                        self.held_buttons.iter().position(|held| *held == button)
                    {
                        self.held_buttons.remove(index);
                    } else {
                        // Nhả một nút chưa từng bấm xuống host: bỏ qua, nếu
                        // không host nhận sự kiện nhả lạc lõng.
                        continue;
                    }
                    out.push(InputEvent::MouseButton {
                        monitor: MONITOR,
                        button,
                        pressed: *pressed,
                        x,
                        y,
                    });
                }

                Event::PointerGone => self.pointer_inside = false,

                Event::MouseWheel { unit, delta, .. } => {
                    if !self.pointer_inside {
                        continue;
                    }
                    let scale = match unit {
                        MouseWheelUnit::Point => 1.0,
                        MouseWheelUnit::Line => POINTS_PER_LINE,
                        MouseWheelUnit::Page => POINTS_PER_PAGE,
                    };
                    out.push(InputEvent::Scroll {
                        monitor: MONITOR,
                        delta_x: delta.x * scale,
                        delta_y: delta.y * scale,
                    });
                }

                Event::Key {
                    key,
                    physical_key,
                    pressed,
                    repeat,
                    ..
                } => {
                    // Hệ điều hành của host tự lặp phím khi giữ; chuyển tiếp
                    // bản lặp của viewer nữa là lặp chồng lên nhau.
                    if *repeat {
                        continue;
                    }
                    // Ưu tiên phím vật lý: bên host ta bơm theo vị trí phím,
                    // nên hai máy khác layout vẫn ra đúng phím người dùng bấm.
                    let Some(code) = map_key(physical_key.unwrap_or(*key)) else {
                        continue;
                    };
                    if *pressed {
                        key_pressed = true;
                    }
                    out.push(InputEvent::Key {
                        code,
                        pressed: *pressed,
                    });
                }

                // egui-winit nuốt tổ hợp sao chép/cắt/dán và chỉ báo lại ý
                // nghĩa. Dựng lại phím chữ; phím Cmd thì người dùng đang giữ
                // thật nên đã được chuyển tiếp từ trước, và sự kiện nhả của
                // chính phím chữ này cũng vẫn tới bình thường (egui chỉ chặn
                // lượt nhấn).
                Event::Copy => {
                    key_pressed = true;
                    out.push(InputEvent::Key {
                        code: KeyCode::C,
                        pressed: true,
                    });
                }
                Event::Cut => {
                    key_pressed = true;
                    out.push(InputEvent::Key {
                        code: KeyCode::X,
                        pressed: true,
                    });
                }
                Event::Paste(_) => {
                    // Bỏ nội dung clipboard của máy viewer: host dán clipboard
                    // của chính nó. Đồng bộ clipboard là việc riêng, làm sau.
                    key_pressed = true;
                    out.push(InputEvent::Key {
                        code: KeyCode::V,
                        pressed: true,
                    });
                }

                Event::Text(text) => {
                    // Có phím vật lý rồi thì host tự sinh ký tự — gửi thêm chữ
                    // là gõ hai lần.
                    if !key_pressed && !text.is_empty() {
                        out.push(InputEvent::Text { text: text.clone() });
                    }
                }

                // IME đã nuốt phím gốc nên không có `Event::Key` nào đi kèm.
                // Đây là đường duy nhất gõ được tiếng Việt hay emoji.
                Event::Ime(egui::ImeEvent::Commit(text)) if !text.is_empty() => {
                    out.push(InputEvent::Text { text: text.clone() })
                }

                // Mất focus giữa lúc đang giữ phím là nguồn gốc của phím kẹt:
                // sự kiện nhả sẽ đi tới cửa sổ khác, host giữ phím mãi mãi.
                Event::WindowFocused(false) => {
                    self.held_buttons.clear();
                    self.pointer_inside = false;
                    out.push(InputEvent::ReleaseAll);
                }

                _ => {}
            }
        }

        out
    }

    /// Quên hết trạng thái và yêu cầu host nhả sạch. Gọi khi người dùng tắt
    /// điều khiển hoặc khi mất kết nối.
    pub fn reset(&mut self) -> InputEvent {
        self.held_buttons.clear();
        self.pointer_inside = false;
        InputEvent::ReleaseAll
    }

    pub fn is_dragging(&self) -> bool {
        !self.held_buttons.is_empty()
    }
}

fn inside(pos: Pos2, video: Rect) -> bool {
    video.contains(pos)
}

/// Đổi toạ độ điểm sang tỉ lệ [0,1] trong ô video. `None` khi ô chưa có kích
/// thước — khung hình đầu tiên chưa về thì `video` rỗng.
fn clamp_to(pos: Pos2, video: Rect) -> Option<(f32, f32)> {
    if video.width() <= 0.0 || video.height() <= 0.0 {
        return None;
    }
    let x = ((pos.x - video.min.x) / video.width()).clamp(0.0, 1.0);
    let y = ((pos.y - video.min.y) / video.height()).clamp(0.0, 1.0);
    Some((x, y))
}

fn map_button(button: egui::PointerButton) -> Option<MouseButton> {
    Some(match button {
        egui::PointerButton::Primary => MouseButton::Left,
        egui::PointerButton::Secondary => MouseButton::Right,
        egui::PointerButton::Middle => MouseButton::Middle,
        egui::PointerButton::Extra1 => MouseButton::Back,
        egui::PointerButton::Extra2 => MouseButton::Forward,
    })
}

/// Ánh xạ phím egui sang mã của giao thức.
///
/// egui gộp numpad vào phím số thường và mô tả dấu câu theo *ký tự* (`Pipe`,
/// `Questionmark`...) chứ không theo vị trí. Ta trả về phím vật lý sinh ra ký
/// tự đó trên bàn phím US; phần Shift đi kèm đã nằm ở sự kiện phím Shift riêng.
pub fn map_key(key: Key) -> Option<KeyCode> {
    Some(match key {
        Key::ArrowDown => KeyCode::ArrowDown,
        Key::ArrowLeft => KeyCode::ArrowLeft,
        Key::ArrowRight => KeyCode::ArrowRight,
        Key::ArrowUp => KeyCode::ArrowUp,

        Key::Escape => KeyCode::Escape,
        Key::Tab => KeyCode::Tab,
        Key::Backspace => KeyCode::Backspace,
        Key::Enter => KeyCode::Enter,
        Key::Space => KeyCode::Space,

        Key::Insert => KeyCode::Insert,
        Key::Delete => KeyCode::Delete,
        Key::Home => KeyCode::Home,
        Key::End => KeyCode::End,
        Key::PageUp => KeyCode::PageUp,
        Key::PageDown => KeyCode::PageDown,

        Key::Colon | Key::Semicolon => KeyCode::Semicolon,
        Key::Comma => KeyCode::Comma,
        Key::Backslash | Key::Pipe => KeyCode::Backslash,
        Key::Slash | Key::Questionmark => KeyCode::Slash,
        Key::Exclamationmark => KeyCode::Digit1,
        Key::OpenBracket | Key::OpenCurlyBracket => KeyCode::BracketLeft,
        Key::CloseBracket | Key::CloseCurlyBracket => KeyCode::BracketRight,
        Key::Backtick => KeyCode::Backquote,
        Key::Minus => KeyCode::Minus,
        Key::Period => KeyCode::Period,
        Key::Plus | Key::Equals => KeyCode::Equal,
        Key::Quote => KeyCode::Quote,

        Key::Num0 => KeyCode::Digit0,
        Key::Num1 => KeyCode::Digit1,
        Key::Num2 => KeyCode::Digit2,
        Key::Num3 => KeyCode::Digit3,
        Key::Num4 => KeyCode::Digit4,
        Key::Num5 => KeyCode::Digit5,
        Key::Num6 => KeyCode::Digit6,
        Key::Num7 => KeyCode::Digit7,
        Key::Num8 => KeyCode::Digit8,
        Key::Num9 => KeyCode::Digit9,

        Key::A => KeyCode::A,
        Key::B => KeyCode::B,
        Key::C => KeyCode::C,
        Key::D => KeyCode::D,
        Key::E => KeyCode::E,
        Key::F => KeyCode::F,
        Key::G => KeyCode::G,
        Key::H => KeyCode::H,
        Key::I => KeyCode::I,
        Key::J => KeyCode::J,
        Key::K => KeyCode::K,
        Key::L => KeyCode::L,
        Key::M => KeyCode::M,
        Key::N => KeyCode::N,
        Key::O => KeyCode::O,
        Key::P => KeyCode::P,
        Key::Q => KeyCode::Q,
        Key::R => KeyCode::R,
        Key::S => KeyCode::S,
        Key::T => KeyCode::T,
        Key::U => KeyCode::U,
        Key::V => KeyCode::V,
        Key::W => KeyCode::W,
        Key::X => KeyCode::X,
        Key::Y => KeyCode::Y,
        Key::Z => KeyCode::Z,

        Key::F1 => KeyCode::F1,
        Key::F2 => KeyCode::F2,
        Key::F3 => KeyCode::F3,
        Key::F4 => KeyCode::F4,
        Key::F5 => KeyCode::F5,
        Key::F6 => KeyCode::F6,
        Key::F7 => KeyCode::F7,
        Key::F8 => KeyCode::F8,
        Key::F9 => KeyCode::F9,
        Key::F10 => KeyCode::F10,
        Key::F11 => KeyCode::F11,
        Key::F12 => KeyCode::F12,

        Key::ShiftLeft => KeyCode::ShiftLeft,
        Key::ShiftRight => KeyCode::ShiftRight,
        Key::ControlLeft => KeyCode::ControlLeft,
        Key::ControlRight => KeyCode::ControlRight,
        Key::AltLeft => KeyCode::AltLeft,
        Key::AltRight => KeyCode::AltRight,
        Key::SuperLeft => KeyCode::MetaLeft,
        Key::SuperRight => KeyCode::MetaRight,

        // F13 trở lên, phím media (Copy/Cut/Paste rời, BrowserBack) và phím thứ
        // 102 của bàn phím ISO chưa có trong giao thức.
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video() -> Rect {
        // Ô video 800x600 nằm giữa cửa sổ, viền đen 100 điểm mỗi bên.
        Rect::from_min_size(Pos2::new(100.0, 50.0), egui::vec2(800.0, 600.0))
    }

    fn key_event(key: Key, pressed: bool) -> Event {
        Event::Key {
            key,
            physical_key: Some(key),
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    #[test]
    fn toa_do_chuan_hoa_theo_o_video_khong_theo_cua_so() {
        let mut capture = InputCapture::new();
        // Giữa ô video, không phải giữa cửa sổ.
        let events = [Event::PointerMoved(Pos2::new(500.0, 350.0))];
        let out = capture.translate(&events, video());
        assert_eq!(
            out,
            vec![InputEvent::MouseMove {
                monitor: 0,
                x: 0.5,
                y: 0.5
            }]
        );
    }

    #[test]
    fn chuot_o_vien_den_thi_khong_gui() {
        let mut capture = InputCapture::new();
        // Bên trái ô video: người dùng đang rê chuột trên viền, không phải đang
        // điều khiển máy từ xa.
        let out = capture.translate(&[Event::PointerMoved(Pos2::new(20.0, 350.0))], video());
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn dang_keo_thi_ra_ngoai_o_van_gui_va_bi_kep() {
        let mut capture = InputCapture::new();
        capture.translate(
            &[Event::PointerButton {
                pos: Pos2::new(500.0, 350.0),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }],
            video(),
        );
        assert!(capture.is_dragging());

        let out = capture.translate(&[Event::PointerMoved(Pos2::new(-40.0, 350.0))], video());
        assert_eq!(
            out,
            vec![InputEvent::MouseMove {
                monitor: 0,
                x: 0.0,
                y: 0.5
            }]
        );
    }

    #[test]
    fn nha_nut_chua_tung_bam_thi_bo_qua() {
        let mut capture = InputCapture::new();
        // Bấm xuống ở HUD (ngoài ô video) rồi nhả trong ô: nếu chuyển tiếp sự
        // kiện nhả, host nhận một cú nhả không có cú bấm đi trước.
        let out = capture.translate(
            &[
                Event::PointerButton {
                    pos: Pos2::new(10.0, 10.0),
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                Event::PointerButton {
                    pos: Pos2::new(500.0, 350.0),
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            video(),
        );
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn co_phim_roi_thi_bo_text_di_kem() {
        let mut capture = InputCapture::new();
        // Một lần bấm 'a' sinh ra cả hai sự kiện này.
        let out = capture.translate(
            &[key_event(Key::A, true), Event::Text("a".into())],
            video(),
        );
        assert_eq!(
            out,
            vec![InputEvent::Key {
                code: KeyCode::A,
                pressed: true
            }]
        );
    }

    #[test]
    fn text_khong_kem_phim_thi_van_gui() {
        let mut capture = InputCapture::new();
        // Bàn phím ảo hay dán bằng chuột: có chữ mà không có phím vật lý nào.
        let out = capture.translate(&[Event::Text("ước".into())], video());
        assert_eq!(
            out,
            vec![InputEvent::Text {
                text: "ước".into()
            }]
        );
    }

    #[test]
    fn ime_luon_gui_du_cung_khung_co_phim() {
        let mut capture = InputCapture::new();
        // IME nuốt phím gốc nên `Event::Key` trong cùng khung là của phím khác
        // (ở đây là Shift) — không được vì thế mà bỏ chuỗi IME.
        let out = capture.translate(
            &[
                key_event(Key::ShiftLeft, true),
                Event::Ime(egui::ImeEvent::Commit("ề".into())),
            ],
            video(),
        );
        assert_eq!(
            out,
            vec![
                InputEvent::Key {
                    code: KeyCode::ShiftLeft,
                    pressed: true
                },
                InputEvent::Text { text: "ề".into() },
            ]
        );
    }

    #[test]
    fn phim_lap_bi_chan() {
        let mut capture = InputCapture::new();
        let repeat = Event::Key {
            key: Key::A,
            physical_key: Some(Key::A),
            pressed: true,
            repeat: true,
            modifiers: egui::Modifiers::NONE,
        };
        assert!(capture.translate(&[repeat], video()).is_empty());
    }

    #[test]
    fn cmd_c_bi_egui_nuot_van_thanh_phim_c() {
        let mut capture = InputCapture::new();
        // Chuỗi thật khi bấm Cmd+C: phím Cmd đi qua bình thường, phím C bị đổi
        // thành `Event::Copy`, rồi sự kiện nhả C vẫn tới.
        let out = capture.translate(
            &[
                key_event(Key::SuperLeft, true),
                Event::Copy,
                key_event(Key::C, false),
            ],
            video(),
        );
        assert_eq!(
            out,
            vec![
                InputEvent::Key {
                    code: KeyCode::MetaLeft,
                    pressed: true
                },
                InputEvent::Key {
                    code: KeyCode::C,
                    pressed: true
                },
                InputEvent::Key {
                    code: KeyCode::C,
                    pressed: false
                },
            ]
        );
    }

    #[test]
    fn mat_focus_thi_yeu_cau_nha_sach() {
        let mut capture = InputCapture::new();
        capture.translate(
            &[Event::PointerButton {
                pos: Pos2::new(500.0, 350.0),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }],
            video(),
        );
        let out = capture.translate(&[Event::WindowFocused(false)], video());
        assert_eq!(out, vec![InputEvent::ReleaseAll]);
        assert!(!capture.is_dragging());
    }

    #[test]
    fn cuon_chi_tinh_khi_con_tro_trong_o_video() {
        let mut capture = InputCapture::new();
        let wheel = Event::MouseWheel {
            unit: MouseWheelUnit::Line,
            delta: egui::vec2(0.0, 2.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::NONE,
        };

        // Chưa biết con trỏ ở đâu thì chưa cuộn.
        assert!(
            capture
                .translate(std::slice::from_ref(&wheel), video())
                .is_empty()
        );

        capture.translate(&[Event::PointerMoved(Pos2::new(500.0, 350.0))], video());
        let out = capture.translate(&[wheel], video());
        assert_eq!(
            out,
            vec![InputEvent::Scroll {
                monitor: 0,
                delta_x: 0.0,
                delta_y: 2.0 * POINTS_PER_LINE
            }]
        );
    }

    #[test]
    fn phim_vat_ly_thang_the_phim_logic() {
        let mut capture = InputCapture::new();
        // Layout Dvorak: bấm vị trí 'S' của QWERTY ra chữ 'o'. Host bơm theo vị
        // trí nên phải gửi 'S'.
        let out = capture.translate(
            &[Event::Key {
                key: Key::O,
                physical_key: Some(Key::S),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            video(),
        );
        assert_eq!(
            out,
            vec![InputEvent::Key {
                code: KeyCode::S,
                pressed: true
            }]
        );
    }
}
