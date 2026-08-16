use std::ffi::{c_int, c_void};
use std::ptr::{NonNull, null, null_mut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use objc2_core_foundation::{CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFType};
use objc2_core_media::{CMFormatDescription, CMSampleBuffer};
use objc2_core_video::CVPixelBuffer;
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, kVTCompressionPropertyKey_AllowFrameReordering,
    kVTCompressionPropertyKey_AverageBitRate, kVTCompressionPropertyKey_DataRateLimits,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTProfileLevel_H264_High_AutoLevel,
    kVTProfileLevel_HEVC_Main42210_AutoLevel, kVTProfileLevel_HEVC_Main_AutoLevel,
    kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
};

use super::{
    cf_dict1, cm_time_us, codec_type, copy_bool_property, erase_dict, guard_callback, now_us,
    set_property, set_property_best_effort,
};
use crate::{
    ChromaSubsampling, Codec, CodecError, EncodedFrame, EncoderConfig, EncoderStats, Result, annexb,
};

/// Dữ liệu đi kèm mỗi frame từ lúc nộp đến lúc callback trả về.
///
/// VideoToolbox cho phép gắn một con trỏ tuỳ ý vào mỗi frame và trả lại nguyên
/// vẹn trong callback. Nhờ đó ta đo được thời gian mã hoá của đúng frame đó,
/// kể cả khi bộ mã hoá trả kết quả không theo thứ tự nộp.
struct SubmitInfo {
    pts_us: u64,
    submitted_us: u64,
    codec: Codec,
}

#[derive(Default)]
struct Counters {
    submitted: AtomicU64,
    emitted: AtomicU64,
    dropped: AtomicU64,
    bytes: AtomicU64,
    errors: AtomicU64,
}

struct Shared {
    tx: Mutex<Sender<EncodedFrame>>,
    counters: Counters,
}

pub struct VtEncoder {
    session: CFRetained<VTCompressionSession>,
    /// Callback nhận con trỏ thô tới đây, nên `Shared` phải sống lâu hơn phiên.
    /// `Drop` huỷ phiên trước khi trường này bị thả.
    shared: Arc<Shared>,
    rx: Receiver<EncodedFrame>,
    config: EncoderConfig,
    actual_chroma: ChromaSubsampling,
    hardware: bool,
    force_keyframe: bool,
    /// Từ điển tuỳ chọn chỉ dựng một lần, dùng lại cho mọi lần ép keyframe.
    force_keyframe_options: CFRetained<CFDictionary<CFType, CFType>>,
}

// An toàn để chuyển sang thread khác: phiên VideoToolbox là đối tượng
// CoreFoundation có khoá nội bộ, và mọi truy cập từ phía Rust đều đi qua
// `&mut self` nên không thể có hai lời gọi song song.
unsafe impl Send for VtEncoder {}

impl VtEncoder {
    pub fn new(config: EncoderConfig) -> Result<Self> {
        let codec = codec_type(config.codec)?;
        let (tx, rx) = channel();
        let shared = Arc::new(Shared {
            tx: Mutex::new(tx),
            counters: Counters::default(),
        });

        // Yêu cầu bộ mã hoá phần cứng. Không dùng cờ "Require" vì trên vài cấu
        // hình ảo hoá chỉ có bộ mã hoá phần mềm — chậm hơn nhưng vẫn chạy được.
        let spec = cf_dict1(
            unsafe { kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder },
            CFBoolean::new(true),
        );

        let mut raw: *mut VTCompressionSession = null_mut();
        let status = unsafe {
            VTCompressionSession::create(
                None,
                config.width as i32,
                config.height as i32,
                codec,
                Some(erase_dict(&spec)),
                None, // ta tự cấp pixel buffer từ ScreenCaptureKit
                None,
                Some(on_encoded),
                Arc::as_ptr(&shared) as *mut c_void,
                NonNull::from(&mut raw),
            )
        };
        let Some(raw) = NonNull::new(raw).filter(|_| status == 0) else {
            return Err(CodecError::SessionCreate {
                what: "mã hoá",
                status,
            });
        };
        let session = unsafe { CFRetained::from_raw(raw) };

        let actual_chroma = configure(&session, &config)?;

        let status = unsafe { session.prepare_to_encode_frames() };
        if status != 0 {
            tracing::debug!(status, "prepare_to_encode_frames báo lỗi, vẫn thử mã hoá");
        }

        let hardware = copy_bool_property(&session, unsafe {
            kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder
        })
        .unwrap_or(false);

        let force_keyframe_options = cf_dict1(
            unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame },
            CFBoolean::new(true),
        );

