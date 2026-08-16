//! Bộ giải mã video, ra thẳng texture D3D11 để viewer vẽ không qua CPU.
//!
//! Khác bộ mã hoá, ở đây dùng MFT **đồng bộ** chứ không phải MFT phần cứng bất
//! đồng bộ. Nghe như đi ngược mục tiêu tốc độ, nhưng không phải: bộ giải mã
//! H.264/HEVC dựng sẵn của Windows là loại đồng bộ *có* tăng tốc phần cứng —
//! phần giải mã thật vẫn chạy trên khối mạch video của GPU qua DXVA, ta chỉ
//! điều khiển nó theo kiểu gọi-và-chờ. Đổi lại:
//!
//! - Nó có trên mọi máy Windows và mọi GPU, không phụ thuộc driver hãng nào.
//! - Gọi-và-chờ hợp với chỗ này: viewer giải mã đúng một frame rồi vẽ ngay,
//!   không có gì để làm song song trong lúc chờ.
//!
//! Frame ra là texture nằm trong mảng texture của chính bộ giải mã. Ta giữ
//! `IMFSample` sống bên trong [`DecodedFrame`] để bộ giải mã không tái dụng ô
//! đó trong lúc viewer còn đang vẽ.

use std::ptr::null_mut;
use std::time::Instant;

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::{
    IMFDXGIBuffer, IMFDXGIDeviceManager, IMFSample, IMFTransform, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_LOW_LATENCY, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE,
    MF_MT_SUBTYPE, MF_SA_D3D11_AWARE, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video,
    MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MFVideoFormat_H264,
    MFVideoFormat_HEVC, MFVideoFormat_NV12,
};
use windows::core::{GUID, Interface};

use crate::mf_params::{attribute_pair, hundred_ns};
use crate::{Codec, CodecError, DecoderStats, FrameFormat, Result};

use super::{activate_first, attach_device, ensure_started, system};

/// Một frame đã giải mã, vẫn nằm trong VRAM.
pub struct DecodedFrame {
    texture: ID3D11Texture2D,
    /// Bộ giải mã trả về *mảng* texture chứ không phải texture rời, nên chỉ
    /// riêng con trỏ texture chưa đủ để biết ảnh nằm ở đâu.
    subresource: u32,
    pub width: u32,
    pub height: u32,
    pub format: FrameFormat,
    pub pts_us: u64,
    pub decode_us: u32,
    /// Không đọc tới, chỉ giữ cho ô texture khỏi bị bộ giải mã dùng lại khi
    /// viewer còn đang vẽ.
    _sample: IMFSample,
}

impl DecodedFrame {
    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.texture
    }

    pub fn subresource(&self) -> u32 {
        self.subresource
    }
}

impl std::fmt::Debug for DecodedFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodedFrame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("subresource", &self.subresource)
            .field("pts_us", &self.pts_us)
            .finish()
    }
}

// An toàn: texture D3D11 và sample của Media Foundation đều free-threaded, và
// `DecodedFrame` không cho mượn ra ngoài thứ gì gắn với một luồng cụ thể.
//
// `Sync` cũng cần: luồng chuỗi xử lý gói frame vào `Arc` rồi đưa sang luồng
// giao diện, mà `Arc<T>` chỉ gửi đi được khi `T` có cả hai. Frame chỉ đọc từ
// lúc dựng xong nên chia sẻ tham chiếu không đẻ ra tranh chấp nào.
unsafe impl Send for DecodedFrame {}
unsafe impl Sync for DecodedFrame {}

pub struct MfDecoder {
    transform: IMFTransform,
    codec: Codec,
    width: u32,
    height: u32,
    /// Kiểu đầu ra chỉ chốt được sau khi bộ giải mã đọc xong tham số của dòng
    /// dữ liệu, nên lần `ProcessOutput` đầu tiên luôn báo "đổi định dạng".
    output_ready: bool,
    _device_manager: IMFDXGIDeviceManager,
    stats: DecoderStats,
    bad_data: bool,
}

// An toàn: mọi con trỏ COM bên trong đều free-threaded, và `&mut self` của
// `decode` đã ngăn hai luồng cùng gọi.
unsafe impl Send for MfDecoder {}

