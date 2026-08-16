//! Tầng vận chuyển: một kết nối QUIC duy nhất phục vụ cả bốn tính năng.
//!
//! ```text
//! ┌──────────── QUIC connection ────────────┐
//! │ datagram (không tin cậy) → video        │
//! │ bi stream #0 (tin cậy)   → input, chat, │
//! │                            điều khiển    │
//! │ uni stream mỗi lần       → truyền file   │
//! └─────────────────────────────────────────┘
//! ```
//!
//! Dùng một kết nối cho tất cả nghĩa là chỉ phải xuyên NAT một lần, và file
//! truyền song song không chặn video (QUIC không có head-of-line blocking giữa
//! các stream, khác hẳn TCP).

/// Xuất lại quinn để các crate khác dùng đúng phiên bản này — hai phiên bản
/// quinn khác nhau trong cùng chương trình sẽ ra hai kiểu `Endpoint` không
/// dùng chung được, mà lỗi thì rất khó đọc.
pub use quinn;

pub mod clock;
pub mod endpoint;
pub mod file;
pub mod session;
pub mod tls;

pub use clock::{ClockSync, now_us};
pub use endpoint::{
    ALPN, client_config, client_config_for, client_endpoint, server_config, server_config_for,
    server_endpoint, transport_config,
};
pub use file::{CHUNK, prepare_offer, read_transfer_id, recv_file, safe_file_name, send_file};
pub use session::{
    ControlReceiver, ControlSender, LinkStats, Session, VideoReceiver, VideoSender,
};
pub use tls::{SelfSignedIdentity, fingerprint_short, generate_self_signed};

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("lỗi TLS: {0}")]
    Tls(String),
    #[error("lỗi I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("kết nối đứt: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("không kết nối được: {0}")]
    Connect(#[from] quinn::ConnectError),
    #[error("lỗi ghi stream: {0}")]
    Write(#[from] quinn::WriteError),
    #[error("lỗi đọc stream: {0}")]
    Read(#[from] quinn::ReadExactError),
    #[error("lỗi đọc dữ liệu: {0}")]
    ReadStream(#[from] quinn::ReadError),
    #[error("stream đã đóng: {0}")]
    ClosedStream(#[from] quinn::ClosedStream),
    #[error("đầu kia dừng nhận: {0}")]
    Stopped(#[from] quinn::StoppedError),
    #[error("tên file không dùng được: {0:?}")]
    BadFileName(String),
    #[error("file {name}: nhận {got} byte nhưng lời mời ghi {want} byte")]
    FileSizeMismatch { name: String, got: u64, want: u64 },
    #[error("file {0}: hash không khớp, nội dung đã hỏng")]
    FileHashMismatch(String),
    #[error("đầu kia huỷ nhận file (mã {0})")]
    FileRejected(u64),
    #[error("lỗi giải mã message: {0}")]
    Decode(#[from] postcard::Error),
    #[error("message quá lớn: {0} byte")]
    MessageTooLarge(usize),
    #[error("hết thời gian chờ")]
    Timeout,
}

pub type Result<T> = std::result::Result<T, TransportError>;
