//! Capture màn hình macOS bằng ScreenCaptureKit.
//!
//! Luồng dữ liệu: ScreenCaptureKit gọi callback trên một dispatch queue riêng
//! của nó, đưa cho ta `CMSampleBuffer` bọc một `CVPixelBuffer` nằm trên
//! IOSurface. Ta chỉ giữ lại handle (retain), không đọc pixel, rồi đặt vào một
//! "ô" duy nhất mà luồng encode lấy ra.
//!
//! Ô chỉ chứa **một** frame: nếu encoder chưa lấy kịp mà frame mới đã tới,
//! frame cũ bị vứt. Đây là lựa chọn có chủ đích cho điều khiển từ xa — hiển thị
//! hình mới nhất quan trọng hơn hiển thị đủ mọi hình.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_core_video::{CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetWidth};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSObject, NSObjectProtocol, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType, SCWindow,
};

use crate::{
    CaptureConfig, CaptureError, CaptureStats, CapturedFrame, DisplayInfo, FrameSlot, PixelFormat,
    Result, ScreenCapturer,
};

/// Handle tới bộ nhớ GPU chứa frame. Không sao chép pixel.
pub struct PixelSurface(CFRetained<CVPixelBuffer>);

// An toàn: CVPixelBuffer là CFType, retain/release nguyên tử, và Apple cho phép
// chuyển pixel buffer giữa các luồng (đây là cách mọi pipeline AVFoundation
// hoạt động). Ta chỉ chuyển quyền sở hữu từ luồng callback sang luồng encode,
// không truy cập đồng thời từ hai luồng.
unsafe impl Send for PixelSurface {}

impl PixelSurface {
    pub fn from_retained(buffer: CFRetained<CVPixelBuffer>) -> Self {
        Self(buffer)
    }

    pub fn as_pixel_buffer(&self) -> &CVPixelBuffer {
        &self.0
    }

    pub fn width(&self) -> u32 {
        CVPixelBufferGetWidth(&self.0) as u32
    }

    pub fn height(&self) -> u32 {
        CVPixelBufferGetHeight(&self.0) as u32
    }
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}

/// Ivars của lớp Objective-C nhận callback.
struct OutputIvars {
    slot: Arc<FrameSlot<CapturedFrame>>,
    counter: Mutex<u64>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "RdScreenOutput"]
    #[ivars = OutputIvars]
    struct ScreenOutput;

    unsafe impl NSObjectProtocol for ScreenOutput {}

    unsafe impl SCStreamOutput for ScreenOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn stream_did_output(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            output_type: SCStreamOutputType,
        ) {
            if output_type != SCStreamOutputType::Screen {
                return;
            }
            let ivars = self.ivars();

            // Bỏ frame mà hệ thống báo là không có gì đổi: encode chúng chỉ tốn
            // điện và băng thông.
            if let Some(status) = unsafe { frame_status(sample_buffer) }
                && status != SCFrameStatus::Complete
            {
                ivars.slot.note_idle();
                return;
            }

            let Some(image_buffer) = (unsafe { sample_buffer.image_buffer() }) else {
                return;
            };
            let surface = PixelSurface(image_buffer);
            let mut counter = ivars.counter.lock().expect("counter mutex bị poison");
            *counter += 1;
            let frame_index = *counter;
            drop(counter);

            ivars.slot.put(CapturedFrame {
                width: surface.width(),
                height: surface.height(),
                capture_us: now_us(),
                frame_index,
                surface,
            });
        }
    }

    unsafe impl SCStreamDelegate for ScreenOutput {
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn stream_did_stop(&self, _stream: &SCStream, error: &NSError) {
            let message = error.localizedDescription().to_string();
            tracing::warn!(%message, "ScreenCaptureKit dừng luồng");
            self.ivars().slot.stop(Some(message));
        }
    }
);

impl ScreenOutput {
    fn new(slot: Arc<FrameSlot<CapturedFrame>>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars {
            slot,
            counter: Mutex::new(0),
        });
        unsafe { msg_send![super(this), init] }
    }
}

