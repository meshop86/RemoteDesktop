//! Thống kê trượt cho HUD.
//!
//! Chỉ giữ vài giây gần nhất chứ không tính trung bình từ đầu phiên: khi mạng
//! nghẽn hay màn hình đột ngột đổi nội dung, số liệu phải phản ứng ngay thì mới
//! dùng để chẩn đoán được.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Cửa sổ quan sát. 2 giây đủ mượt để số không nhảy loạn, đủ ngắn để thấy ngay
/// lúc chất lượng tụt.
const WINDOW: Duration = Duration::from_secs(2);

struct Sample {
    at: Instant,
    pipeline_us: u32,
    encode_us: u32,
    decode_us: u32,
    bytes: usize,
    keyframe: bool,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Summary {
    pub fps: f32,
    pub pipeline_p50_ms: f32,
    pub pipeline_p99_ms: f32,
    pub encode_p50_ms: f32,
    pub decode_p50_ms: f32,
    pub mbps: f32,
    /// Số keyframe trong cửa sổ. Keyframe nặng gấp nhiều lần frame thường nên
    /// đây là thứ đầu tiên cần nhìn khi bitrate đột nhiên vọt lên.
    pub keyframes: u32,
}

#[derive(Default)]
pub struct Metrics {
    samples: VecDeque<Sample>,
    /// Số frame giao diện vẽ được, kể cả khi vẽ lại cùng một frame video.
    pub repaints: u64,
}

impl Metrics {
    pub fn push(
        &mut self,
        pipeline_us: u32,
        encode_us: u32,
        decode_us: u32,
        bytes: usize,
        keyframe: bool,
    ) {
        let now = Instant::now();
        self.samples.push_back(Sample {
            at: now,
            pipeline_us,
            encode_us,
            decode_us,
            bytes,
            keyframe,
        });
        while let Some(front) = self.samples.front() {
            if now.duration_since(front.at) > WINDOW {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn summary(&self) -> Summary {
        if self.samples.is_empty() {
            return Summary::default();
        }
        let span = self
            .samples
            .back()
            .zip(self.samples.front())
            .map(|(last, first)| last.at.duration_since(first.at).as_secs_f32())
            .unwrap_or(0.0)
            .max(1e-3);

        let bytes: usize = self.samples.iter().map(|s| s.bytes).sum();
        let mut pipeline: Vec<u32> = self.samples.iter().map(|s| s.pipeline_us).collect();
        let mut encode: Vec<u32> = self.samples.iter().map(|s| s.encode_us).collect();
        let mut decode: Vec<u32> = self.samples.iter().map(|s| s.decode_us).collect();
        pipeline.sort_unstable();
        encode.sort_unstable();
        decode.sort_unstable();

        Summary {
            // Trừ 1 vì n mẫu chỉ tạo ra n-1 khoảng thời gian giữa các frame.
            fps: (self.samples.len().saturating_sub(1)) as f32 / span,
            pipeline_p50_ms: percentile_ms(&pipeline, 0.50),
            pipeline_p99_ms: percentile_ms(&pipeline, 0.99),
            encode_p50_ms: percentile_ms(&encode, 0.50),
            decode_p50_ms: percentile_ms(&decode, 0.50),
            mbps: bytes as f32 * 8.0 / span / 1e6,
            keyframes: self.samples.iter().filter(|s| s.keyframe).count() as u32,
        }
    }
}

fn percentile_ms(sorted: &[u32], q: f32) -> f32 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = (((sorted.len() - 1) as f32) * q).round() as usize;
    sorted[index] as f32 / 1000.0
}
