//! Đổi texture BGRA của màn hình sang NV12 cho bộ mã hoá, hoàn toàn trên GPU.
//!
//! **Vì sao phải đổi:** màn hình cho ra BGRA, còn mọi bộ mã hoá phần cứng trên
//! Windows đều chỉ nhận NV12. Không có đường tắt.
//!
//! **Vì sao dùng `ID3D11VideoProcessor` chứ không viết shader:** mạch chuyển
//! màu là phần cứng cố định nằm sẵn trong GPU, không ăn vào phần tính toán mà
//! bộ mã hoá đang dùng, và nó nằm cùng device nên pixel không rời VRAM. Một
//! pixel shader tự viết sẽ chiếm đúng phần GPU đang bận nhất.
//!
//! Đây là bước *có tổn hao*: NV12 là 4:2:0 8-bit, tức đã vứt đi ba phần tư
//! thông tin màu. Bản macOS giữ được 4:2:2 10-bit nhờ VideoToolbox nhận thẳng
//! BGRA. Chênh lệch này là thật và nhìn thấy được ở chữ màu trên nền màu.

use std::mem::ManuallyDrop;
use std::ptr::null_mut;

use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_RATE_NORMAL, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device,
    ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::core::Interface;

use crate::{CodecError, Result};
use crate::mf_params::{ColorUsage, NominalRange, color_space};

use super::system;

/// Số texture NV12 luân phiên. Bộ mã hoá phần cứng giữ frame lại vài nhịp sau
/// khi ta giao, nên ghi đè quá sớm là hình bị xé.
const RING: usize = 4;

/// Số view ảnh vào giữ lại. Vòng texture của capture chỉ vài cái nên con số
/// này thừa sức phủ hết, và có chặn trên thì cache không phình vô hạn nếu
/// nguồn đổi texture liên tục.
const MAX_INPUT_VIEWS: usize = 8;

pub(super) struct Nv12Converter {
    video_context: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    enumerator: ID3D11VideoProcessorEnumerator,
    video_device: ID3D11VideoDevice,
    ring: Vec<Target>,
    /// Ánh xạ texture nguồn → view. Tạo view mỗi frame là tạo một đối tượng
    /// COM 60 lần mỗi giây cho đúng vài texture lặp đi lặp lại.
    input_views: Vec<(*mut core::ffi::c_void, ID3D11VideoProcessorInputView)>,
    index: usize,
    width: u32,
    height: u32,
}

struct Target {
    texture: ID3D11Texture2D,
    view: ID3D11VideoProcessorOutputView,
}

