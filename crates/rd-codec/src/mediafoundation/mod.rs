//! Mã hoá và giải mã bằng Media Foundation trên Windows.
//!
//! Bản đối chiếu của [`crate::videotoolbox`], nhưng khác nó ở ba chỗ đáng lưu ý
//! — đều là giới hạn của nền tảng chứ không phải lựa chọn:
//!
//! 1. **Chỉ 4:2:0 8-bit.** Bộ mã hoá phần cứng qua Media Foundation nhận NV12,
//!    hết. VideoToolbox trên macOS nhận thẳng BGRA nên giữ được 4:2:2 10-bit.
//! 2. **Chỉ bộ mã hoá phần cứng.** Bộ mã hoá phần mềm của Windows là MFT đồng
//!    bộ, cách điều khiển khác hẳn, và ở 4K nó không theo kịp thời gian thực —
//!    hỗ trợ nó là viết thêm một nhánh code chỉ để cho ra trải nghiệm không
//!    dùng được. Không có phần cứng thì báo [`CodecError::Unsupported`].
//! 3. **Phải chuyển màu trước khi mã hoá.** Xem [`convert`].
//!
//! MFT phần cứng chạy *bất đồng bộ*: ta không gọi "mã hoá frame này" rồi chờ,
//! mà nó bắn sự kiện "cho tôi frame" / "có frame ra" và ta đáp lại. Vì thế
//! [`MfEncoder`] có hàng đợi bên trong giống hệt bản VideoToolbox.

mod convert;
mod decoder;
mod encoder;

pub use decoder::{DecodedFrame, MfDecoder};
pub use encoder::MfEncoder;

use std::ptr::null_mut;
use std::sync::OnceLock;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
    D3D11CreateDevice, ID3D11Device, ID3D11Multithread,
};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFDXGIDeviceManager, IMFTransform, MF_VERSION, MFCreateDXGIDeviceManager,
    MFMediaType_Video, MFSTARTUP_LITE, MFStartup, MFT_CATEGORY_VIDEO_DECODER,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_ENUM_FLAG_SYNCMFT, MFT_MESSAGE_SET_D3D_MANAGER, MFT_REGISTER_TYPE_INFO, MFTEnumEx,
    MFVideoFormat_H264, MFVideoFormat_HEVC,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::Interface;

use crate::{Codec, CodecError, Result};

/// Gói một lỗi HRESULT vào lỗi của ta, kèm tên việc đang làm.
///
/// HRESULT trần (`0x80070005`) không cho biết gì; cái quyết định người đọc log
/// có lần ra được hay không là câu mô tả đi kèm.
pub(crate) fn system(what: &'static str, err: windows::core::Error) -> CodecError {
    CodecError::System {
        what,
        detail: err.to_string(),
    }
}

/// Khởi động Media Foundation đúng một lần cho cả tiến trình.
///
/// Không gọi `MFShutdown`: nó phải là lời gọi cuối cùng chạm tới Media
/// Foundation trong tiến trình, mà ta không biết bộ mã hoá nào còn sống ở
/// luồng nào. Bỏ qua nó chỉ nghĩa là vài tài nguyên được hệ điều hành thu hồi
/// lúc thoát, thay vì trước đó vài mili giây.
pub(crate) fn ensure_started() -> Result<()> {
    static STARTED: OnceLock<std::result::Result<(), String>> = OnceLock::new();

    // `MFSTARTUP_LITE` bỏ qua phần dựng socket cho luồng mạng của Media
    // Foundation — ta tự lo phần truyền, không dùng tới nó.
    match STARTED.get_or_init(|| {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) }.map_err(|err| err.to_string())
    }) {
        Ok(()) => Ok(()),
        Err(detail) => Err(CodecError::System {
            what: "khởi động Media Foundation",
            detail: detail.clone(),
        }),
    }
}

/// Mảng `IMFActivate` do Media Foundation cấp phát, tự dọn khi ra khỏi tầm.
///
/// Dọn có hai nửa dễ quên: nhả tham chiếu của *từng phần tử*, rồi mới trả vùng
/// nhớ của mảng. Gói vào đây để cả bộ mã hoá, bộ giải mã lẫn phần dò khả năng
/// máy cùng dùng đúng một bản.
struct Activates {
    ptr: *mut Option<IMFActivate>,
    count: u32,
}

