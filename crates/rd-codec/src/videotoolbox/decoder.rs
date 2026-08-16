use std::ffi::{c_int, c_void};
use std::ptr::{NonNull, null_mut};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, Ordering};

use objc2_core_foundation::{CFBoolean, CFNumber, CFRetained, CFType};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo,
    CMVideoFormatDescriptionCreateFromH264ParameterSets,
    CMVideoFormatDescriptionCreateFromHEVCParameterSets, kCMTimeInvalid,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetWidth,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
    kCVPixelFormatType_422YpCbCr10BiPlanarVideoRange,
};
use objc2_video_toolbox::{
    VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord,
    VTDecompressionSession, kVTDecompressionPropertyKey_RealTime,
};

use super::{
    NAL_LENGTH_SIZE, cf_dict, cm_time_us, erase_dict, guard_callback, now_us,
    set_property_best_effort,
};
use crate::{ChromaSubsampling, Codec, CodecError, DecoderStats, FrameFormat, Result, annexb};

/// Frame đã giải mã, vẫn nằm trên bộ nhớ GPU.
///
/// Không kéo pixel về CPU: viewer nạp thẳng `CVPixelBuffer` này vào texture của
/// wgpu qua IOSurface. Một frame 4K là 33 MB — copy nó mỗi lần vẽ sẽ ngốn hết
/// ngân sách độ trễ.
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub format: FrameFormat,
    pub pts_us: u64,
    pub decode_us: u32,
    pixels: CFRetained<CVPixelBuffer>,
}

impl DecodedFrame {
    pub fn pixel_buffer(&self) -> &CVPixelBuffer {
        &self.pixels
    }
}

impl std::fmt::Debug for DecodedFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodedFrame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("format", &self.format)
            .field("pts_us", &self.pts_us)
            .field("decode_us", &self.decode_us)
            .finish_non_exhaustive()
    }
}

// CVPixelBuffer là đối tượng CoreFoundation: đếm tham chiếu của nó nguyên tử,
// và ta chuyển hẳn quyền sở hữu sang thread khác chứ không dùng chung.
unsafe impl Send for DecodedFrame {}

// Sau khi tạo, frame là bất biến: mọi phương thức đều nhận `&self` và chỉ đọc.
// Muốn ghi vào pixel buffer phải gọi hàm `unsafe` của CoreVideo, tức là người
// gọi tự nhận trách nhiệm. Nhờ vậy nhiều luồng cùng đọc một frame là an toàn,
// và viewer có thể giữ frame trong `Arc` để chia cho luồng render.
unsafe impl Sync for DecodedFrame {}

/// Nơi callback đặt kết quả. Giải mã chạy đồng bộ nên chỉ cần một ô.
#[derive(Default)]
struct Slot {
    frame: Mutex<Option<CFRetained<CVPixelBuffer>>>,
    /// Mã lỗi callback báo về, 0 là không có. Tách khỏi `frame` vì hai trường
    /// hợp "không có hình" khác hẳn nhau: chưa tới keyframe thì chỉ cần chờ,
    /// còn dữ liệu hỏng thì phải xin keyframe mới, chờ mãi cũng không tự khỏi.
    status: AtomicI32,
}

pub struct VtDecoder {
    codec: Codec,
    format_out: FrameFormat,
    session: Option<CFRetained<VTDecompressionSession>>,
    format: Option<CFRetained<CMFormatDescription>>,
    /// Parameter set đang dùng. Khi host đổi độ phân giải, chuỗi này đổi theo
    /// và ta phải dựng lại cả format description lẫn phiên giải mã.
    parameter_sets: Vec<Vec<u8>>,
    slot: Box<Slot>,
    stats: DecoderStats,
    bad_data: bool,
}

// Cùng lý do như bộ mã hoá: mọi truy cập đi qua `&mut self`.
unsafe impl Send for VtDecoder {}

