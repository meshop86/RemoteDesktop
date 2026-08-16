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

use std::sync::Arc;

use eframe::egui;

/// Nối tiến trình vào console của tiến trình gọi nó, nếu có.
///
/// Bản Windows dựng ở subsystem "windows" nên khởi động không kèm console. Chạy
/// từ terminal thì hàm này mượn lại console của terminal đó để `--help` và log
/// vẫn hiện ra; bấm từ Explorer thì không có console nào để mượn, `AttachConsole`
/// trả về lỗi và ta đi tiếp như thường.
///
/// Nối xong vẫn phải mở `CONOUT$`/`CONIN$` rồi gán làm handle chuẩn: handle mà
/// tiến trình nhận lúc khởi động không tự trỏ sang console vừa nối.
#[cfg(target_os = "windows")]
fn attach_parent_console() {
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Console::{
        AttachConsole, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE,
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

fn main() -> eframe::Result<()> {
    #[cfg(target_os = "windows")]
    attach_parent_console();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = match app::Args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(err) => {
            eprintln!("tham số không hợp lệ: {err}\n\n{}", app::USAGE);
            std::process::exit(2);
        }
    };
    if args.help {
        println!("{}", app::USAGE);
        return Ok(());
    }

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

/// Xin thêm tính năng texture 16-bit chuẩn hoá — điều kiện để hiển thị được
/// video 10-bit. Không có thì vẫn chạy, chỉ là ở 8-bit.
fn device_setup() -> eframe::egui_wgpu::WgpuSetup {
    let mut setup = match eframe::egui_wgpu::WgpuConfiguration::default().wgpu_setup {
        eframe::egui_wgpu::WgpuSetup::CreateNew(create) => create,
        other => return other,
    };
    setup.device_descriptor = Arc::new(|adapter: &wgpu::Adapter| wgpu::DeviceDescriptor {
        label: Some("remote-desktop"),
        required_features: rd_viewer::video::NEEDED_FOR_10BIT & adapter.features(),
        required_limits: wgpu::Limits::default(),
        ..Default::default()
    });
    eframe::egui_wgpu::WgpuSetup::CreateNew(setup)
}
