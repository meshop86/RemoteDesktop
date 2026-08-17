//! Điều khiển máy tính từ xa.
//!
//! Một chương trình, ba chế độ:
//!
//! * **Chia sẻ** — cho máy khác xem và điều khiển máy này.
//! * **Kết nối** — xem và điều khiển máy khác.
//! * **Thử tại chỗ** — chụp rồi giải mã ngay trong máy, không qua mạng. Đây là
//!   cách đo phần độ trễ không liên quan tới đường truyền; con số nó cho ra là
//!   sàn mà chạy hai máy không thể vượt qua.
//!
//! Tất cả tham số dòng lệnh đều tuỳ chọn — chúng chỉ điền sẵn vào màn hình đầu
//! để khỏi gõ lại khi thử đi thử lại:
//!
//! ```text
//! remote-desktop [--host] [--connect ĐỊA_CHỈ|MÃ] [--password MK]
//!                [--rendezvous ĐỊA_CHỈ] [--name TÊN] [--fps N]
//!                [--bitrate KBPS] [--downloads THƯ_MỤC]
//! ```

// Trên Windows, bản phát hành không được là chương trình console: bấm vào biểu
// tượng mà hiện thêm một cửa sổ đen sau lưng giao diện thì trông như lỗi. Đổi
// sang subsystem "windows" và tự nối lại console của terminal gọi nó (xem
// `attach_parent_console`) để chạy bằng dòng lệnh vẫn đọc được `--help` và log.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod app;
mod net;
mod session;
mod ui;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use eframe::egui;

/// Nối tiến trình vào console của tiến trình gọi nó, nếu có.
///
/// Bản Windows dựng ở subsystem "windows" nên khởi động không kèm console. Chạy
/// từ terminal thì hàm này mượn lại console của terminal đó để `--help` và log
/// vẫn hiện ra; bấm từ Explorer thì không có console nào để mượn, `AttachConsole`
/// trả về lỗi và ta đi tiếp như thường.
///
/// Nối xong vẫn phải mở `CONOUT$`/`CONIN$` rồi gán làm handle chuẩn: handle mà
/// tiến trình nhận lúc khởi động không tự trỏ sang console vừa nối. Nhưng chỉ
/// gán cho những ô còn trống — ai gọi mà có hứng thú chuyển hướng đầu ra sang
/// file hay pipe thì phải giữ nguyên ý họ.
#[cfg(target_os = "windows")]
fn attach_parent_console() {
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE, SetStdHandle,
    };
    use windows::core::w;

    // SAFETY: chỉ gắn vào console sẵn có của tiến trình cha. Thất bại (không có
    // cha, hoặc đã có console rồi) chỉ là một mã lỗi, không phải trạng thái hỏng.
    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) }.is_err() {
        return;
    }

    for (name, slot, access) in [
        (w!("CONOUT$"), STD_OUTPUT_HANDLE, GENERIC_WRITE),
        (w!("CONOUT$"), STD_ERROR_HANDLE, GENERIC_WRITE),
        (w!("CONIN$"), STD_INPUT_HANDLE, GENERIC_READ),
    ] {
        // SAFETY: mở thiết bị console vừa nối rồi gán vào đúng ô handle chuẩn.
        // Handle này sống tới khi tiến trình kết thúc nên không đóng ở đây.
        unsafe {
            // `GetStdHandle` báo lỗi cho cả handle rỗng lẫn handle hỏng, nên
            // thành công ở đây nghĩa là ô này đã có chủ.
            if GetStdHandle(slot).is_ok() {
                continue;
            }
            let Ok(handle) = CreateFileW(
                name,
                access.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            ) else {
                continue;
            };
            let _ = SetStdHandle(slot, handle);
        }
    }
}

/// Nơi để file nhật ký, theo đúng thói quen của từng hệ điều hành.
fn log_path() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let dir = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let dir = std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Logs"));
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let dir = std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"));

    Some(dir?.join("RemoteDesktop").join("rd.log"))
}