impl VtDecoder {
    /// `chroma` phải khớp mức mà bên mã hoá thực sự dùng
    /// ([`VtEncoder::actual_chroma`]), vì nó quyết định định dạng frame trả ra.
    /// Chọn sai không làm hỏng ảnh — VideoToolbox sẽ tự chuyển đổi — nhưng phải
    /// trả giá bằng một lần chuyển màu thừa trên mỗi frame.
    pub fn new(codec: Codec, chroma: ChromaSubsampling) -> Result<Self> {
        if codec == Codec::Av1 {
            return Err(CodecError::Unsupported(
                "AV1 chưa được hỗ trợ trên đường VideoToolbox".into(),
            ));
        }
        let format_out = match chroma {
            ChromaSubsampling::Yuv420 => FrameFormat::Nv12VideoRange,
            ChromaSubsampling::Yuv422 | ChromaSubsampling::Yuv444 => FrameFormat::P210VideoRange,
        };
        Ok(Self {
            codec,
            format_out,
            session: None,
            format: None,
            parameter_sets: Vec::new(),
            slot: Box::new(Slot::default()),
            stats: DecoderStats::default(),
            bad_data: false,
        })
    }

    pub fn stats(&self) -> DecoderStats {
        self.stats
    }

    pub fn frame_format(&self) -> FrameFormat {
        self.format_out
    }

    /// Bộ giải mã có từ chối dữ liệu kể từ lần hỏi trước không, đồng thời xoá
    /// cờ. Gần như luôn có nghĩa là mất frame tham chiếu trên đường truyền:
    /// chuỗi dự đoán đứt và mọi frame sau đó vô dụng cho tới keyframe kế tiếp,
    /// nên phía gọi phải xin host phát keyframe chứ đừng chờ.
    pub fn take_bad_data(&mut self) -> bool {
        std::mem::take(&mut self.bad_data)
    }

    /// Giải mã một access unit dạng Annex B.
    ///
    /// Trả `Ok(None)` khi frame không giải mã được *và đó là điều bình thường*:
    /// dữ liệu đến trước keyframe đầu tiên thì không có parameter set để dựng
    /// bộ giải mã. Viewer chỉ cần chờ keyframe kế tiếp.
    pub fn decode(&mut self, data: &[u8], pts_us: u64) -> Result<Option<DecodedFrame>> {
        self.stats.frames_submitted += 1;

        let classify = match self.codec {
            Codec::Hevc => annexb::classify_hevc,
            _ => annexb::classify_h264,
        };

        let mut parameter_sets: Vec<Vec<u8>> = Vec::new();
        let mut payload: Vec<&[u8]> = Vec::new();
        for nalu in annexb::iter_nalus(data) {
            match classify(nalu) {
                annexb::NaluClass::ParameterSet => parameter_sets.push(nalu.to_vec()),
                annexb::NaluClass::Keyframe | annexb::NaluClass::Delta => payload.push(nalu),
                annexb::NaluClass::Other => payload.push(nalu),
            }
        }

        if !parameter_sets.is_empty() && parameter_sets != self.parameter_sets {
            self.rebuild(&parameter_sets)?;
            self.parameter_sets = parameter_sets;
        }

        let (Some(session), Some(format)) = (self.session.as_ref(), self.format.as_ref()) else {
            self.stats.frames_dropped += 1;
            return Ok(None);
        };
        if payload.is_empty() {
            // Gói chỉ chứa parameter set — hợp lệ, chưa có hình để trả.
            return Ok(None);
        }

        // VideoToolbox chỉ nhận dạng length-prefix, ngược với dạng Annex B ta
        // dùng trên đường truyền.
        let mut prefixed = Vec::with_capacity(data.len());
        annexb::annexb_to_length_prefixed(&payload, &mut prefixed);

        let started_us = now_us();
        let sample = unsafe { make_sample_buffer(&prefixed, format, pts_us)? };

        self.slot.frame.lock().expect("slot không poison").take();
        self.slot.status.store(0, Ordering::Relaxed);
        let mut info = VTDecodeInfoFlags::empty();
        // Cờ rỗng = giải mã đồng bộ: hàm chỉ trả về sau khi callback đã chạy,
        // nên đọc `slot` ngay bên dưới là an toàn và không cần chờ đợi gì.
        let status = unsafe {
            session.decode_frame(&sample, VTDecodeFrameFlags::empty(), null_mut(), &mut info)
        };
        if status != 0 {
            self.stats.errors += 1;
            return Err(CodecError::Decode(status));
        }

        let Some(pixels) = self.slot.frame.lock().expect("slot không poison").take() else {
            self.stats.frames_dropped += 1;
            let status = self.slot.status.swap(0, Ordering::Relaxed);
            if status != 0 {
                self.stats.errors += 1;
                self.bad_data = true;
                tracing::debug!(status, "bộ giải mã từ chối dữ liệu");
            }
            return Ok(None);
        };

        self.stats.frames_emitted += 1;
        self.bad_data = false;
        Ok(Some(DecodedFrame {
            width: CVPixelBufferGetWidth(&pixels) as u32,
            height: CVPixelBufferGetHeight(&pixels) as u32,
            format: self.format_out,
            pts_us,
            decode_us: now_us().saturating_sub(started_us) as u32,
            pixels,
        }))
    }

