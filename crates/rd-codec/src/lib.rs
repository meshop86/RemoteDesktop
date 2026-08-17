//! Mã hoá và giải mã video bằng phần cứng.
//!
//! Cấu hình ở đây khác hẳn cấu hình dùng để lưu file phim, vì mục tiêu khác
//! nhau hoàn toàn:
//!
//! - **Không B-frame.** B-frame tham chiếu cả frame trước lẫn frame sau nên bộ
//!   mã hoá phải giữ lại frame đang có, chờ frame kế tiếp rồi mới phát ra được.
//!   Đổi lại vài phần trăm dung lượng, ta phải trả thêm nguyên một khoảng thời
//!   gian frame vào độ trễ — không đáng.
//! - **Realtime mode.** Bảo bộ mã hoá ưu tiên phát ra đúng hạn hơn là nén tối
//!   ưu; nó sẽ không gom nhiều frame lại xử lý theo lô.
//! - **Keyframe theo yêu cầu.** Keyframe rất nặng (gấp 10-30 lần frame thường)
//!   nên không phát định kỳ dày; chỉ phát khi viewer vừa kết nối hoặc báo mất
//!   dữ liệu.

pub mod annexb;

// Không cfg-gate: toàn phép gói bit thuần, để test của nó chạy được ngay trên
// máy dev macOS thay vì chỉ khi build Windows.
pub mod mf_params;

#[cfg(target_os = "macos")]
pub mod videotoolbox;

#[cfg(target_os = "windows")]
pub mod mediafoundation;

#[cfg(target_os = "macos")]
pub use videotoolbox::{VtDecoder as PlatformDecoder, VtEncoder as PlatformEncoder};

// Hai bí danh này chỉ để gọi tên cho gọn, **không** phải giao diện chung: hàm
// khởi tạo của hai nền tảng nhận tham số khác nhau (bản Windows cần device
// D3D11) và kiểu frame trả ra cũng khác. Chỗ nào dựng hoặc đọc frame vẫn phải
// tách nhánh theo hệ điều hành.
#[cfg(target_os = "windows")]
pub use mediafoundation::{MfDecoder as PlatformDecoder, MfEncoder as PlatformEncoder};

pub use rd_protocol::control::{ChromaSubsampling, Codec};

/// Codec mà máy này **giải mã** được, xếp theo thứ tự ưu tiên.
///
/// Viewer khai danh sách này trong lời chào, host chỉ được mã hoá bằng codec
/// nằm trong đó. Đoán bừa là hỏng theo kiểu khó hiểu nhất: phiên nối xong, báo
/// thành công, rồi tắt ngay vì không dựng nổi bộ giải mã — không hình, không
/// điều khiển được, mà nhìn bề ngoài thì mọi thứ đều ổn.
#[cfg(target_os = "windows")]
pub fn decodable() -> Vec<Codec> {
    mediafoundation::decodable()
}

/// Codec mà máy này **mã hoá** được.
#[cfg(target_os = "windows")]
pub fn encodable() -> Vec<Codec> {
    mediafoundation::encodable()
}

/// Mọi máy Mac chạy được bản này đều giải mã được cả hai codec: HEVC có mặt từ
/// macOS 10.13, và VideoToolbox tự lùi về giải mã bằng phần mềm ở những máy
/// không có mạch phần cứng. Không có API nào hỏi được "dựng nổi phiên không" mà
/// không dựng thử, nên khai thẳng còn hơn dựng thử rồi vứt.
#[cfg(target_os = "macos")]
pub fn decodable() -> Vec<Codec> {
    vec![Codec::Hevc, Codec::H264]
}

