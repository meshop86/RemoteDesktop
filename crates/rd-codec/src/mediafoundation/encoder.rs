//! Bộ mã hoá phần cứng qua Media Foundation Transform (MFT).
//!
//! MFT phần cứng chạy bất đồng bộ theo kiểu "kéo": nó bắn sự kiện
//! `METransformNeedInput` khi sẵn sàng nhận frame và `METransformHaveOutput`
//! khi có dữ liệu ra. Ta không được gọi `ProcessInput` khi chưa được mời — gọi
//! bừa thì nhận `MF_E_NOTACCEPTING`.
//!
//! Vì thế có một luồng riêng ngồi chờ sự kiện. Nó không bao giờ chờ frame từ
//! phía gọi: [`MfEncoder::submit`] chỉ bỏ frame vào hàng đợi rồi tự thử đưa
//! vào MFT nếu đang có "vé" `NeedInput` chưa dùng. Hai bên gặp nhau ở đúng một
//! mutex, nên không bên nào chặn bên nào.

use std::collections::{HashMap, VecDeque};
use std::mem::ManuallyDrop;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI,
    IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFMediaType, IMFSample, IMFTransform,
    MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS, METransformDrainComplete, METransformHaveOutput,
    METransformNeedInput, MF_EVENT_TYPE, MF_MT_ALL_SAMPLES_INDEPENDENT, MF_MT_AVG_BITRATE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC_UNLOCK,
    MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFMediaType_Video, MFSampleExtension_CleanPoint, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_MESSAGE_COMMAND_DRAIN,
    MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MFVideoFormat_H264,
    MFVideoFormat_HEVC, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_High,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, Interface};

use crate::annexb::{NaluClass, classify_h264, classify_hevc, iter_nalus, push_nalu};
use crate::mf_params::{attribute_pair, hundred_ns};
use crate::{
    ChromaSubsampling, Codec, CodecError, EncodedFrame, EncoderConfig, EncoderStats, Result,
};

use super::convert::Nv12Converter;
use super::{ensure_started, system};

/// Giá trị `MF_MT_PIXEL_ASPECT_RATIO` cho điểm ảnh vuông. Màn hình máy tính
/// luôn vuông; không khai thì vài bộ mã hoá tự suy ra tỉ lệ của TV analog.
const SQUARE_PIXELS: u64 = attribute_pair(1, 1);

/// Số frame tối đa được nằm chờ vé `NeedInput`. Đủ để lấp ống dẫn của MFT phần
/// cứng, không đủ để một MFT đã kẹt ăn hết VRAM.
const MAX_PENDING: usize = 4;

#[derive(Default)]
struct Counters {
    submitted: AtomicU64,
    emitted: AtomicU64,
    dropped: AtomicU64,
    bytes: AtomicU64,
    errors: AtomicU64,
}

/// Một frame NV12 đang chờ tới lượt vào MFT.
struct Job {
    texture: ID3D11Texture2D,
    pts: i64,
    force_key: bool,
}

/// Phần trạng thái mà luồng gọi và luồng sự kiện cùng chạm vào.
struct Pipeline {
    transform: IMFTransform,
    codec_api: Option<ICodecAPI>,
    codec: Codec,
    /// Số lời mời `NeedInput` chưa dùng. MFT phần cứng thường mời trước vài
    /// frame để lấp ống dẫn của nó.
    credits: u32,
    pending: VecDeque<Job>,
    /// pts (đơn vị 100ns) → lúc nộp, để tính `encode_us`. MFT giữ nguyên pts
    /// từ vào tới ra nên đây là cách duy nhất ghép được hai đầu.
    submitted_at: HashMap<i64, Instant>,
    /// SPS/PPS/VPS lấy từ kiểu đầu ra, chèn lại vào keyframe nếu bộ mã hoá
    /// không tự chèn. Viewer vào giữa chừng mà thiếu chúng là không giải mã
    /// được frame nào.
    parameter_sets: Vec<u8>,
    frame_duration: i64,
}

