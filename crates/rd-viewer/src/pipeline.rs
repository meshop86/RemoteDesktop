//! Chuỗi chụp → mã hoá → giải mã, tách làm hai nửa dùng lại được.
//!
//! [`EncodeSource`] là nửa của máy host, [`DecodeSink`] là nửa của máy viewer.
//! Chạy trên một máy thì [`Pipeline`] nối thẳng hai nửa vào nhau — đó là cách
//! đo phần độ trễ *không* liên quan tới mạng. Chạy hai máy thì phần mạng chen
//! vào đúng khe giữa chúng, và không nửa nào phải đổi.
//!
//! Cách chia này còn giữ được một thứ khó thay thế: mọi đoạn code chỉ có trên
//! Windows (mượn device D3D11 của capture cho bộ mã hoá, tự mở device riêng cho
//! bộ giải mã) nằm gọn trong crate này, nên `cargo clippy --target
//! x86_64-pc-windows-msvc -p rd-viewer` vẫn kiểm được chúng từ máy macOS.
//!
//! Tách khỏi luồng giao diện là bắt buộc: mã hoá một frame 1080p mất 5-9 ms, đủ
//! làm giật giao diện nếu chạy chung. [`Pipeline`] đẩy frame đã giải mã qua một
//! kênh một chỗ, luồng giao diện chỉ lấy frame *mới nhất* và bỏ hết frame cũ —
//! với điều khiển từ xa, hình cũ không còn giá trị.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rd_capture::{CaptureConfig, ScreenCapturer};
use rd_codec::{
    ChromaSubsampling, Codec, DecoderStats, EncodedFrame, EncoderConfig, EncoderStats,
    PlatformDecoder, PlatformEncoder,
};

#[cfg(target_os = "macos")]
use rd_codec::videotoolbox::DecodedFrame;
#[cfg(target_os = "windows")]
use rd_codec::mediafoundation::DecodedFrame;

/// Một frame đã đi hết chặng, kèm số liệu của từng chặng.
pub struct PipelineFrame {
    pub frame: Arc<DecodedFrame>,
    /// Kích thước gói đã nén — chính là số byte đã phải đẩy qua mạng.
    pub bytes: usize,
    pub keyframe: bool,
    pub encode_us: u32,
    pub decode_us: u32,
    /// Từ lúc frame rời khỏi màn hình đến lúc giải mã xong. Cộng thêm thời gian
    /// vẽ sẽ ra độ trễ người dùng thực sự cảm nhận được.
    ///
    /// Chạy hai máy thì con số này chỉ đúng khi hai đồng hồ đã đồng bộ (xem
    /// [`rd_transport::ClockSync`]); lệch đồng hồ bao nhiêu thì lệch vào đây
    /// bấy nhiêu.
    pub pipeline_us: u32,
}

