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
    MFSTARTUP_LITE, MFStartup, MFT_ENUM_FLAG, MFT_MESSAGE_SET_D3D_MANAGER, MFT_REGISTER_TYPE_INFO,
    MFTEnumEx,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::Interface;

use crate::{CodecError, Result};

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

/// Tìm và khởi tạo transform đầu tiên khớp yêu cầu.
///
/// Media Foundation trả về một mảng `IMFActivate` do nó tự cấp phát, và việc
/// dọn mảng đó có hai nửa dễ quên: nhả tham chiếu từng phần tử, rồi mới trả
/// vùng nhớ. Gộp vào đây để cả bộ mã hoá lẫn bộ giải mã cùng dùng đúng một bản.
pub(crate) fn activate_first(
    category: windows::core::GUID,
    flags: MFT_ENUM_FLAG,
    input: Option<&MFT_REGISTER_TYPE_INFO>,
    output: Option<&MFT_REGISTER_TYPE_INFO>,
    what: &str,
) -> Result<IMFTransform> {
    let mut activates: *mut Option<IMFActivate> = null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            category,
            flags,
            input.map(|info| info as *const _),
            output.map(|info| info as *const _),
            &mut activates,
            &mut count,
        )
    }
    .map_err(|err| system("liệt kê transform", err))?;

    if activates.is_null() || count == 0 {
        if !activates.is_null() {
            unsafe { CoTaskMemFree(Some(activates.cast())) };
        }
        return Err(CodecError::Unsupported(format!("máy này không có {what}")));
    }

    // An toàn: Media Foundation vừa cấp mảng này với đúng `count` phần tử.
    let list = unsafe { std::slice::from_raw_parts(activates, count as usize) };
    let mut created = None;
    let mut last_error = None;
    for activate in list.iter().flatten() {
        match unsafe { activate.ActivateObject::<IMFTransform>() } {
            Ok(transform) => {
                created = Some(transform);
                break;
            }
            // Máy có nhiều GPU thì phần tử đầu có thể thuộc GPU đang tắt.
            Err(err) => last_error = Some(err),
        }
    }
    for activate in list.iter().flatten() {
        drop(activate.clone());
    }
    unsafe { CoTaskMemFree(Some(activates.cast())) };

    created.ok_or_else(|| match last_error {
        Some(err) => system("khởi tạo transform", err),
        None => CodecError::Unsupported(format!("không khởi tạo được {what}")),
    })
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
