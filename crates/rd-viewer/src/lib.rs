//! Phần lõi của viewer, tách khỏi `main.rs` để test dựng được từng mảnh.
//!
//! Cụ thể là để kiểm chứng đường vẽ: nạp frame lên GPU rồi đọc ngược pixel ra
//! so với màu gốc. Nếu ma trận chuyển màu hay hệ số dải hẹp sai, ảnh vẫn hiện
//! ra bình thường với người không để ý — chỉ có số mới bắt được.

pub mod control;
pub mod input_capture;
pub mod metrics;
pub mod pipeline;
pub mod video;