/// Thông tin cố định của phiên, để hiện lên HUD.
#[derive(Debug, Clone)]
pub struct PipelineInfo {
    pub source: String,
    pub width: u32,
    pub height: u32,
    pub codec: Codec,
    pub chroma: ChromaSubsampling,
    pub hardware: bool,
    pub target_fps: u32,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct PipelineCounters {
    pub captured: u64,
    pub encoded: u64,
    pub decoded: u64,
    /// Frame bị bỏ vì giao diện chưa vẽ kịp frame trước.
    pub dropped_late: u64,
    pub errors: u64,
}

// ─────────────────────────── nửa của máy host ───────────────────────────

/// Nguồn frame đã nén: chụp màn hình rồi mã hoá ngay, không qua CPU.
pub struct EncodeSource {
    source: Source,
    encoder: PlatformEncoder,
    info: PipelineInfo,
    /// Frame đầu tiên đã phải chụp ngay lúc dựng (để biết phân giải, và trên
    /// Windows là để mượn device D3D11), nên nó chờ ở đây cho lượt lấy đầu.
    pending: Option<rd_capture::CapturedFrame>,
    /// Giữ device của capture để bộ giải mã cùng máy dùng lại được — trộn hai
    /// device là phải chép texture qua RAM ở giữa.
    #[cfg(target_os = "windows")]
    device: windows::Win32::Graphics::Direct3D11::ID3D11Device,
}

impl EncodeSource {
    /// `allow_10bit` do phần cứng đồ hoạ của **người xem** quyết định: thiếu
    /// texture 16-bit chuẩn hoá thì không hiển thị được 4:2:2 10-bit, nên phải
    /// lùi về 4:2:0 8-bit ngay từ khâu mã hoá thay vì để hỏng ở khâu vẽ.
    pub fn start(target_fps: u32, bitrate_kbps: u32, allow_10bit: bool) -> anyhow::Result<Self> {
        let capture_config = CaptureConfig {
            target_fps,
            ..Default::default()
        };
        let (mut source, source_name) = open_source(capture_config)?;
        let first = source.next_frame(Duration::from_secs(5))?;
        let (width, height) = (first.width, first.height);

        let config = EncoderConfig {
            width,
            height,
            codec: Codec::Hevc,
            chroma: if allow_10bit {
                ChromaSubsampling::Yuv422
            } else {
                ChromaSubsampling::Yuv420
            },
            target_bitrate_kbps: bitrate_kbps,
            target_fps,
            keyframe_interval_secs: 10,
        };

        #[cfg(target_os = "windows")]
        let device = first.surface.device().clone();
        #[cfg(target_os = "macos")]
        let encoder = PlatformEncoder::new(config)?;
        #[cfg(target_os = "windows")]
        let encoder = PlatformEncoder::new(config, &device)?;

        let info = PipelineInfo {
            source: source_name,
            width,
            height,
            codec: Codec::Hevc,
            chroma: encoder.actual_chroma(),
            hardware: encoder.is_hardware(),
            target_fps,
        };

        Ok(Self {
            source,
            encoder,
            info,
            pending: Some(first),
            #[cfg(target_os = "windows")]
            device,
        })
    }

    pub fn info(&self) -> &PipelineInfo {
        &self.info
    }

    pub fn stats(&self) -> EncoderStats {
        self.encoder.stats()
    }

    /// Phát một keyframe ở frame kế tiếp. Viewer xin cái này khi vừa vào phiên
    /// hoặc khi mất gói tới mức hình vỡ.
    pub fn request_keyframe(&mut self) {
        self.encoder.request_keyframe();
    }

    pub fn set_bitrate(&mut self, kbps: u32) -> rd_codec::Result<()> {
        self.encoder.set_bitrate(kbps)
    }

    /// Chụp một frame rồi mã hoá.
    ///
    /// `Ok(None)` nghĩa là trong khoảng chờ không có frame mới — màn hình đứng
    /// yên, chuyện bình thường và không tốn gì. Chỉ `Err` mới là hỏng.
    pub fn next_encoded(&mut self, timeout: Duration) -> anyhow::Result<Option<EncodedFrame>> {
        let captured = match self.pending.take() {
            Some(frame) => frame,
            None => match self.source.next_frame(timeout) {
                Ok(frame) => frame,
                Err(rd_capture::CaptureError::Timeout) => return Ok(None),
                Err(err) => return Err(err.into()),
            },
        };
        submit(&mut self.encoder, &captured)?;
        Ok(Some(self.encoder.next_frame(Duration::from_millis(500))?))
    }

