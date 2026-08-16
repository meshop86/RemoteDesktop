//! Trạng thái phiên ở tầng ứng dụng: khung chat và danh sách file đang truyền.
//!
//! Cố ý không có `async`, không có socket, không có egui. Phần khó của chat và
//! truyền file không nằm ở chỗ gửi byte — chỗ đó [`rd_transport`] lo rồi — mà ở
//! chỗ *ai đang chờ ai*: một lời mời gửi đi rồi có thể bị từ chối, chấp nhận,
//! đứt giữa chừng, hoặc trùng số hiệu với lời mời của đầu kia. Gom hết vào đây
//! thì luật chơi kiểm tra được bằng unit test chạy trong vài micro giây, thay vì
//! phải dựng hai máy lên mới biết sai.
//!
//! [`rd_transport`]: https://docs.rs/rd-transport

pub mod chat;
pub mod transfer;

pub use chat::{ChatLog, MAX_CHAT_BODY};
pub use transfer::{Direction, IdSpace, Transfer, TransferState, Transfers};