    /// Dựng lại format description và phiên giải mã theo parameter set mới.
    fn rebuild(&mut self, parameter_sets: &[Vec<u8>]) -> Result<()> {
        let format = unsafe { make_format_description(self.codec, parameter_sets)? };

        let record = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(on_decoded),
            decompressionOutputRefCon: &*self.slot as *const Slot as *mut c_void,
        };

        // Ghim định dạng đầu ra. Không ghim thì VideoToolbox tự chọn, và nó có
        // thể trả về định dạng riêng của Apple không có trong SDK công khai.
        // Hai khoá còn lại buộc buffer nằm trên IOSurface tương thích Metal —
        // điều kiện để viewer nạp thẳng vào texture mà không copy.
        let fourcc = match self.format_out {
            FrameFormat::Nv12VideoRange => kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            FrameFormat::P210VideoRange => kCVPixelFormatType_422YpCbCr10BiPlanarVideoRange,
        };
        let fourcc = CFNumber::new_i32(fourcc as i32);
        let yes = CFBoolean::new(true);
        let empty: CFRetained<objc2_core_foundation::CFDictionary<CFType, CFType>> =
            objc2_core_foundation::CFDictionary::from_slices(&[], &[]);
        let attributes = cf_dict(&[
            (unsafe { kCVPixelBufferPixelFormatTypeKey }, &fourcc),
            (unsafe { kCVPixelBufferMetalCompatibilityKey }, yes),
            (unsafe { kCVPixelBufferIOSurfacePropertiesKey }, &empty),
        ]);

        let mut raw: *mut VTDecompressionSession = null_mut();
        let status = unsafe {
            VTDecompressionSession::create(
                None,
                &format,
                None,
                Some(erase_dict(&attributes)),
                &record,
                NonNull::from(&mut raw),
            )
        };
        let Some(raw) = NonNull::new(raw).filter(|_| status == 0) else {
            return Err(CodecError::SessionCreate {
                what: "giải mã",
                status,
            });
        };
        let session = unsafe { CFRetained::from_raw(raw) };

        set_property_best_effort(
            &session,
            unsafe { kVTDecompressionPropertyKey_RealTime },
            CFBoolean::new(true),
            "RealTime",
        );

        // Huỷ phiên cũ trước khi thay, để callback đang chạy kết thúc hẳn.
        if let Some(old) = self.session.take() {
            unsafe { old.invalidate() };
        }
        self.session = Some(session);
        self.format = Some(format);
        tracing::debug!(codec = ?self.codec, "dựng lại phiên giải mã theo parameter set mới");
        Ok(())
    }
}

impl Drop for VtDecoder {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            unsafe { session.invalidate() };
        }
    }
}

