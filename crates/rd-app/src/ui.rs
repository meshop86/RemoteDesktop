//! Mảnh giao diện dùng chung giữa chế độ mạng và chế độ thử tại chỗ.

use std::sync::Arc;

use eframe::egui;

#[cfg(target_os = "macos")]
use rd_codec::videotoolbox::DecodedFrame;
#[cfg(target_os = "windows")]
use rd_codec::mediafoundation::DecodedFrame;

use rd_viewer::metrics::Summary;
use rd_viewer::pipeline::PipelineInfo;
use rd_viewer::video;

pub const GOOD: egui::Color32 = egui::Color32::from_rgb(120, 220, 140);
pub const WARN: egui::Color32 = egui::Color32::from_rgb(230, 200, 110);
pub const BAD: egui::Color32 = egui::Color32::from_rgb(235, 120, 120);

/// Ô vẽ video bên trong `outer`.
///
/// Giữ đúng tỉ lệ khung hình của máy bên kia, thừa đâu để đen đó — kéo giãn cho
/// vừa cửa sổ sẽ làm chữ méo và khó đọc. Phần bắt input dùng lại đúng ô này để
/// chuẩn hoá toạ độ; tính riêng hai chỗ là chuột lệch đúng bằng bề rộng viền.
pub fn video_rect(outer: egui::Rect, width: u32, height: u32) -> egui::Rect {
    let aspect = width as f32 / height.max(1) as f32;
    let mut size = egui::vec2(outer.width(), outer.width() / aspect);
    if size.y > outer.height() {
        size = egui::vec2(outer.height() * aspect, outer.height());
    }
    egui::Rect::from_center_size(outer.center(), size)
}

/// Xanh dưới 30 ms (không cảm nhận được), vàng dưới 60 ms, đỏ trên nữa.
pub fn latency_color(ms: f32) -> egui::Color32 {
    if ms < 30.0 {
        GOOD
    } else if ms < 60.0 {
        WARN
    } else {
        BAD
    }
}

pub fn draw_video(ui: &egui::Ui, rect: egui::Rect, frame: &Arc<DecodedFrame>) {
    ui.painter()
        .add(eframe::egui_wgpu::Callback::new_paint_callback(
            rect,
            video::VideoCallback {
                frame: frame.clone(),
            },
        ));
}

/// Nền đen cho phần thừa hai bên video. `ui` mà eframe đưa vào không có nền, để
/// nguyên thì lộ ra màu nền mặc định của hệ điều hành.
pub fn paint_backdrop(ui: &egui::Ui, rect: egui::Rect) {
    ui.painter().rect_filled(rect, 0.0, egui::Color32::BLACK);
}

pub struct HudData<'a> {
    pub info: Option<&'a PipelineInfo>,
    pub summary: Summary,
    /// Dòng phụ ở cuối: trạng thái điều khiển, đường truyền...
    pub lines: Vec<(String, egui::Color32)>,
    /// Độ trễ mạng một chiều đo được, `None` khi chưa có mẫu ping nào.
    pub network_ms: Option<f32>,
}

pub fn draw_hud(ui: &mut egui::Ui, data: &HudData<'_>) {
    egui::Frame::NONE
        .fill(egui::Color32::from_black_alpha(190))
        .inner_margin(8.0)
        .corner_radius(6.0)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            match data.info {
                Some(info) => {
                    ui.label(
                        egui::RichText::new(format!(
                            "{}x{}  {:?} {:?}",
                            info.width, info.height, info.codec, info.chroma
                        ))
                        .monospace()
                        .strong(),
                    );
                    ui.label(
                        egui::RichText::new(format!("nguồn: {}", info.source))
                            .monospace()
                            .small(),
                    );
                    ui.separator();
                    ui.label(
                        egui::RichText::new(format!(
                            "{:5.1}/{} fps   {:5.1} Mbps   {} kf",
                            data.summary.fps,
                            info.target_fps,
                            data.summary.mbps,
                            data.summary.keyframes
                        ))
                        .monospace(),
                    );
                }
                None => {
                    ui.label(egui::RichText::new("chưa có video").monospace());
                }
            }

            ui.label(
                egui::RichText::new(format!(
                    "trễ  p50 {:5.1} ms   p99 {:5.1} ms",
                    data.summary.pipeline_p50_ms, data.summary.pipeline_p99_ms
                ))
                .monospace()
                .color(latency_color(data.summary.pipeline_p50_ms)),
            );
            let encode = if data.summary.encode_p50_ms > 0.0 {
                format!("{:4.1} ms", data.summary.encode_p50_ms)
            } else {
                // Bên nhận không biết máy kia mã hoá mất bao lâu — con số đó
                // không đi trên dây.
                "   —   ".to_string()
            };
            ui.label(
                egui::RichText::new(format!(
                    "  mã hoá {encode}   giải mã {:4.1} ms",
                    data.summary.decode_p50_ms
                ))
                .monospace()
                .small(),
            );
            if let Some(ms) = data.network_ms {
                ui.label(
                    egui::RichText::new(format!("  mạng một chiều {ms:4.1} ms"))
                        .monospace()
                        .small(),
                );
            }

            if !data.lines.is_empty() {
                ui.separator();
            }
            for (text, color) in &data.lines {
                ui.label(
                    egui::RichText::new(text)
                        .monospace()
                        .small()
                        .color(*color),
                );
            }
        });
}

/// Đặt HUD vào góc trên trái của `outer`.
pub fn hud_at(ui: &mut egui::Ui, outer: egui::Rect, data: &HudData<'_>) {
    let anchor = outer.min + egui::vec2(12.0, 12.0);
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(egui::Rect::from_min_size(anchor, egui::vec2(340.0, 240.0)))
            .layout(egui::Layout::top_down(egui::Align::Min)),
        |ui| draw_hud(ui, data),
    );
}

/// Đổi số byte sang chuỗi người đọc được.
pub fn bytes(value: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = value as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
