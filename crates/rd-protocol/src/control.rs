//! Message trên kênh tin cậy: input, chất lượng, chat, file, thống kê.

use serde::{Deserialize, Serialize};

use crate::{ProtocolError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Codec {
    H264 = 0,
    Hevc = 1,
    Av1 = 2,
}

impl Codec {
    pub fn from_u8(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Codec::H264),
            1 => Ok(Codec::Hevc),
            2 => Ok(Codec::Av1),
            other => Err(ProtocolError::UnknownCodec(other)),
        }
    }
}

/// Kiểu lấy mẫu màu. 4:4:4 giữ nguyên màu từng pixel nên chữ và viền cửa sổ
/// sắc nét; 4:2:0 nén màu xuống 1/4 nên nhẹ băng thông hơn nhưng chữ bị nhoè.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChromaSubsampling {
    Yuv420,
    /// Giữ đủ màu theo chiều dọc — chữ nét hơn hẳn 4:2:0. Đây là mức cao nhất
    /// mà bộ mã hoá phần cứng của Apple silicon hỗ trợ.
    Yuv422,
    Yuv444,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub id: u8,
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// Hệ số scale của hệ điều hành (Retina = 2.0). Cần để map toạ độ chuột.
    pub scale: f32,
    pub refresh_hz: u32,
    pub is_primary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct QualityRequest {
    pub codec: Codec,
    pub chroma: ChromaSubsampling,
    pub target_bitrate_kbps: u32,
    pub target_fps: u32,
    /// Giới hạn cạnh dài nhất; None nghĩa là giữ nguyên phân giải gốc.
    pub max_dimension: Option<u32>,
}