        tracing::info!(
            width = config.width,
            height = config.height,
            codec = ?config.codec,
            chroma = ?actual_chroma,
            hardware,
            "đã tạo phiên mã hoá"
        );

        Ok(Self {
            session,
            shared,
            rx,
            config,
            actual_chroma,
            hardware,
            force_keyframe: false,
            force_keyframe_options,
        })
    }

    /// Mức lấy mẫu màu thực tế phần cứng chấp nhận (có thể thấp hơn yêu cầu).
    pub fn actual_chroma(&self) -> ChromaSubsampling {
        self.actual_chroma
    }

    /// `true` nếu đang chạy trên mạch mã hoá phần cứng.
    pub fn is_hardware(&self) -> bool {
        self.hardware
    }

    pub fn config(&self) -> &EncoderConfig {
        &self.config
    }

    /// Yêu cầu frame kế tiếp là keyframe. Gọi khi viewer vừa kết nối hoặc báo
    /// mất dữ liệu — đây là cách duy nhất để hình ảnh hồi phục sau mất gói.
    pub fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    /// Đổi bitrate giữa chừng, không phải dựng lại phiên. Bộ điều khiển tắc
    /// nghẽn gọi hàm này mỗi khi băng thông khả dụng thay đổi.
    pub fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        set_property(
            &self.session,
            unsafe { kVTCompressionPropertyKey_AverageBitRate },
            &CFNumber::new_i32((kbps as i32).saturating_mul(1000)),
            "AverageBitRate",
        )?;
        set_data_rate_limits(&self.session, kbps);
        self.config.target_bitrate_kbps = kbps;
        Ok(())
    }

    /// Nộp một frame. Trả về ngay, kết quả lấy qua [`Self::next_frame`].
    pub fn submit(&mut self, pixels: &CVPixelBuffer, pts_us: u64) -> Result<()> {
        let info = Box::new(SubmitInfo {
            pts_us,
            submitted_us: now_us(),
            codec: self.config.codec,
        });
        let options = if self.force_keyframe {
            Some(erase_dict(&self.force_keyframe_options))
        } else {
            None
        };

        let duration = cm_time_us((1_000_000 / self.config.target_fps.max(1)) as i64);
        let mut flags = VTEncodeInfoFlags::empty();
        let refcon = Box::into_raw(info);
        let status = unsafe {
            self.session.encode_frame(
                pixels,
                cm_time_us(pts_us as i64),
                duration,
                options,
                refcon as *mut c_void,
                &mut flags,
            )
        };
        if status != 0 {
            // Nộp thất bại thì callback không chạy, nên phải tự thu hồi.
            drop(unsafe { Box::from_raw(refcon) });
            self.shared.counters.errors.fetch_add(1, Ordering::Relaxed);
            return Err(CodecError::Encode(status));
        }

        self.force_keyframe = false;
        self.shared.counters.submitted.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Lấy frame đã nén nếu có sẵn, không chờ.
    pub fn try_next_frame(&mut self) -> Option<EncodedFrame> {
        // Kênh đóng cũng trả `None` như khi rỗng: bên gọi là vòng lặp vẽ, nó
        // chỉ cần biết "chưa có frame". Bộ mã hoá chết thì `next_frame` báo.
        self.rx.try_recv().ok()
    }

    /// Chờ frame đã nén trong tối đa `timeout`.
    pub fn next_frame(&mut self, timeout: Duration) -> Result<EncodedFrame> {
        self.rx.recv_timeout(timeout).map_err(|err| match err {
            std::sync::mpsc::RecvTimeoutError::Timeout => CodecError::Timeout,
            std::sync::mpsc::RecvTimeoutError::Disconnected => CodecError::Closed,
        })
    }

    /// Buộc bộ mã hoá phát hết frame còn trong hàng đợi. Chỉ dùng khi kết thúc
    /// phiên hoặc khi đo đạc — trong lúc chạy thật, gọi hàm này sẽ làm nghẽn.
    pub fn flush(&mut self) -> Result<()> {
        let status = unsafe { self.session.complete_frames(objc2_core_media::kCMTimeInvalid) };
        if status == 0 {
            Ok(())
        } else {
            Err(CodecError::Encode(status))
        }
    }

    pub fn stats(&self) -> EncoderStats {
        let c = &self.shared.counters;
        EncoderStats {
            frames_submitted: c.submitted.load(Ordering::Relaxed),
            frames_emitted: c.emitted.load(Ordering::Relaxed),
            frames_dropped: c.dropped.load(Ordering::Relaxed),
            bytes_emitted: c.bytes.load(Ordering::Relaxed),
            errors: c.errors.load(Ordering::Relaxed),
        }
    }
}