impl MfDecoder {
    /// `device` phải là device D3D11 mà viewer dùng để vẽ — texture của hai
    /// device khác nhau không dùng chung được, và bắc cầu giữa chúng là đúng
    /// cái lần copy mà cả đường đi này sinh ra để tránh.
    ///
    /// `width`/`height` chỉ là gợi ý ban đầu; kích thước thật lấy từ chính dòng
    /// dữ liệu khi bộ giải mã chốt kiểu đầu ra.
    pub fn new(codec: Codec, width: u32, height: u32, device: &ID3D11Device) -> Result<Self> {
        ensure_started()?;

        let info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: subtype(codec)?,
        };
        let transform = activate_first(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&info),
            None,
            &format!("bộ giải mã {codec:?}"),
        )?;

        let attributes = unsafe { transform.GetAttributes() }
            .map_err(|err| system("đọc thuộc tính của bộ giải mã", err))?;

        // Không nhận D3D11 thì nó sẽ giải mã bằng CPU và trả frame trong RAM
        // thường — chạy được nhưng ở 4K là mất hàng chục ms mỗi frame cộng một
        // lần tải ngược lên GPU. Thà báo lỗi để bên gọi biết mà xử lý.
        if unsafe { attributes.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0) == 0 {
            return Err(CodecError::Unsupported(
                "bộ giải mã của máy này không nhận texture D3D11".into(),
            ));
        }
        // Bảo bộ giải mã đừng gom frame lại xử lý theo lô.
        let _ = unsafe { attributes.SetUINT32(&MF_LOW_LATENCY, 1) };

        let device_manager = attach_device(&transform, device)?;
        set_input_type(&transform, codec, width, height)?;

        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|err| system("báo bộ giải mã bắt đầu luồng", err))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|err| system("báo bộ giải mã bắt đầu dòng dữ liệu", err))?;
        }

        Ok(Self {
            transform,
            codec,
            width,
            height,
            output_ready: false,
            _device_manager: device_manager,
            stats: DecoderStats::default(),
            bad_data: false,
        })
    }

    pub fn stats(&self) -> DecoderStats {
        self.stats
    }

    pub fn frame_format(&self) -> FrameFormat {
        FrameFormat::Nv12VideoRange
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// Bộ giải mã có từ chối dữ liệu kể từ lần hỏi trước không, đồng thời xoá
    /// cờ. Gần như luôn có nghĩa là mất frame tham chiếu trên đường truyền:
    /// chuỗi dự đoán đứt và mọi frame sau đó vô dụng cho tới keyframe kế tiếp,
    /// nên phía gọi phải xin host phát keyframe chứ đừng chờ.
    ///
    /// Media Foundation không có mã lỗi riêng cho việc này — nó chỉ lặng lẽ
    /// không trả frame. Nên ta suy ra: sau khi bộ giải mã đã phát được ít nhất
    /// một frame, một access unit không cho ra gì nghĩa là nó bị từ chối (dòng
    /// này không có B-frame nên không có chuyện giữ lại để sắp xếp lại thứ tự).
    pub fn take_bad_data(&mut self) -> bool {
        std::mem::take(&mut self.bad_data)
    }

    /// Ghi nhận một access unit không cho ra hình.
    fn note_no_output(&mut self) {
        self.stats.frames_dropped += 1;
        if self.stats.frames_emitted > 0 {
            self.stats.errors += 1;
            self.bad_data = true;
        }
    }

    /// Giải mã một access unit dạng Annex B.
    ///
    /// Trả `Ok(None)` khi chưa có hình ra, và đó là chuyện bình thường: dữ liệu
    /// tới trước keyframe đầu tiên thì chưa đủ tham số để dựng ảnh, còn gói chỉ
    /// chứa parameter set thì vốn không mang hình nào.
    ///
    /// Khác bản VideoToolbox: không phải đổi sang dạng length-prefix, bộ giải
    /// mã của Windows nhận thẳng Annex B và tự đọc parameter set trong dòng.
    pub fn decode(&mut self, data: &[u8], pts_us: u64) -> Result<Option<DecodedFrame>> {
        self.stats.frames_submitted += 1;
        let started = Instant::now();

        let sample = make_sample(data, hundred_ns(pts_us))?;
        if let Err(err) = unsafe { self.transform.ProcessInput(0, &sample, 0) } {
            self.stats.errors += 1;
            return Err(system("nạp dữ liệu vào bộ giải mã", err));
        }

        // Vòng lặp chỉ quay lại đúng một lần: khi bộ giải mã báo đã đọc xong
        // tham số dòng và cần ta chốt lại kiểu đầu ra.
        for _ in 0..2 {
            match self.pull()? {
                Pulled::Frame(sample) => {
                    let frame = self.wrap(sample, pts_us, started)?;
                    self.stats.frames_emitted += 1;
                    self.bad_data = false;
                    return Ok(Some(frame));
                }
                Pulled::NeedInput => {
                    self.note_no_output();
                    return Ok(None);
                }
                Pulled::StreamChange => self.negotiate_output()?,
            }
        }

        self.note_no_output();
        Ok(None)
    }

    fn pull(&mut self) -> Result<Pulled> {
        if !self.output_ready {
            return Ok(Pulled::StreamChange);
        }

        let info = unsafe { self.transform.GetOutputStreamInfo(0) }
            .map_err(|err| system("hỏi thông tin luồng ra", err))?;
        // Bộ giải mã DXVA luôn tự cấp sample vì texture phải nằm trong mảng của
        // chính nó; nhánh còn lại chỉ để phòng bộ giải mã phần mềm.
        let provides = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        let supplied = if provides {
            None
        } else {
            let buffer = unsafe { MFCreateMemoryBuffer(info.cbSize.max(1)) }
                .map_err(|err| system("cấp buffer cho frame ra", err))?;
            let sample =
                unsafe { MFCreateSample() }.map_err(|err| system("cấp sample cho frame ra", err))?;
            unsafe { sample.AddBuffer(&buffer) }
                .map_err(|err| system("gắn buffer vào sample ra", err))?;
            Some(sample)
        };

        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: std::mem::ManuallyDrop::new(supplied),
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        let outcome = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };

        // Lấy quyền sở hữu ra khỏi `ManuallyDrop` dù thành công hay không — để
        // nguyên là rò một sample mỗi frame.
        let [buffer] = &mut buffers;
        let sample = unsafe { std::mem::ManuallyDrop::take(&mut buffer.pSample) };
        drop(unsafe { std::mem::ManuallyDrop::take(&mut buffer.pEvents) });

        match outcome {
            Ok(()) => match sample {
                Some(sample) => Ok(Pulled::Frame(sample)),
                None => Ok(Pulled::NeedInput),
            },
            Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(Pulled::NeedInput),
            Err(err) if err.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                self.output_ready = false;
                Ok(Pulled::StreamChange)
            }
            Err(err) => {
                self.stats.errors += 1;
                Err(system("lấy frame ra khỏi bộ giải mã", err))
            }
        }
    }

    /// Chốt kiểu đầu ra sau khi bộ giải mã đã đọc được tham số của dòng dữ liệu.
    fn negotiate_output(&mut self) -> Result<()> {
        let mut chosen = None;
        for index in 0.. {
            let Ok(media_type) = (unsafe { self.transform.GetOutputAvailableType(0, index) }) else {
                break;
            };
            if unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) }.ok() == Some(MFVideoFormat_NV12) {
                chosen = Some(media_type);
                break;
            }
        }
        let media_type = chosen.ok_or_else(|| {
            CodecError::Unsupported("bộ giải mã không chào định dạng NV12 nào".into())
        })?;

        // Đọc kích thước *trước* khi chốt: đây mới là kích thước thật của dòng
        // dữ liệu, còn con số truyền vào `new` chỉ là dự đoán.
        if let Ok(size) = unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) } {
            self.width = (size >> 32) as u32;
            self.height = size as u32;
        }

        unsafe { self.transform.SetOutputType(0, &media_type, 0) }
            .map_err(|err| system("chốt kiểu đầu ra của bộ giải mã", err))?;
        self.output_ready = true;
        Ok(())
    }

    fn wrap(&self, sample: IMFSample, pts_us: u64, started: Instant) -> Result<DecodedFrame> {
        let buffer = unsafe { sample.GetBufferByIndex(0) }
            .map_err(|err| system("lấy buffer của frame đã giải mã", err))?;
        let dxgi: IMFDXGIBuffer = buffer
            .cast()
            .map_err(|err| system("frame đã giải mã không nằm trên GPU", err))?;

        let mut raw = null_mut();
        unsafe { dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw) }
            .map_err(|err| system("lấy texture của frame đã giải mã", err))?;
        // An toàn: `GetResource` trả về một tham chiếu đã tăng đếm, và
        // `from_raw` nhận luôn quyền sở hữu tham chiếu đó.
        let texture = unsafe { ID3D11Texture2D::from_raw(raw) };
        let subresource = unsafe { dxgi.GetSubresourceIndex() }.unwrap_or(0);

        Ok(DecodedFrame {
            texture,
            subresource,
            width: self.width,
            height: self.height,
            format: FrameFormat::Nv12VideoRange,
            pts_us,
            decode_us: started.elapsed().as_micros() as u32,
            _sample: sample,
        })
    }
}