impl Nv12Converter {
    pub(super) fn new(device: &ID3D11Device, width: u32, height: u32, fps: u32) -> Result<Self> {
        let video_device: ID3D11VideoDevice = device
            .cast()
            .map_err(|err| system("lấy giao diện video của device D3D11", err))?;
        let context = unsafe { device.GetImmediateContext() }
            .map_err(|err| system("lấy device context", err))?;
        let video_context: ID3D11VideoContext = context
            .cast()
            .map_err(|err| system("lấy giao diện video của context", err))?;

        let rate = DXGI_RATIONAL {
            Numerator: fps.max(1),
            Denominator: 1,
        };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: width,
            InputHeight: height,
            OutputFrameRate: rate,
            OutputWidth: width,
            OutputHeight: height,
            // Nói rõ ta cần nhanh chứ không cần đẹp: driver sẽ bỏ các bước lọc
            // khử nhiễu, khử răng cưa mà ta không dùng tới.
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };

        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&content) }
            .map_err(|err| system("tạo bộ liệt kê video processor", err))?;
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
            .map_err(|err| system("tạo video processor", err))?;

        // BGRA của màn hình là dải đầy 0-255; NV12 cho bộ mã hoá là dải hẹp
        // BT.709. Không khai báo thì driver đoán, và đoán sai làm ảnh bợt màu.
        let input = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
            _bitfield: color_space(ColorUsage::Processing, true, true, NominalRange::Full),
        };
        let output = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
            _bitfield: color_space(ColorUsage::Processing, false, true, NominalRange::Limited),
        };
        unsafe {
            video_context.VideoProcessorSetStreamColorSpace(&processor, 0, &input);
            video_context.VideoProcessorSetOutputColorSpace(&processor, &output);
            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            // Không chèn frame, không lặp frame: mỗi frame vào là đúng một
            // frame ra, nếu không thì độ trễ và số liệu đo đều sai.
            video_context.VideoProcessorSetStreamOutputRate(
                &processor,
                0,
                D3D11_VIDEO_PROCESSOR_OUTPUT_RATE_NORMAL,
                false,
                None,
            );
        }

        let mut ring = Vec::with_capacity(RING);
        for _ in 0..RING {
            ring.push(Target::new(device, &video_device, &enumerator, width, height)?);
        }

        Ok(Self {
            video_context,
            processor,
            enumerator,
            video_device,
            ring,
            input_views: Vec::new(),
            index: 0,
            width,
            height,
        })
    }

    /// Chuyển một texture BGRA thành texture NV12 trong vòng của ta.
    pub(super) fn convert(&mut self, source: &ID3D11Texture2D) -> Result<ID3D11Texture2D> {
        // Video processor cũng là bộ co giãn, nên khung hình sai kích thước sẽ
        // được nó *lặng lẽ* kéo cho vừa. Bên kia nhận được ảnh méo mà không có
        // lỗi nào báo về, nên chặn ngay tại đây.
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source.GetDesc(&mut desc) };
        if desc.Width != self.width || desc.Height != self.height {
            return Err(CodecError::Unsupported(format!(
                "khung hình {}x{} không khớp bộ mã hoá {}x{}",
                desc.Width, desc.Height, self.width, self.height
            )));
        }

        let input = self.input_view(source)?;
        let target = &self.ring[self.index % self.ring.len()];
        self.index = self.index.wrapping_add(1);

        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: null_mut(),
            pInputSurface: ManuallyDrop::new(Some(input)),
            ppFutureSurfaces: null_mut(),
            ppPastSurfacesRight: null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: null_mut(),
        };

        let outcome = unsafe {
            self.video_context.VideoProcessorBlt(
                &self.processor,
                &target.view,
                0,
                std::slice::from_ref(&stream),
            )
        };
        // `ManuallyDrop` nên phải tự nhả tham chiếu, kể cả khi Blt hỏng — bỏ
        // sót là rò một đối tượng COM mỗi frame.
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        outcome.map_err(|err| system("chuyển màu BGRA sang NV12", err))?;

        Ok(target.texture.clone())
    }

    fn input_view(&mut self, source: &ID3D11Texture2D) -> Result<ID3D11VideoProcessorInputView> {
        let key = source.as_raw();
        if let Some((_, view)) = self.input_views.iter().find(|(raw, _)| *raw == key) {
            return Ok(view.clone());
        }

        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        let mut view = None;
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                source,
                &self.enumerator,
                &desc,
                Some(&mut view),
            )
        }
        .map_err(|err| system("tạo view ảnh vào cho video processor", err))?;
        let view = view.expect("CreateVideoProcessorInputView thành công thì phải có view");

        if self.input_views.len() >= MAX_INPUT_VIEWS {
            self.input_views.remove(0);
        }
        self.input_views.push((key, view.clone()));
        Ok(view)
    }
}

impl Target {
    fn new(
        device: &ID3D11Device,
        video_device: &ID3D11VideoDevice,
        enumerator: &ID3D11VideoProcessorEnumerator,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            // Video processor ghi vào đây qua render target; bộ mã hoá đọc ra
            // không cần cờ bind nào.
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };

        let mut texture = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .map_err(|err| system("tạo texture NV12", err))?;
        let texture = texture.expect("CreateTexture2D thành công thì phải có texture");

        let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        let mut view = None;
        unsafe {
            video_device.CreateVideoProcessorOutputView(
                &texture,
                enumerator,
                &view_desc,
                Some(&mut view),
            )
        }
        .map_err(|err| system("tạo view ảnh ra cho video processor", err))?;

        Ok(Self {
            texture,
            view: view.expect("CreateVideoProcessorOutputView thành công thì phải có view"),
        })
    }
}