/// Ghi log ra màn hình *và* ra một file cố định.
///
/// File log không phải để cho vui: bản Windows chạy không kèm cửa sổ console,
/// nên khi chương trình tắt ngóm lúc khởi động thì không còn chỗ nào khác để
/// biết nó chết ở đâu. Có file thì người dùng gửi lại được.
///
/// Trả về đường dẫn file, hoặc `None` nếu không mở được (khi đó vẫn còn log
/// màn hình, chỉ là mất chỗ để đọc lại).
fn init_logging() -> Option<PathBuf> {
    use tracing_subscriber::prelude::*;

    // Log cũ quá cỡ thì bỏ hẳn rồi ghi lại từ đầu — đơn giản hơn xoay vòng file,
    // mà mục đích chỉ là xem lần chạy gần đây.
    const MAX_BYTES: u64 = 4 << 20;

    let path = log_path();
    let file = path.as_ref().and_then(|path| {
        std::fs::create_dir_all(path.parent()?).ok()?;
        if std::fs::metadata(path).is_ok_and(|meta| meta.len() > MAX_BYTES) {
            std::fs::remove_file(path).ok();
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
    });

    let mo_duoc = file.is_some();
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .with(file.map(|file| {
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(Arc::new(file))
        }))
        .init();

    // Không mở được file thì đừng chỉ người dùng tới một đường dẫn trống rỗng.
    path.filter(|_| mo_duoc)
}

/// Báo lỗi ở chỗ người dùng chắc chắn nhìn thấy, rồi thoát.
///
/// Chạy từ Explorer thì không có console: `eprintln!` bay vào hư không và
/// chương trình chỉ đơn giản là "bấm vào không lên gì". Hộp thoại là cách duy
/// nhất để lỗi khởi động đến được mắt người dùng.
fn fatal(message: &str, log: Option<&Path>) -> ! {
    let full = match log {
        Some(path) => format!("{message}\n\nNhật ký: {}", path.display()),
        None => message.to_owned(),
    };
    eprintln!("{full}");
    tracing::error!("{full}");
    show_error(&full);
    std::process::exit(1);
}

#[cfg(target_os = "windows")]
fn show_error(text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    use windows::core::HSTRING;

    // SAFETY: hai chuỗi sống hết lời gọi, và MessageBoxW chỉ đọc chúng.
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(text),
            &HSTRING::from("Điều khiển từ xa"),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(not(target_os = "windows"))]
fn show_error(_text: &str) {}

/// Cho mọi cú panic hiện ra thành hộp thoại, không chỉ nằm im trong log.
fn install_panic_hook(log: Option<PathBuf>) {
    let truoc = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Vẫn để cách báo lỗi mặc định chạy: nó in ra stderr và vào file log.
        truoc(info);
        let mut text = format!("Chương trình gặp lỗi và phải dừng.\n\n{info}");
        if let Some(path) = &log {
            text.push_str(&format!("\n\nNhật ký: {}", path.display()));
        }
        show_error(&text);
    }));
}

fn main() {
    #[cfg(target_os = "windows")]
    attach_parent_console();

    let log = init_logging();
    install_panic_hook(log.clone());
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        "khởi động"
    );

    let args = match app::Args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(err) => fatal(
            &format!("Tham số không hợp lệ: {err}\n\n{}", app::USAGE),
            log.as_deref(),
        ),
    };
    if args.help {
        println!("{}", app::USAGE);
        return;
    }

    if let Err(err) = run(args) {
        fatal(
            &format!(
                "Không mở được cửa sổ chương trình.\n\n{err}\n\n\
                 Thường là do trình điều khiển card màn hình. Thử cập nhật driver, \
                 hoặc chạy lại bằng dòng lệnh để xem chi tiết."
            ),
            log.as_deref(),
        );
    }
}

fn run(args: app::Args) -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([720.0, 460.0])
            .with_title("Điều khiển từ xa"),
        wgpu_options: eframe::egui_wgpu::WgpuConfiguration {
            surface: eframe::egui_wgpu::SurfaceConfig {
                // Không chờ nhịp quét màn hình: hình hiện sớm hơn tới một chu
                // kỳ quét (16 ms ở 60 Hz). Có thể thấy xé hình, nhưng với điều
                // khiển từ xa thì độ trễ quan trọng hơn.
                present_mode: wgpu::PresentMode::AutoNoVsync,
                desired_maximum_frame_latency: Some(1),
            },
            wgpu_setup: device_setup(),
            ..Default::default()
        },
        ..Default::default()
    };

    eframe::run_native(
        "remote-desktop",
        options,
        Box::new(move |cc| Ok(Box::new(app::RdApp::new(cc, args)?))),
    )
}