enum Pulled {
    Frame(IMFSample),
    NeedInput,
    StreamChange,
}

fn subtype(codec: Codec) -> Result<GUID> {
    match codec {
        Codec::H264 => Ok(MFVideoFormat_H264),
        Codec::Hevc => Ok(MFVideoFormat_HEVC),
        Codec::Av1 => Err(CodecError::Unsupported("AV1 chưa hỗ trợ".into())),
    }
}

fn set_input_type(
    transform: &IMFTransform,
    codec: Codec,
    width: u32,
    height: u32,
) -> Result<()> {
    let subtype = subtype(codec)?;
    let media_type = unsafe { windows::Win32::Media::MediaFoundation::MFCreateMediaType() }
        .map_err(|err| system("tạo kiểu dữ liệu vào", err))?;
    unsafe {
        media_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .and_then(|()| media_type.SetGUID(&MF_MT_SUBTYPE, &subtype))
            .and_then(|()| media_type.SetUINT64(&MF_MT_FRAME_SIZE, attribute_pair(width, height)))
    }
    .map_err(|err| system("mô tả kiểu dữ liệu vào", err))?;

    unsafe { transform.SetInputType(0, &media_type, 0) }
        .map_err(|err| system("đặt kiểu dữ liệu vào cho bộ giải mã", err))
}