// An toàn: mọi truy cập đều qua `Mutex`, và các con trỏ COM ở đây đều là
// free-threaded (MFT phần cứng bắt buộc phải vậy để chạy được kiểu bất đồng bộ).
unsafe impl Send for Pipeline {}

struct Shared {
    pipeline: Mutex<Pipeline>,
    tx: Mutex<Sender<EncodedFrame>>,
    counters: Counters,
    stopping: AtomicBool,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Pipeline> {
        self.pipeline
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub struct MfEncoder {
    shared: Arc<Shared>,
    rx: Receiver<EncodedFrame>,
    pump: Option<std::thread::JoinHandle<()>>,
    converter: Nv12Converter,
    /// Giữ cho device manager sống bằng đúng tuổi thọ của MFT — thả sớm là MFT
    /// mất đường vào GPU giữa chừng.
    _device_manager: IMFDXGIDeviceManager,
    config: EncoderConfig,
    force_keyframe: bool,
}

// An toàn: mọi con trỏ COM bên trong đều free-threaded, và mọi hàm chạm tới
// chúng đều nhận `&mut self` nên hai luồng không thể cùng gọi. Luồng bơm sự
// kiện dùng `Arc<Shared>` riêng, đã có `Mutex` bảo vệ.
unsafe impl Send for MfEncoder {}

impl MfEncoder {
    /// Tạo bộ mã hoá trên **đúng** device D3D11 đã sinh ra texture màn hình.
    ///
    /// Không nhận device khác được: hai device D3D11 không dùng chung tài
    /// nguyên nếu tài nguyên không khai báo shared, và khai báo shared thì mất
    /// luôn cái lợi zero-copy.
    pub fn new(config: EncoderConfig, device: &ID3D11Device) -> Result<Self> {
        ensure_started()?;

        let converter = Nv12Converter::new(device, config.width, config.height, config.target_fps)?;
        let transform = find_hardware_encoder(config.codec)?;

        // Phải mở khoá trước mọi thứ khác: MFT phần cứng mặc định từ chối làm
        // việc với ứng dụng không phải Media Session.
        let attributes = unsafe { transform.GetAttributes() }
            .map_err(|err| system("đọc thuộc tính của MFT", err))?;
        unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
            .map_err(|err| system("mở khoá MFT bất đồng bộ", err))?;

        let device_manager = super::attach_device(&transform, device)?;

        let codec_api: Option<ICodecAPI> = transform.cast().ok();
        if let Some(api) = &codec_api {
            configure_codec_api(api, &config);
        }

        set_output_type(&transform, &config)?;
        set_input_type(&transform, &config)?;

        let parameter_sets = read_parameter_sets(&transform);

        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|err| system("báo MFT bắt đầu luồng", err))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|err| system("báo MFT bắt đầu dòng dữ liệu", err))?;
        }

        let frame_duration = hundred_ns(1_000_000 / config.target_fps.max(1) as u64);
        let (tx, rx) = channel();
        let shared = Arc::new(Shared {
            pipeline: Mutex::new(Pipeline {
                transform,
                codec_api,
                codec: config.codec,
                credits: 0,
                pending: VecDeque::new(),
                submitted_at: HashMap::new(),
                parameter_sets,
                frame_duration,
            }),
            tx: Mutex::new(tx),
            counters: Counters::default(),
            stopping: AtomicBool::new(false),
        });

        let pump = spawn_pump(Arc::clone(&shared))?;

        tracing::info!(
            width = config.width,
            height = config.height,
            codec = ?config.codec,
            "đã tạo bộ mã hoá Media Foundation"
        );

