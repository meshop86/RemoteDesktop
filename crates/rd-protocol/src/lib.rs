//! Wire format dùng chung giữa host, viewer và rendezvous server.
//!
//! Tách làm hai đường rõ rệt vì hai đường này có yêu cầu hoàn toàn khác nhau:
//!
//! * [`video`] — đường nóng, chạy trên QUIC datagram (không tin cậy, không
//!   retransmit). Header cố định 20 byte, parse bằng phép dịch bit, không đi
//!   qua serde để tránh mọi chi phí thừa trên mỗi gói.
//! * [`control`] — input, chat, file, thương lượng chất lượng. Chạy trên QUIC
//!   stream tin cậy, dùng serde + postcard cho tiện mở rộng về sau.

pub mod assembler;
pub mod control;
pub mod video;

pub use assembler::{AssembledFrame, FrameAssembler};
pub use control::{
    ChatMessage, Codec, ControlMessage, FileChunkAck, FileOffer, HostEvent, InputEvent, KeyCode,
    MonitorInfo, MouseButton, QualityRequest, ViewerCommand,
};
pub use video::{VIDEO_HEADER_LEN, VideoFlags, VideoHeader, fragment_frame};

/// Kích thước payload tối đa nhét vừa một QUIC datagram mà không bị IP
/// fragment: 1200 byte là ngưỡng an toàn cho hầu hết đường mạng Internet
/// (kể cả khi có tunnel/PPPoE), trừ đi header video 20 byte.
pub const MAX_DATAGRAM_SIZE: usize = 1200;
pub const MAX_VIDEO_PAYLOAD: usize = MAX_DATAGRAM_SIZE - VIDEO_HEADER_LEN;

/// Phiên bản giao thức. Hai đầu khác phiên bản thì từ chối bắt tay ngay.
pub const PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("gói tin quá ngắn: {0} byte")]
    TooShort(usize),
    #[error("loại gói không hợp lệ: {0:#x}")]
    UnknownPacketKind(u8),
    #[error("codec không hợp lệ: {0}")]
    UnknownCodec(u8),
    #[error("lỗi giải mã control message: {0}")]
    Decode(#[from] postcard::Error),
    #[error("phiên bản giao thức không khớp: ta {ours}, đối phương {theirs}")]
    VersionMismatch { ours: u16, theirs: u16 },
}

pub type Result<T> = std::result::Result<T, ProtocolError>;

/// Mã hoá một control message thành bytes để gửi qua stream tin cậy.
pub fn encode_control<T: serde::Serialize>(msg: &T) -> Vec<u8> {
    // postcard chỉ lỗi khi serializer hết chỗ; ở đây dùng Vec nên không thể lỗi.
    postcard::to_allocvec(msg).expect("serialize control message vào Vec không thể lỗi")
}

/// Giải mã control message nhận từ stream tin cậy.
pub fn decode_control<'a, T: serde::Deserialize<'a>>(bytes: &'a [u8]) -> Result<T> {
    Ok(postcard::from_bytes(bytes)?)
}