    pub fn stop(&mut self) {
        self.source.stop();
    }
}

/// Đưa frame vừa chụp vào bộ mã hoá.
///
/// Dùng thẳng thời điểm chụp làm timestamp: nó đi xuyên qua bộ mã hoá và quay
/// ra ở bộ giải mã, nên trừ đi là được độ trễ của cả chuỗi.
#[cfg(target_os = "macos")]
fn submit(
    encoder: &mut PlatformEncoder,
    captured: &rd_capture::CapturedFrame,
) -> rd_codec::Result<()> {
    encoder.submit(captured.surface.as_pixel_buffer(), captured.capture_us)
}

#[cfg(target_os = "windows")]
fn submit(
    encoder: &mut PlatformEncoder,
    captured: &rd_capture::CapturedFrame,
) -> rd_codec::Result<()> {
    encoder.submit(captured.surface.texture(), captured.capture_us)
}

// ────────────────────────── nửa của máy viewer ──────────────────────────

/// Bộ giải mã cộng phần dựng frame để giao diện vẽ.
pub struct DecodeSink {
    decoder: PlatformDecoder,
    /// Chỉ để giữ device sống bằng tuổi thọ bộ giải mã; không đọc tới.
    #[cfg(target_os = "windows")]
    _device: windows::Win32::Graphics::Direct3D11::ID3D11Device,
}

impl DecodeSink {
    /// Nhận cả `width`/`height` lẫn `chroma` để phía gọi không phải rẽ nhánh
    /// theo hệ điều hành, dù mỗi nền tảng chỉ dùng một nửa: Media Foundation
    /// phải biết kích thước trước khi dựng, còn VideoToolbox thì suy ra kích
    /// thước từ dòng dữ liệu nhưng cần biết mức lấy mẫu màu để khỏi phải chuyển
    /// đổi thừa một lần trên mỗi frame.
    #[cfg_attr(target_os = "windows", allow(unused_variables))]
    #[cfg_attr(target_os = "macos", allow(unused_variables))]
    pub fn new(
        codec: Codec,
        width: u32,
        height: u32,
        chroma: ChromaSubsampling,
    ) -> anyhow::Result<Self> {
        #[cfg(target_os = "macos")]
        {
            Ok(Self {
                decoder: PlatformDecoder::new(codec, chroma)?,
            })
        }
        #[cfg(target_os = "windows")]
        {
            // Máy chỉ làm viewer thì không có capture để mượn device, phải tự
            // mở một cái.
            let device = rd_codec::mediafoundation::create_device()?;
            Ok(Self {
                decoder: PlatformDecoder::new(codec, width, height, &device)?,
                _device: device,
            })
        }
    }

    /// Bản dùng lại device có sẵn — chỉ có khi host và viewer là cùng một máy.
    #[cfg(target_os = "windows")]
    pub fn with_device(
        codec: Codec,
        width: u32,
        height: u32,
        device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            decoder: PlatformDecoder::new(codec, width, height, device)?,
            _device: device.clone(),
        })
    }

    pub fn stats(&self) -> DecoderStats {
        self.decoder.stats()
    }

    /// Bộ giải mã có từ chối dữ liệu kể từ lần hỏi trước không (và xoá cờ).
    /// `true` nghĩa là chuỗi dự đoán đã đứt vì mất frame: phải xin keyframe,
    /// chờ suông thì hình đứng yên tới hết chu kỳ keyframe.
    pub fn take_bad_data(&mut self) -> bool {
        self.decoder.take_bad_data()
    }

    /// Giải mã một access unit.
    ///
    /// `Ok(None)` là bình thường: dữ liệu đến trước keyframe đầu tiên thì chưa
    /// có parameter set để dựng bộ giải mã, chờ keyframe kế tiếp là xong.
    pub fn decode(
        &mut self,
        data: &[u8],
        pts_us: u64,
        encode_us: u32,
        keyframe: bool,
    ) -> anyhow::Result<Option<PipelineFrame>> {
        let Some(decoded) = self.decoder.decode(data, pts_us)? else {
            return Ok(None);
        };
        let now_us = now_us();
        Ok(Some(PipelineFrame {
            bytes: data.len(),
            keyframe,
            encode_us,
            decode_us: decoded.decode_us,
            pipeline_us: now_us.saturating_sub(decoded.pts_us) as u32,
            frame: Arc::new(decoded),
        }))
    }
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

// ───────────────────────── hai nửa nối thẳng ─────────────────────────