impl Drop for VtEncoder {
    fn drop(&mut self) {
        // Phải huỷ phiên *trước* khi `shared` bị thả: callback đang chạy vẫn
        // giữ con trỏ thô tới nó. `invalidate` chờ mọi callback kết thúc.
        unsafe { self.session.invalidate() };
    }
}

/// Đặt toàn bộ thuộc tính cho phiên, trả về mức lấy mẫu màu thực tế đạt được.
fn configure(session: &VTCompressionSession, config: &EncoderConfig) -> Result<ChromaSubsampling> {
    // Hai thuộc tính dưới đây quyết định độ trễ, thiếu là hỏng mục tiêu.
    set_property(
        session,
        unsafe { kVTCompressionPropertyKey_RealTime },
        CFBoolean::new(true),
        "RealTime",
    )?;
    set_property(
        session,
        unsafe { kVTCompressionPropertyKey_AllowFrameReordering },
        CFBoolean::new(false),
        "AllowFrameReordering",
    )?;

    let chroma = apply_profile(session, config);

    set_property(
        session,
        unsafe { kVTCompressionPropertyKey_AverageBitRate },
        &CFNumber::new_i32((config.target_bitrate_kbps as i32).saturating_mul(1000)),
        "AverageBitRate",
    )?;
    set_data_rate_limits(session, config.target_bitrate_kbps);

    set_property_best_effort(
        session,
        unsafe { kVTCompressionPropertyKey_ExpectedFrameRate },
        &CFNumber::new_i32(config.target_fps as i32),
        "ExpectedFrameRate",
    );
    set_property_best_effort(
        session,
        unsafe { kVTCompressionPropertyKey_MaxKeyFrameInterval },
        &CFNumber::new_i32((config.target_fps * config.keyframe_interval_secs).max(1) as i32),
        "MaxKeyFrameInterval",
    );
    set_property_best_effort(
        session,
        unsafe { kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration },
        &CFNumber::new_f64(config.keyframe_interval_secs as f64),
        "MaxKeyFrameIntervalDuration",
    );
    // Có từ macOS 14; đúng đánh đổi cho điều khiển từ xa: nhanh hơn, hơi to hơn.
    set_property_best_effort(
        session,
        unsafe { kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality },
        CFBoolean::new(true),
        "PrioritizeEncodingSpeedOverQuality",
    );

    Ok(chroma)
}