impl Activates {
    fn enumerate(
        category: windows::core::GUID,
        flags: MFT_ENUM_FLAG,
        input: Option<&MFT_REGISTER_TYPE_INFO>,
        output: Option<&MFT_REGISTER_TYPE_INFO>,
    ) -> Result<Self> {
        let mut ptr: *mut Option<IMFActivate> = null_mut();
        let mut count = 0u32;
        unsafe {
            MFTEnumEx(
                category,
                flags,
                input.map(|info| info as *const _),
                output.map(|info| info as *const _),
                &mut ptr,
                &mut count,
            )
        }
        .map_err(|err| system("liệt kê transform", err))?;
        // Không có phần tử nào thì con trỏ có thể null; `count` là nguồn sự thật.
        if ptr.is_null() {
            count = 0;
        }
        Ok(Self { ptr, count })
    }

    fn as_slice(&self) -> &[Option<IMFActivate>] {
        if self.ptr.is_null() {
            return &[];
        }
        // An toàn: Media Foundation vừa cấp mảng này với đúng `count` phần tử.
        unsafe { std::slice::from_raw_parts(self.ptr, self.count as usize) }
    }
}

impl Drop for Activates {
    fn drop(&mut self) {
        if self.ptr.is_null() {
            return;
        }
        for index in 0..self.count as usize {
            // Đọc giá trị ra rồi thả chính là nhả tham chiếu mà `MFTEnumEx` đã
            // tăng hộ ta. An toàn vì sau vòng này không ai đọc lại ô đó nữa —
            // vùng nhớ được trả ngay bên dưới.
            drop(unsafe { std::ptr::read(self.ptr.add(index)) });
        }
        unsafe { CoTaskMemFree(Some(self.ptr.cast())) };
    }
}

/// Tìm và khởi tạo transform đầu tiên khớp yêu cầu.
pub(crate) fn activate_first(
    category: windows::core::GUID,
    flags: MFT_ENUM_FLAG,
    input: Option<&MFT_REGISTER_TYPE_INFO>,
    output: Option<&MFT_REGISTER_TYPE_INFO>,
    what: &str,
) -> Result<IMFTransform> {
    let activates = Activates::enumerate(category, flags, input, output)?;
    if activates.count == 0 {
        return Err(CodecError::Unsupported(format!("máy này không có {what}")));
    }

    let mut created = None;
    let mut last_error = None;
    for activate in activates.as_slice().iter().flatten() {
        match unsafe { activate.ActivateObject::<IMFTransform>() } {
            Ok(transform) => {
                created = Some(transform);
                break;
            }
            // Máy có nhiều GPU thì phần tử đầu có thể thuộc GPU đang tắt.
            Err(err) => last_error = Some(err),
        }
    }

    created.ok_or_else(|| match last_error {
        Some(err) => system("khởi tạo transform", err),
        None => CodecError::Unsupported(format!("không khởi tạo được {what}")),
    })
}

/// Có ít nhất một transform khớp yêu cầu hay không.
///
/// Chỉ đếm chứ không khởi tạo: đủ để trả lời "máy này giải mã được HEVC không"
/// mà không đánh thức GPU hay giữ tài nguyên nào.
fn has_transform(
    category: windows::core::GUID,
    flags: MFT_ENUM_FLAG,
    input: Option<&MFT_REGISTER_TYPE_INFO>,
    output: Option<&MFT_REGISTER_TYPE_INFO>,
) -> bool {
    Activates::enumerate(category, flags, input, output)
        .map(|activates| activates.count > 0)
        .unwrap_or(false)
}

fn subtype(codec: Codec) -> Option<windows::core::GUID> {
    match codec {
        Codec::H264 => Some(MFVideoFormat_H264),
        Codec::Hevc => Some(MFVideoFormat_HEVC),
        Codec::Av1 => None,
    }
}