/// Đọc trạng thái frame từ attachment của sample buffer.
unsafe fn frame_status(sample_buffer: &CMSampleBuffer) -> Option<SCFrameStatus> {
    let attachments = unsafe { sample_buffer.sample_attachments_array(false) }?;
    // CFArray và NSArray là toll-free bridged, ép kiểu ở đây là hợp lệ.
    let ptr: *const _ = &*attachments;
    let array: &NSArray<NSDictionary<NSString, AnyObject>> = unsafe { &*ptr.cast() };
    let dict = array.firstObject()?;
    let value = unsafe { dict.objectForKey(SCStreamFrameInfoStatus) }?;
    let number = value.downcast::<NSNumber>().ok()?;
    Some(SCFrameStatus(number.integerValue()))
}

/// Bọc `Retained<T>` để chuyển được qua channel giữa các luồng.
struct SendRetained<T: objc2::Message>(Retained<T>);
// An toàn: chỉ dùng cho object bất biến (SCShareableContent) và chuyển hẳn
// quyền sở hữu, không chia sẻ đồng thời.
unsafe impl<T: objc2::Message> Send for SendRetained<T> {}

/// Đổi NSError của ScreenCaptureKit thành lỗi của ta.
///
/// Phải so theo **mã lỗi**, không so chuỗi: macOS trả thông báo theo ngôn ngữ
/// hệ thống nên so chuỗi tiếng Anh sẽ trượt trên máy cài tiếng Việt.
fn classify_error(code: isize, message: String) -> CaptureError {
    match code {
        // SCStreamErrorUserDeclined / MissingEntitlements
        -3801 | -3803 => CaptureError::PermissionDenied,
        _ => CaptureError::Platform(format!("{message} (mã {code})")),
    }
}

fn shareable_content(timeout: Duration) -> Result<Retained<SCShareableContent>> {
    let (tx, rx) =
        std::sync::mpsc::channel::<std::result::Result<SendRetained<SCShareableContent>, (isize, String)>>();
    let handler = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        let result = if content.is_null() {
            let detail = if error.is_null() {
                (0, "ScreenCaptureKit không trả về nội dung".to_string())
            } else {
                let error = unsafe { &*error };
                (error.code(), error.localizedDescription().to_string())
            };
            Err(detail)
        } else {
            match unsafe { Retained::retain(content) } {
                Some(retained) => Ok(SendRetained(retained)),
                None => Err((0, "không giữ được tham chiếu SCShareableContent".to_string())),
            }
        };
        let _ = tx.send(result);
    });

    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };

    match rx.recv_timeout(timeout) {
        Ok(Ok(content)) => Ok(content.0),
        Ok(Err((code, message))) => Err(classify_error(code, message)),
        // Hệ thống không gọi lại thường là do chưa được cấp quyền quay màn hình.
        Err(_) => Err(CaptureError::PermissionDenied),
    }
}

pub struct MacScreenCapturer {
    stream: Retained<SCStream>,
    _output: Retained<ScreenOutput>,
    slot: Arc<FrameSlot<CapturedFrame>>,
    display: DisplayInfo,
    running: bool,
}

// `Retained<T>` chỉ tự động là `Send` khi `T: Send + Sync`, mà objc2 không dám
// khẳng định điều đó cho lớp do hệ thống định nghĩa. Ở đây thì chuyển được:
// SCStream không phải đối tượng giao diện nên không đòi luồng chính, nó tự đẩy
// frame về trên dispatch queue riêng, và bộ đếm tham chiếu của ObjC là nguyên tử.
// Chỉ `Send` chứ không `Sync`: mọi phương thức đổi trạng thái đều nhận `&mut self`
// nên tại một thời điểm chỉ một luồng chạm vào được.
unsafe impl Send for MacScreenCapturer {}

impl MacScreenCapturer {
    pub fn display(&self) -> &DisplayInfo {
        &self.display
    }
}

impl ScreenCapturer for MacScreenCapturer {
    fn list_displays() -> Result<Vec<DisplayInfo>> {
        let content = shareable_content(Duration::from_secs(10))?;
        let displays = unsafe { content.displays() };
        let mut out = Vec::with_capacity(displays.len());
        for (index, display) in displays.iter().enumerate() {
            let display_id = unsafe { display.displayID() };
            let filter = unsafe {
                SCContentFilter::initWithDisplay_excludingWindows(
                    SCContentFilter::alloc(),
                    &display,
                    &NSArray::<SCWindow>::new(),
                )
            };
            // pointPixelScale là hệ số Retina; nhân với contentRect mới ra số
            // pixel thật, nếu không sẽ capture ở nửa độ phân giải trên máy Retina.
            let scale = unsafe { filter.pointPixelScale() };
            let rect = unsafe { filter.contentRect() };
            out.push(DisplayInfo {
                id: display_id,
                name: format!("Display {}", index + 1),
                width: (rect.size.width * scale as f64).round() as u32,
                height: (rect.size.height * scale as f64).round() as u32,
                scale,
                refresh_hz: 60,
                is_primary: index == 0,
            });
        }
        if out.is_empty() {
            return Err(CaptureError::Platform("không tìm thấy màn hình nào".into()));
        }
        Ok(out)
    }