unsafe fn make_format_description(
    codec: Codec,
    parameter_sets: &[Vec<u8>],
) -> Result<CFRetained<CMFormatDescription>> {
    let pointers: Vec<NonNull<u8>> = parameter_sets
        .iter()
        .map(|set| NonNull::new(set.as_ptr().cast_mut()).ok_or(CodecError::MissingParameterSets))
        .collect::<Result<_>>()?;
    let sizes: Vec<usize> = parameter_sets.iter().map(|set| set.len()).collect();

    let mut raw: *const CMFormatDescription = std::ptr::null();
    let status = match codec {
        Codec::Hevc => unsafe {
            CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                None,
                pointers.len(),
                NonNull::from(&pointers[0]),
                NonNull::from(&sizes[0]),
                NAL_LENGTH_SIZE as c_int,
                None,
                NonNull::from(&mut raw),
            )
        },
        Codec::H264 => unsafe {
            CMVideoFormatDescriptionCreateFromH264ParameterSets(
                None,
                pointers.len(),
                NonNull::from(&pointers[0]),
                NonNull::from(&sizes[0]),
                NAL_LENGTH_SIZE as c_int,
                NonNull::from(&mut raw),
            )
        },
        Codec::Av1 => return Err(CodecError::Unsupported("AV1 chưa hỗ trợ".into())),
    };
    let Some(raw) = NonNull::new(raw.cast_mut()).filter(|_| status == 0) else {
        return Err(CodecError::Decode(status));
    };
    Ok(unsafe { CFRetained::from_raw(raw) })
}

/// Bọc dữ liệu đã nén thành `CMSampleBuffer` mà không copy.
///
/// # Safety
///
/// `payload` phải sống lâu hơn sample buffer trả về. Ở đây điều đó được bảo đảm
/// vì lời gọi giải mã là đồng bộ và cả hai đều nằm trên stack của `decode`.
unsafe fn make_sample_buffer(
    payload: &[u8],
    format: &CMFormatDescription,
    pts_us: u64,
) -> Result<CFRetained<CMSampleBuffer>> {
    let mut block: *mut CMBlockBuffer = null_mut();
    let status = unsafe {
        CMBlockBuffer::create_with_memory_block(
            None,
            payload.as_ptr().cast_mut() as *mut c_void,
            payload.len(),
            objc2_core_foundation::kCFAllocatorNull,
            std::ptr::null(),
            0,
            payload.len(),
            0,
            NonNull::from(&mut block),
        )
    };
    let Some(block) = NonNull::new(block).filter(|_| status == 0) else {
        return Err(CodecError::Decode(status));
    };
    let block = unsafe { CFRetained::from_raw(block) };

    let timing = CMSampleTimingInfo {
        duration: unsafe { kCMTimeInvalid },
        presentationTimeStamp: cm_time_us(pts_us as i64),
        decodeTimeStamp: unsafe { kCMTimeInvalid },
    };
    let size = payload.len();

    let mut sample: *mut CMSampleBuffer = null_mut();
    let status = unsafe {
        CMSampleBuffer::create_ready(
            None,
            Some(&block),
            Some(format),
            1,
            1,
            &timing,
            1,
            &size,
            NonNull::from(&mut sample),
        )
    };
    let Some(sample) = NonNull::new(sample).filter(|_| status == 0) else {
        return Err(CodecError::Decode(status));
    };
    Ok(unsafe { CFRetained::from_raw(sample) })
}

unsafe extern "C-unwind" fn on_decoded(
    ref_con: *mut c_void,
    _source_ref_con: *mut c_void,
    status: i32,
    _flags: VTDecodeInfoFlags,
    image_buffer: *mut CVImageBuffer,
    _pts: objc2_core_media::CMTime,
    _duration: objc2_core_media::CMTime,
) {
    guard_callback("VTDecompressionOutputCallback", || {
        if ref_con.is_null() {
            return;
        }
        let slot = unsafe { &*(ref_con as *const Slot) };
        if status != 0 {
            // Không log ở đây: mất một frame tham chiếu là hỏng *mọi* frame sau
            // đó cho tới keyframe kế tiếp, tức hàng trăm dòng giống hệt nhau.
            // Phía gọi gộp lại thành một dòng cho mỗi đợt.
            slot.status.store(status, Ordering::Relaxed);
            return;
        }
        let Some(buffer) = NonNull::new(image_buffer) else {
            return;
        };
        // Callback chỉ mượn buffer; phải giữ lại thì mới dùng được sau khi trả về.
        let retained = unsafe { CFRetained::retain(buffer) };
        if let Ok(mut guard) = slot.frame.lock() {
            *guard = Some(retained);
        }
    });
}
