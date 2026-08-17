//! Nhúng thông tin phiên bản vào rd-rendezvous.exe. Xem `crates/rd-app/build.rs`
//! để biết vì sao — bản cài Windows chở theo cả hai file exe, thiếu thông tin
//! file nào thì file đó bị soi.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let mut res = winresource::WindowsResource::new();
    res.set("FileDescription", "Máy chủ hẹn gặp cho Remote Desktop")
        .set("ProductName", "Remote Desktop")
        .set("CompanyName", "Luong Xuan Hoa")
        .set("LegalCopyright", "Copyright (c) 2026 Luong Xuan Hoa")
        .set("OriginalFilename", "rd-rendezvous.exe");

    if let Err(err) = res.compile() {
        println!("cargo:warning=không nhúng được resource vào exe: {err}");
    }
}