/// Codec mà máy này **giải mã** được.
///
/// Điều kiện dò phải trùng khít với điều kiện lúc dựng thật ở [`decoder`],
/// không thì lời khai này thành lời hứa suông.
///
/// Windows **không** kèm sẵn bộ giải mã HEVC — nó nằm trong gói "HEVC Video
/// Extensions" của Microsoft Store — nên một máy Windows mới cài thường chỉ trả
/// về H.264.
pub fn decodable() -> Vec<Codec> {
    if ensure_started().is_err() {
        return Vec::new();
    }
    [Codec::Hevc, Codec::H264]
        .into_iter()
        .filter(|codec| {
            let Some(guid) = subtype(*codec) else {
                return false;
            };
            let info = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: guid,
            };
            has_transform(
                MFT_CATEGORY_VIDEO_DECODER,
                MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&info),
                None,
            )
        })
        .collect()
}

/// Codec mà máy này **mã hoá** được bằng phần cứng.
pub fn encodable() -> Vec<Codec> {
    if ensure_started().is_err() {
        return Vec::new();
    }
    [Codec::Hevc, Codec::H264]
        .into_iter()
        .filter(|codec| {
            let Some(guid) = subtype(*codec) else {
                return false;
            };
            let info = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: guid,
            };
            has_transform(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                None,
                Some(&info),
            )
        })
        .collect()
}

/// Mở một device D3D11 dùng được cho Media Foundation.
///
/// Máy chỉ làm viewer thì không có capture, mà không có capture thì không có
/// device nào sẵn — bộ giải mã vẫn cần một cái để đặt texture đầu ra vào VRAM.
///
/// Ba điều kiện, thiếu cái nào cũng hỏng theo kiểu khó lần:
///
/// * `VIDEO_SUPPORT` — không có thì `ResetDevice` của device manager từ chối.
/// * `BGRA_SUPPORT` — cần cho đường chuyển màu và cho chia sẻ sang Direct2D.
/// * **Khoá đa luồng** — MFT phần cứng gọi vào device từ luồng riêng của nó,
///   song song với luồng ta đang chép texture. Device D3D11 mặc định *không*
///   an toàn đa luồng; thiếu khoá này thì hỏng ngẫu nhiên chứ không báo lỗi.
pub fn create_device() -> Result<ID3D11Device> {
    let mut last = String::new();
    // Thử GPU thật trước, rồi mới tới bộ dựng hình phần mềm: máy ảo và máy CI
    // thường không có GPU, mà giải mã bằng WARP tuy chậm nhưng vẫn chạy được.
    for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        match try_create_device(driver) {
            Ok(device) => return Ok(device),
            Err(err) => last = err.to_string(),
        }
    }
    Err(CodecError::System {
        what: "mở device D3D11",
        detail: last,
    })
}

fn try_create_device(driver: D3D_DRIVER_TYPE) -> windows::core::Result<ID3D11Device> {
    let mut device = None;
    unsafe {
        D3D11CreateDevice(
            None,
            driver,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
    }
    let device = device.expect("D3D11CreateDevice thành công thì phải có device");
    let multithread: ID3D11Multithread = device.cast()?;
    // Trả về trạng thái *trước* khi đổi, không phải mã lỗi — ta đang đặt chứ
    // không đang hỏi nên không cần biết.
    let _ = unsafe { multithread.SetMultithreadProtected(true) };
    Ok(device)
}

/// Nối transform vào device D3D11 để nó đọc/ghi thẳng texture trong VRAM.
///
/// Giá trị trả về phải sống đúng bằng tuổi thọ của transform — thả sớm là
/// transform mất đường vào GPU giữa chừng.
pub(crate) fn attach_device(
    transform: &IMFTransform,
    device: &ID3D11Device,
) -> Result<IMFDXGIDeviceManager> {
    let mut token = 0u32;
    let mut manager = None;
    unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }
        .map_err(|err| system("tạo device manager DXGI", err))?;
    let manager = manager.expect("MFCreateDXGIDeviceManager thành công thì phải có manager");

    unsafe { manager.ResetDevice(device, token) }
        .map_err(|err| system("gắn device D3D11 vào device manager", err))?;
    unsafe { transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize) }
        .map_err(|err| system("giao device manager cho transform", err))?;

    Ok(manager)
}
