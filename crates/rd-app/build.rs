//! Nhúng thông tin phiên bản và biểu tượng vào file .exe trên Windows.
//!
//! Một file exe không tên, không nhà phát hành, không biểu tượng — lại còn chụp
//! màn hình, giả lập chuột phím và mở kết nối ra Internet — chính là chân dung
//! mà Defender/SmartScreen dựng sẵn cho phần mềm gián điệp. Điền đủ mấy trường
//! này không biến chương trình thành "đã ký", nhưng bỏ đi được một trong những
//! dấu hiệu khiến máy người dùng kêu virus dù chương trình sạch.
//!
//! Ngoài Windows thì tệp này không làm gì.

fn main() {
    println!("cargo:rerun-if-changed=../../packaging/icon.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // FileVersion và ProductVersion winresource tự lấy từ `version` của crate.
    let mut res = winresource::WindowsResource::new();
    res.set_icon("../../packaging/icon.ico")
        .set("FileDescription", "Điều khiển máy tính từ xa")
        .set("ProductName", "Remote Desktop")
        .set("CompanyName", "Luong Xuan Hoa")
        .set("LegalCopyright", "Copyright (c) 2026 Luong Xuan Hoa")
        .set("OriginalFilename", "remote-desktop.exe");

    if let Err(err) = res.compile() {
        // Máy thiếu công cụ nhúng resource thì vẫn build ra chương trình chạy
        // được, chỉ là trơ như trước. Chặn build ở đây chỉ làm khổ người vừa
        // clone về; bản phát hành dựng trên CI có đủ SDK nên vẫn đúng.
        println!("cargo:warning=không nhúng được resource vào exe: {err}");
    }
}