    fn start(config: CaptureConfig) -> Result<Self> {
        let content = shareable_content(Duration::from_secs(10))?;
        let displays = unsafe { content.displays() };
        let display: Retained<SCDisplay> = displays
            .iter()
            .find(|d| config.display_id == 0 || unsafe { d.displayID() } == config.display_id)
            .ok_or(CaptureError::DisplayNotFound(config.display_id))?;

        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::<SCWindow>::new(),
            )
        };
        let scale = unsafe { filter.pointPixelScale() };
        let rect = unsafe { filter.contentRect() };
        let mut width = (rect.size.width * scale as f64).round() as u32;
        let mut height = (rect.size.height * scale as f64).round() as u32;

        if let Some(limit) = config.max_dimension
            && width.max(height) > limit
        {
            let factor = limit as f64 / width.max(height) as f64;
            width = (width as f64 * factor).round() as u32;
            height = (height as f64 * factor).round() as u32;
        }
        // Encoder phần cứng yêu cầu kích thước chẵn.
        width -= width % 2;
        height -= height % 2;

        let stream_config = unsafe { SCStreamConfiguration::new() };
        unsafe {
            stream_config.setWidth(width as usize);
            stream_config.setHeight(height as usize);
            stream_config.setPixelFormat(match config.pixel_format {
                PixelFormat::Bgra32 => fourcc(b"BGRA"),
                PixelFormat::Nv12 => fourcc(b"420v"),
            });
            stream_config.setMinimumFrameInterval(CMTime {
                value: 1,
                timescale: config.target_fps.max(1) as i32,
                flags: CMTimeFlags::Valid,
                epoch: 0,
            });
            stream_config.setQueueDepth(config.queue_depth.max(3) as isize);
            stream_config.setShowsCursor(config.show_cursor);
            stream_config.setScalesToFit(false);
            stream_config.setPreservesAspectRatio(true);
        }

        let slot = FrameSlot::new();
        let output = ScreenOutput::new(slot.clone());
        let delegate = ProtocolObject::<dyn SCStreamDelegate>::from_ref(&*output);
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &stream_config,
                Some(delegate),
            )
        };

        // Queue riêng cho callback capture: không dùng chung với queue nào khác
        // để frame không bị kẹt sau công việc khác.
        let queue = DispatchQueue::new("com.rd.capture", None);
        unsafe {
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::<dyn SCStreamOutput>::from_ref(&*output),
                    SCStreamOutputType::Screen,
                    Some(&queue),
                )
                .map_err(|e| CaptureError::Platform(e.localizedDescription().to_string()))?;
        }

        let (tx, rx) = std::sync::mpsc::channel::<Option<(isize, String)>>();
        let handler = RcBlock::new(move |error: *mut NSError| {
            let detail = if error.is_null() {
                None
            } else {
                let error = unsafe { &*error };
                Some((error.code(), error.localizedDescription().to_string()))
            };
            let _ = tx.send(detail);
        });
        unsafe { stream.startCaptureWithCompletionHandler(Some(&handler)) };

        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(None) => {}
            Ok(Some((code, message))) => return Err(classify_error(code, message)),
            Err(_) => return Err(CaptureError::PermissionDenied),
        }

        Ok(Self {
            stream,
            _output: output,
            slot,
            display: DisplayInfo {
                id: unsafe { display.displayID() },
                name: "Display".to_string(),
                width,
                height,
                scale,
                refresh_hz: config.target_fps,
                is_primary: true,
            },
            running: true,
        })
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<CapturedFrame> {
        self.slot.take(timeout)
    }

    fn stats(&self) -> CaptureStats {
        self.slot.stats()
    }

    fn stop(&mut self) {
        if !self.running {
            return;
        }
        self.running = false;
        unsafe { self.stream.stopCaptureWithCompletionHandler(None) };
        self.slot.stop(None);
    }
}

impl Drop for MacScreenCapturer {
    fn drop(&mut self) {
        self.stop();
    }
}
