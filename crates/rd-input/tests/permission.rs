//! Cổng quyền phải khớp với trạng thái thật của hệ điều hành.
//!
//! Đây là chỗ dễ hỏng âm thầm nhất của cả crate: trên macOS, thiếu quyền
//! Accessibility thì `CGEvent::post` **không báo lỗi** — nó chỉ lặng lẽ không
//! làm gì. Nếu `open()` cứ trả về `Ok` thì người dùng thấy phần mềm chạy bình
//! thường mà máy bên kia không nhúc nhích, và không có lấy một dòng log để lần
//! ra.
//!
//! Test chạy được ở cả hai phía: chưa cấp quyền thì đòi báo `PermissionDenied`,
//! cấp rồi thì đòi mở được.

#[cfg(target_os = "macos")]
mod macos {
    use rd_input::macos::{is_trusted, main_screen_geometry};
    use rd_input::{InputError, InputInjector, PlatformInjector};

    #[test]
    fn mo_kenh_bom_khop_voi_quyen_he_thong() {
        let geometry = main_screen_geometry().expect("đọc kích thước màn hình chính");
        let trusted = is_trusted();
        eprintln!(
            "quyền Accessibility: {}",
            if trusted { "đã cấp" } else { "chưa cấp" }
        );

        match (trusted, PlatformInjector::open(geometry)) {
            (true, Ok(_)) => {}
            (false, Err(InputError::PermissionDenied)) => {}
            (true, Err(err)) => panic!("đã có quyền mà vẫn không mở được: {err}"),
            (false, Ok(_)) => {
                panic!("chưa có quyền mà open() vẫn thành công — sự kiện sẽ bị nuốt im lặng")
            }
            (_, Err(err)) => panic!("lỗi không mong đợi: {err}"),
        }
    }

    #[test]
    fn kich_thuoc_man_hinh_hop_ly() {
        let geometry = main_screen_geometry().expect("đọc kích thước màn hình chính");
        // Không có màn hình thật nào nhỏ hơn thế; số 0 hay số âm nghĩa là đọc hỏng.
        assert!(geometry.width >= 640.0, "{geometry:?}");
        assert!(geometry.height >= 480.0, "{geometry:?}");
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use rd_input::windows::main_screen_geometry;
    use rd_input::{InputInjector, PlatformInjector};

    /// Windows không có cổng quyền để kiểm trước: `SendInput` chỉ bị UIPI chặn
    /// khi cửa sổ đang focus chạy ở mức toàn vẹn cao hơn. Nên `open()` phải
    /// luôn thành công — báo lỗi ở đây là ta tự dựng rào không có thật.
    #[test]
    fn mo_kenh_bom_khong_can_xin_quyen() {
        let geometry = main_screen_geometry().expect("đọc kích thước màn hình chính");
        PlatformInjector::open(geometry).expect("Windows không chặn SendInput lúc mở");
    }

    #[test]
    fn kich_thuoc_man_hinh_hop_ly() {
        let geometry = main_screen_geometry().expect("đọc kích thước màn hình chính");
        assert!(geometry.width >= 640.0, "{geometry:?}");
        assert!(geometry.height >= 480.0, "{geometry:?}");
    }
}
