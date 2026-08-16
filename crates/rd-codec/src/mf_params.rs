//! Những con số Media Foundation đòi, tách khỏi phần gọi API.
//!
//! Đây toàn là phép gói bit và đổi đơn vị. Gói sai thì không có lỗi nào báo về:
//! Media Foundation nhận đại con số đó và trả ra hình méo màu hoặc timestamp
//! lệch. Tách ra đây để chúng chạy được dưới `cargo test` ngay trên máy dev,
//! thay vì chỉ khi có một máy Windows trong tay.

/// Nhiều thuộc tính của Media Foundation là một cặp `u32` nhét chung vào một
/// `u64`: kích thước khung hình, tỉ lệ khung hình, tỉ lệ điểm ảnh.
pub const fn attribute_pair(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | low as u64
}

/// Đổi micro giây sang đơn vị 100 nano giây — đơn vị thời gian của toàn bộ
/// Media Foundation.
pub fn hundred_ns(micros: u64) -> i64 {
    // `u64` micro giây lớn hơn `i64` 100ns đúng 10 lần nên phải chặn trên,
    // nhưng ngưỡng đó là hơn 29 nghìn năm — chặn để không tràn âm, không phải
    // vì nó xảy ra được.
    micros.saturating_mul(10).min(i64::MAX as u64) as i64
}

/// Bên nào của phép chuyển màu đang được mô tả.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorUsage {
    /// Ảnh để xem — bộ xử lý video được phép làm đẹp.
    Playback,
    /// Ảnh để xử lý tiếp. Bắt buộc dùng cho đường đi vào bộ mã hoá: mọi phép
    /// "làm đẹp" đều là biến dạng dữ liệu mà bên kia không hoàn lại được.
    Processing,
}

/// Dải giá trị của mẫu màu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NominalRange {
    Unknown,
    /// 16-235 — dải hẹp, quy ước của video.
    Limited,
    /// 0-255 — dải đầy, quy ước của màn hình máy tính.
    Full,
}

impl NominalRange {
    fn code(self) -> u32 {
        match self {
            Self::Unknown => 0,
            Self::Limited => 1,
            Self::Full => 2,
        }
    }
}

/// Gói `D3D11_VIDEO_PROCESSOR_COLOR_SPACE` — một struct toàn bit field, nên
/// windows-rs để lộ ra đúng một `u32`.
///
/// Bố cục theo `d3d11.h`: bit 0 `Usage`, bit 1 `RGB_Range`, bit 2
/// `YCbCr_Matrix`, bit 3 `YCbCr_xvYCC`, bit 4-5 `Nominal_Range`.
///
/// `rgb_full` chỉ có nghĩa ở phía RGB, `bt709` chỉ có nghĩa ở phía YCbCr — bên
/// còn lại bỏ qua bit đó, nên truyền gì cũng được.
pub fn color_space(usage: ColorUsage, rgb_full: bool, bt709: bool, range: NominalRange) -> u32 {
    let usage_bit = match usage {
        ColorUsage::Playback => 0,
        ColorUsage::Processing => 1,
    };
    // `RGB_Range`: 0 = dải đầy 0-255, 1 = dải hẹp 16-235. Ngược dấu với tên
    // biến nên dễ nhầm, viết rõ ra.
    let rgb_bit = if rgb_full { 0 } else { 1 };
    // `YCbCr_Matrix`: 0 = BT.601, 1 = BT.709.
    let matrix_bit = if bt709 { 1 } else { 0 };

    usage_bit | (rgb_bit << 1) | (matrix_bit << 2) | (range.code() << 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_u32_nam_dung_nua_tren_nua_duoi() {
        assert_eq!(attribute_pair(1920, 1080), (1920u64 << 32) | 1080);
        // Cạnh dễ sai nhất: số ở nửa dưới không được tràn lên nửa trên.
        assert_eq!(attribute_pair(0, u32::MAX), u32::MAX as u64);
        assert_eq!(attribute_pair(u32::MAX, 0), (u32::MAX as u64) << 32);
    }

    #[test]
    fn micro_giay_thanh_don_vi_100ns() {
        assert_eq!(hundred_ns(0), 0);
        assert_eq!(hundred_ns(1), 10);
        assert_eq!(hundred_ns(1_000_000), 10_000_000);
    }

    /// Timestamp âm làm bộ mã hoá vứt frame. Tràn phải kẹp lại, không quấn vòng.
    #[test]
    fn timestamp_khong_bao_gio_am() {
        assert!(hundred_ns(u64::MAX) > 0);
        assert_eq!(hundred_ns(u64::MAX), i64::MAX);
    }

    #[test]
    fn khong_gian_mau_dat_dung_tung_bit() {
        // Phía vào: BGRA từ màn hình, dải đầy, đường xử lý.
        let input = color_space(ColorUsage::Processing, true, true, NominalRange::Full);
        assert_eq!(input & 1, 1, "Usage phải là 1 (xử lý)");
        assert_eq!((input >> 1) & 1, 0, "RGB_Range 0 nghĩa là dải đầy");
        assert_eq!((input >> 4) & 3, 2, "Nominal_Range 2 nghĩa là 0-255");

        // Phía ra: NV12 dải hẹp BT.709 — đúng thứ bộ mã hoá chờ.
        let output = color_space(ColorUsage::Processing, false, true, NominalRange::Limited);
        assert_eq!((output >> 2) & 1, 1, "YCbCr_Matrix 1 nghĩa là BT.709");
        assert_eq!((output >> 4) & 3, 1, "Nominal_Range 1 nghĩa là 16-235");
        assert_eq!((output >> 3) & 1, 0, "xvYCC luôn tắt");
    }

    /// BT.601 và BT.709 lệch nhau đủ để da người ngả xanh. Hai giá trị này
    /// không được trùng nhau.
    #[test]
    fn hai_ma_tran_mau_khac_nhau() {
        let bt601 = color_space(ColorUsage::Processing, false, false, NominalRange::Limited);
        let bt709 = color_space(ColorUsage::Processing, false, true, NominalRange::Limited);
        assert_ne!(bt601, bt709);
    }
}
