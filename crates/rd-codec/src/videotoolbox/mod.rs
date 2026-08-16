//! Mã hoá / giải mã bằng VideoToolbox — bộ tăng tốc phần cứng của Apple.
//!
//! Trên Apple silicon, bộ mã hoá là một khối mạch riêng (media engine) nằm
//! ngoài CPU và GPU. Nó đọc thẳng từ IOSurface mà ScreenCaptureKit trả về, nên
//! toàn bộ đường đi capture → encode không tốn một byte copy nào qua CPU.

mod decoder;
mod encoder;

pub use decoder::{DecodedFrame, VtDecoder};
pub use encoder::VtEncoder;

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_core_foundation::{CFBoolean, CFDictionary, CFRetained, CFString, CFType};
use objc2_core_media::{CMTime, CMTimeFlags, CMVideoCodecType};
use objc2_video_toolbox::{VTSessionCopyProperty, VTSessionSetProperty};

use crate::{Codec, CodecError, Result};

/// Đồng hồ dùng cho mọi timestamp gửi vào VideoToolbox: 1 tick = 1 micro giây.
/// Cùng đơn vị với `capture_us` nên timestamp đi xuyên suốt mà không cần đổi.
pub(crate) const TIMESCALE_US: i32 = 1_000_000;

/// Số byte của trường độ dài trong dạng length-prefix mà VideoToolbox dùng.
pub(crate) const NAL_LENGTH_SIZE: i32 = 4;

pub(crate) fn cm_time_us(us: i64) -> CMTime {
    CMTime {
        value: us,
        timescale: TIMESCALE_US,
        flags: CMTimeFlags::Valid,
        epoch: 0,
    }
}

pub(crate) fn codec_type(codec: Codec) -> Result<CMVideoCodecType> {
    match codec {
        Codec::Hevc => Ok(objc2_core_media::kCMVideoCodecType_HEVC),
        Codec::H264 => Ok(objc2_core_media::kCMVideoCodecType_H264),
        Codec::Av1 => Err(CodecError::Unsupported(
            "VideoToolbox không mã hoá được AV1".into(),
        )),
    }
}

/// Từ điển một cặp key/value — dạng duy nhất VideoToolbox cần ở đây.
pub(crate) fn cf_dict1(key: &CFString, value: &CFType) -> CFRetained<CFDictionary<CFType, CFType>> {
    let key: &CFType = key;
    CFDictionary::from_slices(&[key], &[value])
}

/// Từ điển nhiều cặp key/value.
pub(crate) fn cf_dict(
    pairs: &[(&CFString, &CFType)],
) -> CFRetained<CFDictionary<CFType, CFType>> {
    let keys: Vec<&CFType> = pairs.iter().map(|(key, _)| *key as &CFType).collect();
    let values: Vec<&CFType> = pairs.iter().map(|(_, value)| *value).collect();
    CFDictionary::from_slices(&keys, &values)
}

/// Bỏ tham số kiểu của `CFDictionary`.
///
/// Binding cho `CFDictionary` có hai tham số kiểu để dùng an toàn từ Rust,
/// nhưng hàm C nhận dạng không tham số. Cả hai đều là cùng một struct rỗng
/// `#[repr(C)]` nên ép con trỏ không đổi bố cục bộ nhớ.
pub(crate) fn erase_dict(dict: &CFDictionary<CFType, CFType>) -> &CFDictionary {
    unsafe { &*(dict as *const CFDictionary<CFType, CFType> as *const CFDictionary) }
}

/// Đặt một thuộc tính bắt buộc. Lỗi ở đây là lỗi thật, không bỏ qua được.
pub(crate) fn set_property(
    session: &CFType,
    key: &CFString,
    value: &CFType,
    name: &'static str,
) -> Result<()> {
    let status = unsafe { VTSessionSetProperty(session, key, Some(value)) };
    if status == 0 {
        Ok(())
    } else {
        Err(CodecError::Property { key: name, status })
    }
}

/// Đặt một thuộc tính tuỳ chọn: phiên bản macOS cũ hoặc bộ mã hoá khác có thể
/// không có nó, và thiếu nó ta vẫn chạy được (chỉ kém tối ưu hơn chút).
pub(crate) fn set_property_best_effort(
    session: &CFType,
    key: &CFString,
    value: &CFType,
    name: &'static str,
) {
    let status = unsafe { VTSessionSetProperty(session, key, Some(value)) };
    if status != 0 {
        tracing::debug!(property = name, status, "phiên không nhận thuộc tính này");
    }
}

/// Đọc lại một thuộc tính kiểu bool, để xác nhận điều ta *yêu cầu* có thực sự
/// được đáp ứng không (ví dụ: có đúng là đang chạy phần cứng không).
pub(crate) fn copy_bool_property(session: &CFType, key: &CFString) -> Option<bool> {
    let mut out: *const CFBoolean = std::ptr::null();
    let status = unsafe {
        VTSessionCopyProperty(
            session,
            key,
            None,
            (&mut out) as *mut *const CFBoolean as *mut c_void,
        )
    };
    if status != 0 || out.is_null() {
        return None;
    }
    // VTSessionCopyProperty trả về đối tượng đã +1 retain, ta phải nhận sở hữu.
    let value = unsafe { CFRetained::from_raw(NonNull::new(out.cast_mut())?) };
    Some(value.value())
}

/// Chạy thân callback do hệ thống gọi, chặn mọi panic tại biên FFI.
///
/// Panic xuyên qua khung stack của C/Objective-C là hành vi không xác định và
/// sẽ làm sập tiến trình ở chỗ khó lần ra. Thà ghi log rồi bỏ frame đó.
pub(crate) fn guard_callback(what: &'static str, body: impl FnOnce()) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
    if result.is_err() {
        tracing::error!(callback = what, "callback của VideoToolbox panic — bỏ frame");
    }
}

pub(crate) fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}