        Ok(Self {
            shared,
            rx,
            pump: Some(pump),
            converter,
            _device_manager: device_manager,
            config,
            force_keyframe: false,
        })
    }

    /// Luôn là 4:2:0: đường đi Media Foundation không có lựa chọn nào khác.
    pub fn actual_chroma(&self) -> ChromaSubsampling {
        ChromaSubsampling::Yuv420
    }

    /// Luôn `true` — hàm [`new`](Self::new) từ chối khởi tạo nếu không tìm được
    /// bộ mã hoá phần cứng.
    pub fn is_hardware(&self) -> bool {
        true
    }

    pub fn config(&self) -> &EncoderConfig {
        &self.config
    }

    /// Yêu cầu frame kế tiếp là keyframe. Gọi khi viewer vừa kết nối hoặc báo
    /// mất dữ liệu — đây là cách duy nhất để hình ảnh hồi phục sau mất gói.
    pub fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    pub fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        let pipeline = self.shared.lock();
        let Some(api) = &pipeline.codec_api else {
            return Err(CodecError::Unsupported(
                "MFT này không cho đổi bitrate giữa chừng".into(),
            ));
        };
        let value = VARIANT::from(kbps.saturating_mul(1000));
        unsafe { api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &value) }
            .map_err(|err| system("đổi bitrate", err))
    }

    /// Đưa một frame màn hình (BGRA) vào bộ mã hoá.
    ///
    /// Phép chuyển sang NV12 chạy ngay trên luồng gọi vì nó chỉ là một lệnh
    /// GPU; phần chờ đợi nằm hết ở luồng sự kiện.
    pub fn submit(&mut self, surface: &ID3D11Texture2D, pts_us: u64) -> Result<()> {
        let nv12 = self.converter.convert(surface)?;
        let job = Job {
            texture: nv12,
            pts: hundred_ns(pts_us),
            force_key: std::mem::take(&mut self.force_keyframe),
        };

        let mut pipeline = self.shared.lock();
        pipeline.submitted_at.insert(job.pts, Instant::now());
        pipeline.pending.push_back(job);
        // MFT không mời nhận nữa (driver kẹt, phiên đăng nhập bị khoá) thì hàng
        // chờ này phình ra vô hạn, mà mỗi chỗ trong hàng là một texture trong
        // VRAM. Bỏ frame cũ nhất: với hình trực tiếp thì frame cũ vốn đã hết giá
        // trị, giữ lại chỉ để hết bộ nhớ.
        while pipeline.pending.len() > MAX_PENDING {
            if let Some(stale) = pipeline.pending.pop_front() {
                pipeline.submitted_at.remove(&stale.pts);
                self.shared.counters.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.shared.counters.submitted.fetch_add(1, Ordering::Relaxed);
        pipeline.feed(&self.shared.counters);
        Ok(())
    }

    pub fn try_next_frame(&mut self) -> Option<EncodedFrame> {
        self.rx.try_recv().ok()
    }

    pub fn next_frame(&mut self, timeout: Duration) -> Result<EncodedFrame> {
        self.rx.recv_timeout(timeout).map_err(|err| match err {
            std::sync::mpsc::RecvTimeoutError::Timeout => CodecError::Timeout,
            std::sync::mpsc::RecvTimeoutError::Disconnected => CodecError::Closed,
        })
    }

    /// Bảo MFT đẩy nốt mọi frame còn trong ống dẫn ra ngoài.
    pub fn flush(&mut self) -> Result<()> {
        let pipeline = self.shared.lock();
        unsafe {
            pipeline
                .transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)
        }
        .map_err(|err| system("đẩy nốt frame còn lại", err))
    }

    pub fn stats(&self) -> EncoderStats {
        let counters = &self.shared.counters;
        EncoderStats {
            frames_submitted: counters.submitted.load(Ordering::Relaxed),
            frames_emitted: counters.emitted.load(Ordering::Relaxed),
            frames_dropped: counters.dropped.load(Ordering::Relaxed),
            bytes_emitted: counters.bytes.load(Ordering::Relaxed),
            errors: counters.errors.load(Ordering::Relaxed),
        }
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        self.shared.stopping.store(true, Ordering::Release);
        {
            let pipeline = self.shared.lock();
            unsafe {
                // Thứ tự này quan trọng: `FLUSH` vứt frame đang dở và làm mọi
                // lời gọi đang treo trả về, `END_STREAMING` mới khiến hàng đợi
                // sự kiện đóng lại để luồng pump thoát.
                let _ = pipeline.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
                let _ = pipeline
                    .transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
                let _ = pipeline
                    .transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            }
        }
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
    }
}