/// Chọn profile cao nhất mà phần cứng chấp nhận.
///
/// 4:2:0 chỉ giữ một mẫu màu cho mỗi ô 2x2 pixel, nên viền chữ màu bị nhoè rõ
/// rệt — thứ ta nhìn suốt khi điều khiển desktop. 4:2:2 giữ đủ màu theo chiều
/// dọc, nét hơn hẳn. Bộ mã hoá của Apple silicon hỗ trợ tới 4:2:2 10-bit; máy
/// Intel cũ hơn thì không, nên phải dò rồi lùi.
fn apply_profile(session: &VTCompressionSession, config: &EncoderConfig) -> ChromaSubsampling {
    let wants_high_chroma = matches!(
        config.chroma,
        ChromaSubsampling::Yuv422 | ChromaSubsampling::Yuv444
    );

    if config.codec == Codec::Hevc && wants_high_chroma {
        let key = unsafe { kVTCompressionPropertyKey_ProfileLevel };
        let value = unsafe { kVTProfileLevel_HEVC_Main42210_AutoLevel };
        if set_property(session, key, value, "ProfileLevel").is_ok() {
            return ChromaSubsampling::Yuv422;
        }
        tracing::info!("phần cứng không nhận HEVC 4:2:2, lùi về 4:2:0");
    }

    let fallback = match config.codec {
        Codec::Hevc => unsafe { kVTProfileLevel_HEVC_Main_AutoLevel },
        _ => unsafe { kVTProfileLevel_H264_High_AutoLevel },
    };
    set_property_best_effort(
        session,
        unsafe { kVTCompressionPropertyKey_ProfileLevel },
        fallback,
        "ProfileLevel",
    );
    ChromaSubsampling::Yuv420
}

/// Chặn bộ mã hoá bắn ra một cú nổ dữ liệu lớn hơn nhiều lần bitrate trung
/// bình. Một keyframe không giới hạn có thể lấp đầy hàng đợi mạng và đẩy độ trễ
/// lên hàng trăm mili giây, dù bitrate trung bình vẫn "đúng".
fn set_data_rate_limits(session: &VTCompressionSession, kbps: u32) {
    let burst_bytes = (kbps as f64 * 1000.0 / 8.0 * 1.5) as i64;
    let bytes = CFNumber::new_i64(burst_bytes);
    let seconds = CFNumber::new_f64(1.0);
    let bytes: &CFType = &bytes;
    let seconds: &CFType = &seconds;
    let limits = CFArray::from_objects(&[bytes, seconds]);
    set_property_best_effort(
        session,
        unsafe { kVTCompressionPropertyKey_DataRateLimits },
        &limits,
        "DataRateLimits",
    );
}

/// VideoToolbox gọi hàm này trên thread riêng của nó cho mỗi frame đã nộp —
/// kể cả frame bị bỏ (khi đó `sample_buffer` là null).
unsafe extern "C-unwind" fn on_encoded(
    output_ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: i32,
    _flags: VTEncodeInfoFlags,
    sample_buffer: *mut CMSampleBuffer,
) {
    guard_callback("VTCompressionOutputCallback", || {
        if source_frame_ref_con.is_null() || output_ref_con.is_null() {
            return;
        }
        // Nhận lại quyền sở hữu để giải phóng, dù frame thành công hay không.
        let info = unsafe { Box::from_raw(source_frame_ref_con as *mut SubmitInfo) };
        let shared = unsafe { &*(output_ref_con as *const Shared) };

        if status != 0 {
            shared.counters.errors.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(status, "mã hoá một frame thất bại");
            return;
        }
        let Some(sample) = (unsafe { sample_buffer.as_ref() }) else {
            shared.counters.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };

        match unsafe { to_annexb(sample, info.codec) } {
            Ok((data, keyframe)) => {
                shared
                    .counters
                    .bytes
                    .fetch_add(data.len() as u64, Ordering::Relaxed);
                shared.counters.emitted.fetch_add(1, Ordering::Relaxed);
                let frame = EncodedFrame {
                    data,
                    keyframe,
                    pts_us: info.pts_us,
                    encode_us: now_us().saturating_sub(info.submitted_us) as u32,
                };
                // Lỗi gửi chỉ có nghĩa là phía nhận đã bỏ đi — không phải lỗi.
                if let Ok(tx) = shared.tx.lock() {
                    let _ = tx.send(frame);
                }
            }
            Err(err) => {
                shared.counters.errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(%err, "không đọc được bitstream từ sample buffer");
            }
        }
    });
}