/// Vòng nội bộ một máy: chụp → mã hoá → giải mã → vẽ, không qua mạng.
///
/// Giữ lại vì đây là thước đo duy nhất tách được độ trễ của phần xử lý ra khỏi
/// độ trễ của đường truyền. Số nó cho ra là sàn: chạy hai máy không thể nhanh
/// hơn con số này.
pub struct Pipeline {
    rx: Receiver<PipelineFrame>,
    info: PipelineInfo,
    counters: Arc<Mutex<PipelineCounters>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Pipeline {
    pub fn start(target_fps: u32, bitrate_kbps: u32, allow_10bit: bool) -> anyhow::Result<Self> {
        let mut encoder = EncodeSource::start(target_fps, bitrate_kbps, allow_10bit)?;
        let info = encoder.info().clone();

        #[cfg(target_os = "macos")]
        let mut decoder = DecodeSink::new(info.codec, info.width, info.height, info.chroma)?;
        #[cfg(target_os = "windows")]
        let mut decoder =
            DecodeSink::with_device(info.codec, info.width, info.height, &encoder.device)?;

        // Hàng đợi một chỗ: giao diện luôn nhận hình mới nhất, và luồng này
        // không bao giờ bị chặn vì giao diện chậm.
        let (tx, rx) = sync_channel::<PipelineFrame>(1);
        let counters = Arc::new(Mutex::new(PipelineCounters::default()));
        let stop = Arc::new(AtomicBool::new(false));

        let worker = {
            let counters = counters.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("rd-pipeline".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        match run_once(&mut encoder, &mut decoder, &tx, &counters) {
                            Ok(true) => {}
                            Ok(false) => break,
                            Err(err) => {
                                tracing::warn!(%err, "frame lỗi, bỏ qua");
                                counters.lock().expect("khoá đếm").errors += 1;
                            }
                        }
                    }
                    encoder.stop();
                })?
        };

        Ok(Self {
            rx,
            info,
            counters,
            stop,
            worker: Some(worker),
        })
    }

    pub fn info(&self) -> &PipelineInfo {
        &self.info
    }

    pub fn counters(&self) -> PipelineCounters {
        *self.counters.lock().expect("khoá đếm")
    }

    /// Lấy frame mới nhất, vứt mọi frame cũ hơn đang chờ.
    pub fn latest(&self) -> Option<PipelineFrame> {
        let mut newest = None;
        while let Ok(frame) = self.rx.try_recv() {
            newest = Some(frame);
        }
        newest
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Luồng có thể đang chờ trên kênh đầy; rút hết cho nó đi tiếp.
        while self.rx.try_recv().is_ok() {}
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Một vòng. `Ok(false)` nghĩa là giao diện đã đóng, dừng hẳn.
fn run_once(
    encoder: &mut EncodeSource,
    decoder: &mut DecodeSink,
    tx: &SyncSender<PipelineFrame>,
    counters: &Arc<Mutex<PipelineCounters>>,
) -> anyhow::Result<bool> {
    let Some(encoded) = encoder.next_encoded(Duration::from_millis(500))? else {
        return Ok(true);
    };
    {
        let mut counters = counters.lock().expect("khoá đếm");
        counters.captured += 1;
        counters.encoded += 1;
    }

    let decoded = decoder.decode(
        &encoded.data,
        encoded.pts_us,
        encoded.encode_us,
        encoded.keyframe,
    )?;
    let Some(item) = decoded else {
        return Ok(true);
    };
    counters.lock().expect("khoá đếm").decoded += 1;

    match tx.try_send(item) {
        Ok(()) => Ok(true),
        Err(TrySendError::Full(_)) => {
            counters.lock().expect("khoá đếm").dropped_late += 1;
            Ok(true)
        }
        Err(TrySendError::Disconnected(_)) => Ok(false),
    }
}

/// Nguồn frame: màn hình thật nếu được phép, nếu không thì nội dung tổng hợp.
enum Source {
    Screen(rd_capture::PlatformCapturer),
    Synthetic(rd_capture::synthetic::SyntheticCapturer),
}

impl Source {
    fn next_frame(&mut self, timeout: Duration) -> rd_capture::Result<rd_capture::CapturedFrame> {
        match self {
            Self::Screen(inner) => inner.next_frame(timeout),
            Self::Synthetic(inner) => inner.next_frame(timeout),
        }
    }

    fn stop(&mut self) {
        match self {
            Self::Screen(inner) => inner.stop(),
            Self::Synthetic(inner) => inner.stop(),
        }
    }
}

fn open_source(config: CaptureConfig) -> anyhow::Result<(Source, String)> {
    match rd_capture::PlatformCapturer::start(config.clone()) {
        Ok(capturer) => Ok((Source::Screen(capturer), "màn hình thật".into())),
        Err(err) => {
            tracing::warn!(%err, "không mở được màn hình thật, dùng nguồn tổng hợp");
            let capturer = rd_capture::synthetic::SyntheticCapturer::start(config)?;
            Ok((
                Source::Synthetic(capturer),
                "tổng hợp (chưa có quyền quay màn hình)".into(),
            ))
        }
    }
}