impl Pipeline {
    /// Đưa frame vào MFT chừng nào còn vé mời và còn frame chờ.
    fn feed(&mut self, counters: &Counters) {
        while self.credits > 0 {
            let Some(job) = self.pending.pop_front() else {
                return;
            };
            self.credits -= 1;

            if job.force_key && let Some(api) = &self.codec_api {
                let value = VARIANT::from(1u32);
                if let Err(err) = unsafe { api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &value) }
                {
                    tracing::warn!(%err, "không ép được keyframe, dùng frame thường");
                }
            }

            match self.wrap(&job) {
                Ok(sample) => {
                    if let Err(err) = unsafe { self.transform.ProcessInput(0, &sample, 0) } {
                        tracing::warn!(%err, "MFT từ chối frame");
                        self.submitted_at.remove(&job.pts);
                        counters.dropped.fetch_add(1, Ordering::Relaxed);
                        counters.errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(err) => {
                    tracing::warn!(%err, "không gói được frame thành sample");
                    self.submitted_at.remove(&job.pts);
                    counters.dropped.fetch_add(1, Ordering::Relaxed);
                    counters.errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// Bọc texture NV12 thành `IMFSample` mà không chép pixel.
    fn wrap(&self, job: &Job) -> Result<IMFSample> {
        let buffer = unsafe {
            MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &job.texture, 0, false)
        }
        .map_err(|err| system("bọc texture thành media buffer", err))?;

        // Buffer sinh từ texture có `CurrentLength` bằng 0; MFT bỏ qua sample
        // rỗng nên phải đặt bằng đúng kích thước liên tục của ảnh.
        if let Ok(two_d) = buffer.cast::<windows::Win32::Media::MediaFoundation::IMF2DBuffer>()
            && let Ok(length) = unsafe { two_d.GetContiguousLength() }
        {
            let _ = unsafe { buffer.SetCurrentLength(length) };
        }

        let sample =
            unsafe { MFCreateSample() }.map_err(|err| system("tạo sample", err))?;
        unsafe {
            sample
                .AddBuffer(&buffer)
                .map_err(|err| system("gắn buffer vào sample", err))?;
            sample
                .SetSampleTime(job.pts)
                .map_err(|err| system("đặt timestamp cho sample", err))?;
            let _ = sample.SetSampleDuration(self.frame_duration);
        }
        Ok(sample)
    }

    /// Lấy một frame đã nén ra khỏi MFT. Gọi đúng một lần cho mỗi sự kiện
    /// `HaveOutput`.
    fn drain_one(&mut self, counters: &Counters, tx: &Mutex<Sender<EncodedFrame>>) {
        let sample = match self.process_output() {
            Ok(Some(sample)) => sample,
            Ok(None) => return,
            Err(err) => {
                tracing::warn!(%err, "lấy frame đã nén thất bại");
                counters.errors.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };

        let pts = unsafe { sample.GetSampleTime() }.unwrap_or(0);
        let keyframe = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.unwrap_or(0) == 1;
        let data = match read_sample(&sample) {
            Ok(data) => data,
            Err(err) => {
                tracing::warn!(%err, "đọc dữ liệu nén thất bại");
                counters.errors.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        let data = self.with_parameter_sets(data, keyframe);

        let encode_us = self
            .submitted_at
            .remove(&pts)
            .map(|started| started.elapsed().as_micros() as u32)
            .unwrap_or(0);
        // pts của frame bị bỏ giữa chừng không bao giờ được lấy ra; dọn để bản
        // đồ không phình theo thời gian chạy.
        if self.submitted_at.len() > 256 {
            self.submitted_at.clear();
        }

        counters.emitted.fetch_add(1, Ordering::Relaxed);
        counters
            .bytes
            .fetch_add(data.len() as u64, Ordering::Relaxed);

        let frame = EncodedFrame {
            data,
            keyframe,
            // Đổi ngược về micro giây: pts đi xuyên suốt hệ thống ở đơn vị đó.
            pts_us: (pts.max(0) / 10) as u64,
            encode_us,
        };
        if tx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .send(frame)
            .is_err()
        {
            counters.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn process_output(&self) -> Result<Option<IMFSample>> {
        let info = unsafe { self.transform.GetOutputStreamInfo(0) }
            .map_err(|err| system("hỏi thông tin luồng ra", err))?;

        // MFT phần cứng thường tự cấp sample; loại tự cấp thì ta phải đưa
        // sample rỗng vào cho nó ghi.
        let provides = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        let supplied = if provides {
            None
        } else {
            let buffer = unsafe { MFCreateMemoryBuffer(info.cbSize.max(1)) }
                .map_err(|err| system("cấp buffer cho dữ liệu nén", err))?;
            let sample = unsafe { MFCreateSample() }
                .map_err(|err| system("cấp sample cho dữ liệu nén", err))?;
            unsafe { sample.AddBuffer(&buffer) }
                .map_err(|err| system("gắn buffer vào sample ra", err))?;
            Some(sample)
        };

        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(supplied),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        let outcome =
            unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };

        // Lấy quyền sở hữu ra khỏi `ManuallyDrop` dù thành công hay không —
        // để nguyên là rò một sample mỗi frame.
        let [buffer] = &mut buffers;
        let sample = unsafe { ManuallyDrop::take(&mut buffer.pSample) };
        drop(unsafe { ManuallyDrop::take(&mut buffer.pEvents) });

        outcome.map_err(|err| system("lấy dữ liệu nén ra khỏi MFT", err))?;
        Ok(sample)
    }

    /// Chèn parameter set vào đầu keyframe nếu bộ mã hoá không tự chèn.
    fn with_parameter_sets(&self, data: Vec<u8>, keyframe: bool) -> Vec<u8> {
        if !keyframe || self.parameter_sets.is_empty() {
            return data;
        }
        let classify: fn(&[u8]) -> NaluClass = match self.codec {
            Codec::H264 => classify_h264,
            Codec::Hevc => classify_hevc,
            // AV1 không dùng NAL Annex B; không có gì để dò và cũng không có gì
            // để chèn.
            Codec::Av1 => return data,
        };
        let has_sets = iter_nalus(&data).any(|nalu| classify(nalu) == NaluClass::ParameterSet);
        if has_sets {
            return data;
        }

        let mut out = Vec::with_capacity(self.parameter_sets.len() + data.len());
        out.extend_from_slice(&self.parameter_sets);
        out.extend_from_slice(&data);
        out
    }
}

/// Vòng chờ sự kiện của MFT, chạy trên luồng riêng.
fn spawn_pump(shared: Arc<Shared>) -> Result<std::thread::JoinHandle<()>> {
    let generator: IMFMediaEventGenerator = shared
        .lock()
        .transform
        .cast()
        .map_err(|err| system("lấy nguồn sự kiện của MFT", err))?;

    // Con trỏ COM free-threaded nhưng kiểu của nó không tự khai `Send`.
    struct Handoff(IMFMediaEventGenerator);
    unsafe impl Send for Handoff {}
    impl Handoff {
        /// Phải là *method* chứ không đọc thẳng `handoff.0` trong closure:
        /// closure Rust 2021 bắt riêng từng trường, nên đọc trường là bắt thẳng
        /// con trỏ COM và cái vỏ `Send` này bị bỏ qua.
        fn into_inner(self) -> IMFMediaEventGenerator {
            self.0
        }
    }
    let handoff = Handoff(generator);

    std::thread::Builder::new()
        .name("mf-encoder-pump".into())
        .spawn(move || {
            // Media Foundation đòi luồng nằm trong apartment đa luồng.
            let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            let generator = handoff.into_inner();

            while !shared.stopping.load(Ordering::Acquire) {
                let event = match unsafe { generator.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)) } {
                    Ok(event) => event,
                    // Hàng đợi đóng là cách bình thường để vòng lặp này kết thúc.
                    Err(_) => break,
                };
                let Ok(kind) = (unsafe { event.GetType() }) else {
                    continue;
                };

                let kind = MF_EVENT_TYPE(kind as i32);
                if kind == METransformNeedInput {
                    let mut pipeline = shared.lock();
                    pipeline.credits += 1;
                    pipeline.feed(&shared.counters);
                } else if kind == METransformHaveOutput {
                    let mut pipeline = shared.lock();
                    pipeline.drain_one(&shared.counters, &shared.tx);
                } else if kind == METransformDrainComplete {
                    tracing::debug!("MFT đã đẩy hết frame còn lại");
                }
            }

            if com.is_ok() {
                unsafe { CoUninitialize() };
            }
        })
        .map_err(|err| CodecError::System {
            what: "tạo luồng chờ sự kiện",
            detail: err.to_string(),
        })
}

/// Tìm bộ mã hoá phần cứng cho codec yêu cầu.
fn find_hardware_encoder(codec: Codec) -> Result<IMFTransform> {
    let info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype(codec)?,
    };
    super::activate_first(
        MFT_CATEGORY_VIDEO_ENCODER,
        // `SORTANDFILTER` đưa bộ mã hoá mà nhà sản xuất GPU ưu tiên lên đầu.
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
        None,
        Some(&info),
        &format!("bộ mã hoá {codec:?} chạy bằng phần cứng"),
    )
}

fn subtype(codec: Codec) -> Result<GUID> {
    match codec {
        Codec::H264 => Ok(MFVideoFormat_H264),
        Codec::Hevc => Ok(MFVideoFormat_HEVC),
        // AV1 nén tốt hơn HEVC nhưng phần cứng mã hoá nó mới chỉ có trên đời
        // GPU gần nhất, và cả đường giải mã bên kia cũng chưa có. Mở AV1 là
        // việc của cả hai đầu, không riêng file này.
        Codec::Av1 => Err(CodecError::Unsupported("AV1 chưa hỗ trợ".into())),
    }
}

fn configure_codec_api(api: &ICodecAPI, config: &EncoderConfig) {
    // Tắt mọi thứ khiến bộ mã hoá gom frame lại xử lý theo lô. Đây là thiết
    // lập quan trọng nhất của cả file: bật lên là mất vài chục ms độ trễ.
    set_codec_value(api, &CODECAPI_AVLowLatencyMode, VARIANT::from(true), "chế độ độ trễ thấp");
    set_codec_value(
        api,
        &CODECAPI_AVEncCommonRateControlMode,
        VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
        "chế độ điều tiết bitrate",
    );
    set_codec_value(
        api,
        &CODECAPI_AVEncCommonMeanBitRate,
        VARIANT::from(config.target_bitrate_kbps.saturating_mul(1000)),
        "bitrate trung bình",
    );
    // Keyframe chủ yếu phát theo yêu cầu của viewer, nên khoảng cách định kỳ
    // đặt thưa; 0 nghĩa là "chỉ keyframe đầu tiên", quá liều nếu mất gói.
    set_codec_value(
        api,
        &CODECAPI_AVEncMPVGOPSize,
        VARIANT::from(
            config
                .keyframe_interval_secs
                .saturating_mul(config.target_fps.max(1)),
        ),
        "khoảng cách keyframe",
    );
}

fn set_codec_value(api: &ICodecAPI, key: &GUID, value: VARIANT, what: &str) {
    if let Err(err) = unsafe { api.SetValue(key, &value) } {
        // Không phải MFT nào cũng nhận đủ các khoá này. Thiếu một khoá thì
        // chất lượng kém đi chứ không hỏng, nên chỉ ghi log.
        tracing::debug!(%err, what, "MFT không nhận thiết lập này");
    }
}

fn set_output_type(transform: &IMFTransform, config: &EncoderConfig) -> Result<()> {
    let subtype = subtype(config.codec)?;
    let media_type = unsafe { MFCreateMediaType() }
        .map_err(|err| system("tạo kiểu dữ liệu ra", err))?;
    unsafe {
        media_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .and_then(|()| media_type.SetGUID(&MF_MT_SUBTYPE, &subtype))
            .and_then(|()| {
                media_type.SetUINT32(
                    &MF_MT_AVG_BITRATE,
                    config.target_bitrate_kbps.saturating_mul(1000),
                )
            })
            .and_then(|()| {
                media_type.SetUINT64(
                    &MF_MT_FRAME_SIZE,
                    attribute_pair(config.width, config.height),
                )
            })
            .and_then(|()| {
                media_type.SetUINT64(
                    &MF_MT_FRAME_RATE,
                    attribute_pair(config.target_fps.max(1), 1),
                )
            })
            .and_then(|()| media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, SQUARE_PIXELS))
            .and_then(|()| {
                media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            })
    }
    .map_err(|err| system("mô tả kiểu dữ liệu ra", err))?;

    if config.codec == Codec::H264 {
        // Profile High cho nén tốt hơn hẳn Baseline ở cùng bitrate và mọi phần
        // cứng còn dùng được đều hỗ trợ.
        let _ = unsafe {
            media_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)
        };
    }

    unsafe { transform.SetOutputType(0, &media_type, 0) }
        .map_err(|err| system("đặt kiểu dữ liệu ra", err))
}

fn set_input_type(transform: &IMFTransform, config: &EncoderConfig) -> Result<()> {
    let media_type = unsafe { MFCreateMediaType() }
        .map_err(|err| system("tạo kiểu dữ liệu vào", err))?;
    unsafe {
        media_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .and_then(|()| media_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12))
            .and_then(|()| {
                media_type.SetUINT64(
                    &MF_MT_FRAME_SIZE,
                    attribute_pair(config.width, config.height),
                )
            })
            .and_then(|()| {
                media_type.SetUINT64(
                    &MF_MT_FRAME_RATE,
                    attribute_pair(config.target_fps.max(1), 1),
                )
            })
            .and_then(|()| media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, SQUARE_PIXELS))
            .and_then(|()| {
                media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            })
            .and_then(|()| media_type.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1))
    }
    .map_err(|err| system("mô tả kiểu dữ liệu vào", err))?;