/// Đổi sample buffer của VideoToolbox thành Annex B, chèn parameter set trước
/// keyframe. Trả về `(dữ liệu, có phải keyframe không)`.
unsafe fn to_annexb(sample: &CMSampleBuffer, codec: Codec) -> Result<(Vec<u8>, bool)> {
    let block = unsafe { sample.data_buffer() }
        .ok_or(CodecError::Bitstream(annexb::BitstreamError::Truncated))?;

    let mut length_at_offset = 0usize;
    let mut total_length = 0usize;
    let mut ptr: *mut std::ffi::c_char = null_mut();
    let status =
        unsafe { block.data_pointer(0, &mut length_at_offset, &mut total_length, &mut ptr) };
    if status != 0 || ptr.is_null() {
        return Err(CodecError::Encode(status));
    }

    // Nếu buffer bị chia mảnh, phần liền mạch đầu tiên ngắn hơn tổng độ dài;
    // khi đó phải copy gom lại trước khi phân tích.
    let owned;
    let payload: &[u8] = if length_at_offset == total_length {
        unsafe { std::slice::from_raw_parts(ptr as *const u8, total_length) }
    } else {
        let mut buf = vec![0u8; total_length];
        let status = unsafe {
            block.copy_data_bytes(
                0,
                total_length,
                NonNull::new_unchecked(buf.as_mut_ptr().cast::<std::ffi::c_void>()),
            )
        };
        if status != 0 {
            return Err(CodecError::Encode(status));
        }
        owned = buf;
        &owned
    };

    // Chuyển sang Annex B trước, rồi mới biết có keyframe hay không.
    let mut body = Vec::with_capacity(payload.len() + 256);
    annexb::length_prefixed_to_annexb(payload, &mut body)?;

    let classify = match codec {
        Codec::Hevc => annexb::classify_hevc,
        _ => annexb::classify_h264,
    };
    let keyframe = annexb::iter_nalus(&body).any(|n| classify(n) == annexb::NaluClass::Keyframe);

    if !keyframe {
        return Ok((body, false));
    }

    // Keyframe: chèn VPS/SPS/PPS vào trước. Không có chúng, viewer vừa kết nối
    // sẽ không giải mã được gì cả.
    let desc = unsafe { sample.format_description() }.ok_or(CodecError::MissingParameterSets)?;
    let mut out = Vec::with_capacity(body.len() + 256);
    unsafe { push_parameter_sets(&desc, codec, &mut out)? };
    out.extend_from_slice(&body);
    Ok((out, true))
}

unsafe fn push_parameter_sets(
    desc: &CMFormatDescription,
    codec: Codec,
    out: &mut Vec<u8>,
) -> Result<()> {
    use objc2_core_media::{
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
        CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
    };

    // Hai hàm có cùng chữ ký nên gọi được qua một con trỏ hàm chung.
    let get: unsafe extern "C-unwind" fn(
        &CMFormatDescription,
        usize,
        *mut *const u8,
        *mut usize,
        *mut usize,
        *mut c_int,
    ) -> i32 = match codec {
        Codec::Hevc => CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
        Codec::H264 => CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
        Codec::Av1 => return Err(CodecError::Unsupported("AV1 chưa hỗ trợ".into())),
    };

    let mut count = 0usize;
    let status = unsafe { get(desc, 0, null_mut(), null_mut(), &mut count, null_mut()) };
    if status != 0 || count == 0 {
        return Err(CodecError::MissingParameterSets);
    }

    for index in 0..count {
        let mut ptr: *const u8 = null();
        let mut size = 0usize;
        let mut nal_length = 0 as c_int;
        let status = unsafe { get(desc, index, &mut ptr, &mut size, null_mut(), &mut nal_length) };
        if status != 0 || ptr.is_null() {
            return Err(CodecError::MissingParameterSets);
        }
        annexb::push_nalu(out, unsafe { std::slice::from_raw_parts(ptr, size) });
    }
    Ok(())
}
