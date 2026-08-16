//! Rendezvous: hai máy tìm thấy nhau qua Internet.
//!
//! Vấn đề cần giải: cả host lẫn viewer đều nằm sau NAT, không bên nào có địa
//! chỉ mà bên kia gọi thẳng tới được. Cách làm giống TeamViewer hay Chrome
//! Remote Desktop:
//!
//! 1. Host mở một kết nối tới **rendezvous server** và giữ đó. Server nhìn địa
//!    chỉ nguồn của gói tin là biết được địa chỉ công cộng mà NAT đã cấp cho
//!    host — đây chính là chức năng của STUN, ở đây có sẵn không cần thêm gì.
//! 2. Server cấp cho host một **mã 9 chữ số**. Người dùng đọc mã đó cho bên kia.
//! 3. Viewer hỏi server mã đó, server trả về địa chỉ của host **và** báo ngược
//!    cho host địa chỉ của viewer.
//! 4. Hai bên cùng lúc bắn gói về phía nhau. Gói đi ra mở lỗ trên NAT của chính
//!    mình, nên gói của bên kia vào được — gọi là **hole punching**.
//! 5. Thủng được thì video đi thẳng máy sang máy. Không thủng được (NAT đối
//!    xứng) thì chuyển qua **relay**, chậm hơn nhưng vẫn chạy.
//!
//! Điểm quan trọng: server này **chỉ giới thiệu địa chỉ**. Nó không giữ mật
//! khẩu, không đọc được nội dung. Việc xác thực "viewer này có được phép vào
//! không" diễn ra thẳng giữa hai máy sau khi đã nối được, trên kênh đã mã hoá.

pub mod client;
pub mod proto;
pub mod punch;
pub mod registry;
pub mod relay;
pub mod server;

pub use client::{Call, SignalClient};
pub use proto::{Candidates, FromServer, PeerId, RelayToken, SIGNAL_ALPN, ToServer};
pub use registry::Registry;
pub use relay::{RelayLink, RelayServer};

#[derive(Debug, thiserror::Error)]
pub enum SignalError {
    #[error("lỗi vận chuyển: {0}")]
    Transport(#[from] rd_transport::TransportError),
    #[error("server báo lỗi: {0}")]
    Server(String),
    #[error("không có máy nào mang mã {0}")]
    UnknownPeer(PeerId),
    #[error("server trả về thông điệp không đúng lúc: {0}")]
    Unexpected(&'static str),
    #[error("hết thời gian chờ")]
    Timeout,
    #[error("lỗi I/O: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, SignalError>;
