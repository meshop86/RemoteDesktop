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

mod app;
mod net;
mod session;
mod ui;

use std::sync::Arc;

use eframe::egui;

fn main() -> eframe::Result<()> {
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