#[cfg(target_os = "macos")]
pub fn encodable() -> Vec<Codec> {
    vec![Codec::Hevc, Codec::H264]
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("phần cứng không hỗ trợ cấu hình này ({0})")]
    Unsupported(String),
    #[error("không tạo được phiên {what}: OSStatus {status}")]
    SessionCreate { what: &'static str, status: i32 },
    #[error("không đặt được thuộc tính {key}: OSStatus {status}")]
    Property { key: &'static str, status: i32 },
    #[error("lỗi mã hoá: OSStatus {0}")]
    Encode(i32),
    #[error("lỗi giải mã: OSStatus {0}")]
    Decode(i32),
    #[error("bitstream hỏng: {0}")]
    Bitstream(#[from] annexb::BitstreamError),
    #[error("thiếu parameter set — chưa nhận được keyframe nào")]
    MissingParameterSets,
    #[error("phiên đã đóng")]
    Closed,
    #[error("hết thời gian chờ")]
    Timeout,
    #[error("lỗi hệ thống khi {what}: {detail}")]
    System { what: &'static str, detail: String },
}

pub type Result<T> = std::result::Result<T, CodecError>;

#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub codec: Codec,
    /// Mức lấy mẫu màu *mong muốn*. Phần cứng có thể không đáp ứng được; xem
    /// [`VideoEncoder::actual_chroma`] để biết mức thực tế nhận được.
    pub chroma: ChromaSubsampling,
    pub target_bitrate_kbps: u32,
    pub target_fps: u32,
    /// Khoảng cách tối đa giữa hai keyframe, tính bằng giây. Đặt lớn vì ta chủ
    /// yếu phát keyframe theo yêu cầu của viewer.
    pub keyframe_interval_secs: u32,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            codec: Codec::Hevc,
            chroma: ChromaSubsampling::Yuv422,
            target_bitrate_kbps: 30_000,
            target_fps: 60,
            keyframe_interval_secs: 10,
        }
    }
}

/// Một frame đã nén, đóng gói Annex B, sẵn sàng gửi qua mạng.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// Chính là `capture_us` của frame gốc — đi xuyên suốt để đo độ trễ đầu-cuối.
    pub pts_us: u64,
    /// Thời gian từ lúc đưa frame vào bộ mã hoá đến lúc nhận được kết quả.
    pub encode_us: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EncoderStats {
    pub frames_submitted: u64,
    pub frames_emitted: u64,
    /// Frame bị bộ mã hoá bỏ vì không theo kịp.
    pub frames_dropped: u64,
    pub bytes_emitted: u64,
    pub errors: u64,
}

/// Cách sắp xếp pixel của frame đã giải mã.
///
/// Bộ giải mã phần cứng *tự chọn* định dạng nếu không bị ép, và nó có thể chọn
/// định dạng không nằm trong SDK công khai (thực tế đã gặp `'p422'`). Ta ghim
/// hẳn về hai biến thể dưới đây để phía render chỉ cần biết đúng hai đường —
/// và cả hai đều ánh xạ thẳng sang texture của wgpu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameFormat {
    /// NV12 8-bit, video range. Plane Y một kênh, plane CbCr hai kênh xen kẽ
    /// với kích thước bằng một nửa theo cả hai chiều.
    Nv12VideoRange,
    /// 4:2:2 10-bit, video range. Mỗi mẫu chiếm 16 bit với 10 bit có nghĩa nằm
    /// ở phần cao. Plane CbCr rộng bằng nửa nhưng **cao bằng** plane Y — đó
    /// chính là phần làm chữ màu nét hơn hẳn NV12.
    P210VideoRange,
}

impl FrameFormat {
    /// Số bit thật sự mang thông tin của mỗi mẫu.
    pub fn bit_depth(self) -> u32 {
        match self {
            Self::Nv12VideoRange => 8,
            Self::P210VideoRange => 10,
        }
    }

    /// Hệ số thu nhỏ của plane chroma so với plane luma, theo `(ngang, dọc)`.
    pub fn chroma_shift(self) -> (u32, u32) {
        match self {
            Self::Nv12VideoRange => (1, 1),
            Self::P210VideoRange => (1, 0),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DecoderStats {
    pub frames_submitted: u64,
    pub frames_emitted: u64,
    pub frames_dropped: u64,
    pub errors: u64,
}