impl Default for QualityRequest {
    fn default() -> Self {
        // Mặc định ưu tiên nét: HEVC 4:4:4, giữ nguyên phân giải gốc.
        Self {
            codec: Codec::Hevc,
            chroma: ChromaSubsampling::Yuv444,
            target_bitrate_kbps: 30_000,
            target_fps: 60,
            max_dimension: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

/// Mã phím theo vị trí vật lý trên bàn phím (không phụ thuộc layout người dùng
/// đang dùng), để host bấm đúng phím dù hai máy khác layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyCode {
    A, B, C, D, E, F, G, H, I, J, K, L, M,
    N, O, P, Q, R, S, T, U, V, W, X, Y, Z,
    Digit0, Digit1, Digit2, Digit3, Digit4,
    Digit5, Digit6, Digit7, Digit8, Digit9,
    F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12,
    Escape, Tab, CapsLock, Space, Backspace, Enter,
    ShiftLeft, ShiftRight, ControlLeft, ControlRight,
    AltLeft, AltRight, MetaLeft, MetaRight,
    ArrowUp, ArrowDown, ArrowLeft, ArrowRight,
    Insert, Delete, Home, End, PageUp, PageDown,
    Minus, Equal, BracketLeft, BracketRight, Backslash,
    Semicolon, Quote, Backquote, Comma, Period, Slash,
    Numpad0, Numpad1, Numpad2, Numpad3, Numpad4,
    Numpad5, Numpad6, Numpad7, Numpad8, Numpad9,
    NumpadAdd, NumpadSubtract, NumpadMultiply, NumpadDivide,
    NumpadDecimal, NumpadEnter, NumLock,
    PrintScreen, ScrollLock, Pause, ContextMenu,
}

/// Toạ độ chuột luôn chuẩn hoá về [0.0, 1.0] theo màn hình đang xem, nên viewer
/// không cần biết phân giải thật của host và không bị lệch khi đổi phân giải.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
    MouseMove {
        monitor: u8,
        x: f32,
        y: f32,
    },
    MouseButton {
        monitor: u8,
        button: MouseButton,
        pressed: bool,
        x: f32,
        y: f32,
    },
    Scroll {
        monitor: u8,
        delta_x: f32,
        delta_y: f32,
    },
    Key {
        code: KeyCode,
        pressed: bool,
    },
    /// Ký tự đã qua IME (gõ tiếng Việt, emoji...). Host gõ thẳng chuỗi này.
    Text {
        text: String,
    },
    /// Nhả toàn bộ phím và nút đang giữ. Gửi khi viewer mất focus để tránh
    /// tình trạng phím kẹt ở host.
    ReleaseAll,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub from: String,
    pub body: String,
    /// Unix time mili giây, để hiển thị và sắp xếp.
    pub sent_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileOffer {
    pub transfer_id: u64,
    pub name: String,
    pub size: u64,
    /// BLAKE3 của toàn file, dùng kiểm tra tính toàn vẹn sau khi nhận xong.
    pub hash: [u8; 32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChunkAck {
    pub transfer_id: u64,
    pub received_bytes: u64,
}

/// Viewer → host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ViewerCommand {
    Hello {
        version: u16,
        viewer_name: String,
        /// BLAKE3 của mật khẩu phiên mà host đang hiện trên màn hình.
        ///
        /// Không gửi mật khẩu trần: log của cả hai máy và của relay đều có thể
        /// giữ lại thông điệp này. Gửi hash cũng không biến nó thành bí mật
        /// tuyệt đối — ai bắt được hash thì phát lại được — nhưng kênh đã có
        /// TLS, còn cái này chặn đúng thứ cần chặn: người quét cổng gặp máy ta
        /// và vào thẳng.
        auth: [u8; 32],
        /// Card đồ hoạ của viewer hiển thị được 10-bit hay không.
        ///
        /// Phải hỏi *trước* khi host dựng bộ mã hoá: chọn 4:2:2 10-bit cho một
        /// máy không dựng được texture 16-bit thì hình ra sai màu, mà lúc đó
        /// đổi lại là phải dựng lại cả chuỗi mã hoá giữa phiên.
        wants_10bit: bool,
        /// Codec mà máy xem **giải mã** được, xếp theo thứ tự ưu tiên của nó.
        ///
        /// Host chỉ được mã hoá bằng codec nằm trong danh sách này. Không hỏi mà
        /// cứ chọn HEVC là gặp đúng cái bẫy của Windows: bộ giải mã HEVC không
        /// có sẵn trong Windows (nằm ở gói "HEVC Video Extensions" của Store),
        /// nên phiên nối xong, báo thành công, rồi tắt ngay vì viewer không dựng
        /// nổi bộ giải mã.
        codecs: Vec<Codec>,
    },
    SelectMonitor {
        monitor: u8,
    },
    RequestKeyframe {
        monitor: u8,
    },
    SetQuality(QualityRequest),
    Input(InputEvent),
    Chat(ChatMessage),
    FileOffer(FileOffer),
    FileAccept {
        transfer_id: u64,
    },
    FileReject {
        transfer_id: u64,
    },
    FileChunkAck(FileChunkAck),
    /// Viewer báo tình hình mạng để host chỉnh bitrate.
    Feedback {
        frames_received: u32,
        frames_dropped: u32,
        packets_lost: u32,
        measured_kbps: u32,
    },
    Ping {
        sent_us: u64,
    },
    Disconnect,
}

/// Host → viewer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HostEvent {
    Welcome {
        version: u16,
        host_name: String,
        monitors: Vec<MonitorInfo>,
        active_monitor: u8,
    },
    MonitorsChanged {
        monitors: Vec<MonitorInfo>,
    },
    QualityChanged(QualityRequest),
    Chat(ChatMessage),
    FileOffer(FileOffer),
    FileAccept {
        transfer_id: u64,
    },
    FileReject {
        transfer_id: u64,
    },
    /// Đối xứng với [`ViewerCommand::FileChunkAck`]: file đi được cả hai chiều
    /// nên bên nào cũng phải báo được "đã ghi tới đâu" cho bên gửi vẽ tiến độ.
    FileChunkAck(FileChunkAck),
    Pong {
        sent_us: u64,
        host_us: u64,
    },
    Error {
        message: String,
    },
}

/// Bọc chung hai chiều, tiện cho code log và test.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlMessage {
    FromViewer(ViewerCommand),
    FromHost(HostEvent),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode_control, encode_control};

    #[test]
    fn viewer_command_roundtrip() {
        let msg = ViewerCommand::Input(InputEvent::MouseButton {
            monitor: 1,
            button: MouseButton::Right,
            pressed: true,
            x: 0.25,
            y: 0.75,
        });
        let bytes = encode_control(&msg);
        let decoded: ViewerCommand = decode_control(&bytes).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn host_event_roundtrip() {
        let msg = HostEvent::Welcome {
            version: crate::PROTOCOL_VERSION,
            host_name: "MacBook cua Hoa".into(),
            monitors: vec![MonitorInfo {
                id: 0,
                name: "VA2419".into(),
                width: 1920,
                height: 1080,
                scale: 1.0,
                refresh_hz: 60,
                is_primary: true,
            }],
            active_monitor: 0,
        };
        let bytes = encode_control(&msg);
        assert_eq!(decode_control::<HostEvent>(&bytes).unwrap(), msg);
    }

    #[test]
    fn input_event_stays_small() {
        // Input là thứ gửi nhiều nhất trên kênh tin cậy; giữ nó nhỏ để không
        // cạnh tranh băng thông với video.
        let bytes = encode_control(&ViewerCommand::Input(InputEvent::MouseMove {
            monitor: 0,
            x: 0.5,
            y: 0.5,
        }));
        assert!(bytes.len() <= 16, "input event {} byte, quá lớn", bytes.len());
    }
}