    unsafe { transform.SetInputType(0, &media_type, 0) }
        .map_err(|err| system("đặt kiểu dữ liệu vào", err))
}

/// Đọc SPS/PPS mà bộ mã hoá gắn kèm kiểu đầu ra, nếu có.
fn read_parameter_sets(transform: &IMFTransform) -> Vec<u8> {
    let Ok(media_type) = (unsafe { transform.GetOutputCurrentType(0) }) else {
        return Vec::new();
    };
    sequence_header(&media_type)
}

fn sequence_header(media_type: &IMFMediaType) -> Vec<u8> {
    use windows::Win32::Media::MediaFoundation::MF_MT_MPEG_SEQUENCE_HEADER;

    let Ok(size) = (unsafe { media_type.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) }) else {
        return Vec::new();
    };
    if size == 0 {
        return Vec::new();
    }
    let mut blob = vec![0u8; size as usize];
    if unsafe { media_type.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None) }.is_err() {
        return Vec::new();
    }

    // Blob đã ở dạng Annex B sẵn trên mọi bộ mã hoá gặp thực tế, nhưng chuẩn
    // không bắt buộc. Không thấy start code thì tự bọc lại.
    if blob.starts_with(&[0, 0, 1]) || blob.starts_with(&[0, 0, 0, 1]) {
        blob
    } else {
        let mut out = Vec::with_capacity(blob.len() + 4);
        push_nalu(&mut out, &blob);
        out
    }
}

/// Chép dữ liệu nén ra `Vec`. Đây là lần duy nhất pixel-đã-nén đi qua CPU, và
/// nó bắt buộc: gói tin phải nằm trong bộ nhớ thường mới gửi qua mạng được.
fn read_sample(sample: &IMFSample) -> Result<Vec<u8>> {
    let buffer = unsafe { sample.ConvertToContiguousBuffer() }
        .map_err(|err| system("gộp buffer của sample", err))?;

    let mut ptr: *mut u8 = null_mut();
    let mut length = 0u32;
    unsafe { buffer.Lock(&mut ptr, None, Some(&mut length)) }
        .map_err(|err| system("khoá buffer dữ liệu nén", err))?;

    // An toàn: buffer đang bị khoá nên vùng nhớ đứng yên trong suốt phép chép.
    let data = if ptr.is_null() {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(ptr, length as usize) }.to_vec()
    };
    let _ = unsafe { buffer.Unlock() };
    Ok(data)
}