/// Chọn card màn hình và xin thêm tính năng texture 16-bit chuẩn hoá — điều
/// kiện để hiển thị được video 10-bit. Không có thì vẫn chạy, chỉ là ở 8-bit.
fn device_setup() -> eframe::egui_wgpu::WgpuSetup {
    let mut setup = match eframe::egui_wgpu::WgpuConfiguration::default().wgpu_setup {
        eframe::egui_wgpu::WgpuSetup::CreateNew(create) => create,
        other => return other,
    };
    setup.native_adapter_selector = Some(Arc::new(chon_card));
    setup.device_descriptor = Arc::new(|adapter: &wgpu::Adapter| {
        // Máy không có driver DirectX còn dùng được thì wgpu lùi về OpenGL, mà
        // ở đó hạn mức mặc định của WebGPU là quá tầm — đòi bằng được thì không
        // tạo nổi thiết bị và cửa sổ không mở lên. Cùng cách chọn của egui.
        let base = if adapter.get_info().backend == wgpu::Backend::Gl {
            wgpu::Limits::downlevel_webgl2_defaults()
        } else {
            wgpu::Limits::default()
        };
        wgpu::DeviceDescriptor {
            label: Some("remote-desktop"),
            required_features: rd_viewer::video::NEEDED_FOR_10BIT & adapter.features(),
            required_limits: wgpu::Limits {
                // Đủ cho màn hình 4K và cho khung hình nhận về.
                max_texture_dimension_2d: 8192,
                ..base
            },
            ..Default::default()
        }
    });
    eframe::egui_wgpu::WgpuSetup::CreateNew(setup)
}

/// Chọn card màn hình: card rời trước, rồi card tích hợp, cuối cùng mới tới bộ
/// dựng hình bằng phần mềm.
///
/// Cách chọn mặc định của eframe đòi một card "hiệu năng cao" và chịu thua nếu
/// không có — trên máy driver cũ hoặc trong phiên Remote Desktop của Windows,
/// thua nghĩa là bấm vào chương trình không lên gì cả. Ở đây thì máy còn card
/// nào nhận được là còn chạy, chậm cũng được, miễn là cửa sổ mở ra và người
/// dùng thấy được chuyện gì đang xảy ra.
fn chon_card(
    adapters: &[wgpu::Adapter],
    surface: Option<&wgpu::Surface<'_>>,
) -> Result<wgpu::Adapter, String> {
    fn thu_tu_uu_tien(device_type: wgpu::DeviceType) -> u8 {
        match device_type {
            wgpu::DeviceType::DiscreteGpu => 0,
            wgpu::DeviceType::IntegratedGpu => 1,
            wgpu::DeviceType::VirtualGpu => 2,
            wgpu::DeviceType::Cpu => 3,
            wgpu::DeviceType::Other => 4,
        }
    }

    let hop_le = |adapter: &wgpu::Adapter| match surface {
        // Card không vẽ được lên cửa sổ này thì có nhận cũng vô dụng.
        Some(surface) => !surface.get_capabilities(adapter).formats.is_empty(),
        None => true,
    };

    for adapter in adapters {
        let info = adapter.get_info();
        tracing::info!(
            name = info.name,
            backend = %info.backend,
            device_type = ?info.device_type,
            driver = info.driver_info,
            dung_duoc = hop_le(adapter),
            "thấy card màn hình"
        );
    }

    let chon = adapters
        .iter()
        .filter(|adapter| hop_le(adapter))
        .min_by_key(|adapter| thu_tu_uu_tien(adapter.get_info().device_type))
        .ok_or_else(|| {
            format!(
                "không có card màn hình nào dùng được (máy báo có {} card)",
                adapters.len()
            )
        })?;

    tracing::info!(name = chon.get_info().name, "dùng card");
    Ok(chon.clone())
}