/// Gói dữ liệu nén thành `IMFSample`.
///
/// Đây là lần copy CPU duy nhất của đường giải mã, và nó không tránh được: gói
/// tin vừa từ mạng vào nằm trong RAM thường, phải chép sang buffer của Media
/// Foundation thì bộ giải mã mới đọc được.
fn make_sample(data: &[u8], pts: i64) -> Result<IMFSample> {
    let buffer = unsafe { MFCreateMemoryBuffer(data.len().max(1) as u32) }
        .map_err(|err| system("cấp buffer cho dữ liệu nén", err))?;

    let mut ptr: *mut u8 = null_mut();
    unsafe { buffer.Lock(&mut ptr, None, None) }
        .map_err(|err| system("khoá buffer dữ liệu nén", err))?;
    if !ptr.is_null() {
        // An toàn: buffer vừa được cấp với đúng `data.len()` byte và đang bị
        // khoá, nên vùng nhớ đứng yên trong suốt phép chép.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len()) };
    }
    let _ = unsafe { buffer.Unlock() };
    unsafe { buffer.SetCurrentLength(data.len() as u32) }
        .map_err(|err| system("đặt độ dài buffer", err))?;

    let sample = unsafe { MFCreateSample() }.map_err(|err| system("tạo sample", err))?;
    unsafe {
        sample
            .AddBuffer(&buffer)
            .map_err(|err| system("gắn buffer vào sample", err))?;
        sample
            .SetSampleTime(pts)
            .map_err(|err| system("đặt timestamp cho sample", err))?;
    }
    Ok(sample)
}
