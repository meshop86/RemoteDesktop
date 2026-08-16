//! Đo hiệu năng capture thật trên máy này.
//!
//! Chạy: `cargo run -p rd-capture --example capture_bench --release`
//!
//! Lần đầu chạy, macOS sẽ hỏi quyền Screen Recording cho ứng dụng terminal.
//! Cấp quyền rồi chạy lại (macOS yêu cầu khởi động lại tiến trình sau khi cấp).

use std::time::{Duration, Instant};

use rd_capture::{CaptureConfig, PlatformCapturer, ScreenCapturer};

const FRAMES: usize = 300;

fn main() {
    let displays = match PlatformCapturer::list_displays() {
        Ok(displays) => displays,
        Err(err) => {
            eprintln!("Không liệt kê được màn hình: {err}");
            std::process::exit(1);
        }
    };

    println!("Màn hình khả dụng:");
    for display in &displays {
        println!(
            "  id={} {}x{} scale={:.1} {}",
            display.id,
            display.width,
            display.height,
            display.scale,
            if display.is_primary { "(chính)" } else { "" }
        );
    }

    let target = &displays[0];
    let mut capturer = match PlatformCapturer::start(CaptureConfig {
        display_id: target.id,
        target_fps: 120, // xin tối đa, hệ thống tự giới hạn theo màn hình
        ..Default::default()
    }) {
        Ok(capturer) => capturer,
        Err(err) => {
            eprintln!("Không khởi động được capture: {err}");
            std::process::exit(1);
        }
    };

    println!("\nĐang capture {FRAMES} frame từ màn hình {}...", target.id);
    println!("(di chuyển chuột hoặc cửa sổ để màn hình có thay đổi)");

    let mut intervals_us: Vec<u64> = Vec::with_capacity(FRAMES);
    let mut delivery_us: Vec<i64> = Vec::with_capacity(FRAMES);
    let mut last: Option<Instant> = None;
    let start = Instant::now();
    let mut captured = 0usize;

    while captured < FRAMES {
        match capturer.next_frame(Duration::from_secs(5)) {
            Ok(frame) => {
                let now = Instant::now();
                if let Some(prev) = last {
                    intervals_us.push(now.duration_since(prev).as_micros() as u64);
                }
                last = Some(now);

                let now_us = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_micros() as u64;
                delivery_us.push(now_us as i64 - frame.capture_us as i64);

                if captured == 0 {
                    println!(
                        "Frame đầu tiên: {}x{} (surface {}x{})",
                        frame.width,
                        frame.height,
                        frame.surface.width(),
                        frame.surface.height()
                    );
                }
                captured += 1;
            }
            Err(err) => {
                eprintln!("Dừng sớm sau {captured} frame: {err}");
                break;
            }
        }
    }

    let elapsed = start.elapsed();
    let stats = capturer.stats();
    capturer.stop();

    if intervals_us.is_empty() {
        eprintln!("Không nhận được frame nào.");
        std::process::exit(1);
    }

    intervals_us.sort_unstable();
    delivery_us.sort_unstable();
    let p = |v: &[u64], pct: usize| v[(v.len() * pct / 100).min(v.len() - 1)];

    println!("\n--- Kết quả ---");
    println!(
        "{captured} frame trong {:.2}s => {:.1} fps thực tế",
        elapsed.as_secs_f64(),
        captured as f64 / elapsed.as_secs_f64()
    );
    println!(
        "Khoảng cách giữa 2 frame: p50 {:.2}ms  p99 {:.2}ms  min {:.2}ms",
        p(&intervals_us, 50) as f64 / 1000.0,
        p(&intervals_us, 99) as f64 / 1000.0,
        intervals_us[0] as f64 / 1000.0
    );
    println!(
        "Trễ từ lúc frame về tiến trình đến lúc lấy ra: p50 {:.2}ms",
        delivery_us[delivery_us.len() / 2] as f64 / 1000.0
    );
    println!(
        "Hệ thống giao {} frame | bỏ {} (encoder chậm) | bỏ qua {} frame không đổi",
        stats.frames_delivered, stats.frames_dropped, stats.frames_idle
    );
}
