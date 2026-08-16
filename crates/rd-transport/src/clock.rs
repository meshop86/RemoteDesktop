//! Đồng bộ đồng hồ giữa host và viewer.
//!
//! Muốn hiện được "độ trễ end-to-end" thật, viewer phải so mốc thời gian capture
//! (theo đồng hồ host) với thời điểm hiện tại (theo đồng hồ viewer). Hai đồng hồ
//! này lệch nhau tuỳ máy, nên phải ước lượng độ lệch bằng cặp ping/pong theo
//! đúng cách NTP làm:
//!
//! ```text
//! t0 = viewer gửi Ping      t1 = host nhận và trả lời (host_us)
//! t3 = viewer nhận Pong
//! rtt    = t3 - t0
//! offset = t1 - (t0 + t3) / 2
//! ```
//!
//! Mẫu có RTT nhỏ nhất là mẫu ít bị hàng đợi mạng làm nhiễu nhất, nên ta giữ
//! mẫu tốt nhất trong một cửa sổ trượt thay vì lấy trung bình.

use std::time::{SystemTime, UNIX_EPOCH};

/// Mốc thời gian hiện tại tính bằng micro giây kể từ Unix epoch.
pub fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("đồng hồ hệ thống trước 1970")
        .as_micros() as u64
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    rtt_us: u64,
    offset_us: i64,
    taken_at_us: u64,
}

#[derive(Debug, Default)]
pub struct ClockSync {
    best: Option<Sample>,
    /// Mẫu cũ hơn ngưỡng này bị loại để bám theo trôi đồng hồ (clock drift).
    window_us: u64,
}

impl ClockSync {
    pub fn new() -> Self {
        Self {
            best: None,
            window_us: 30_000_000, // 30 giây
        }
    }

    /// Nạp một cặp ping/pong. `sent_us` là t0, `host_us` là t1, `recv_us` là t3.
    pub fn on_pong(&mut self, sent_us: u64, host_us: u64, recv_us: u64) {
        if recv_us < sent_us {
            return; // đồng hồ nhảy lùi, bỏ mẫu
        }
        let rtt_us = recv_us - sent_us;
        let midpoint = sent_us as i128 + (rtt_us as i128 / 2);
        let offset_us = (host_us as i128 - midpoint) as i64;
        let sample = Sample {
            rtt_us,
            offset_us,
            taken_at_us: recv_us,
        };

        let replace = match self.best {
            None => true,
            Some(best) => {
                rtt_us <= best.rtt_us || recv_us.saturating_sub(best.taken_at_us) > self.window_us
            }
        };
        if replace {
            self.best = Some(sample);
        }
    }

    pub fn rtt_us(&self) -> Option<u64> {
        self.best.map(|s| s.rtt_us)
    }

    pub fn offset_us(&self) -> Option<i64> {
        self.best.map(|s| s.offset_us)
    }

    /// Đổi mốc thời gian của host sang mốc thời gian của máy này.
    pub fn host_to_local(&self, host_us: u64) -> Option<u64> {
        let offset = self.offset_us()?;
        Some((host_us as i128 - offset as i128).max(0) as u64)
    }

    /// Độ trễ từ lúc host capture frame tới bây giờ, tính bằng micro giây.
    /// Trả về `None` khi chưa có mẫu ping/pong nào.
    pub fn latency_us(&self, capture_host_us: u64, now_local_us: u64) -> Option<i64> {
        let capture_local = self.host_to_local(capture_host_us)?;
        Some(now_local_us as i64 - capture_local as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offset_and_rtt_recovered() {
        let mut sync = ClockSync::new();
        // Host chạy nhanh hơn viewer 5 giây, RTT 20ms.
        let host_ahead_us: i64 = 5_000_000;
        let t0 = 1_000_000u64;
        let t3 = t0 + 20_000;
        let host_us = (t0 as i64 + 10_000 + host_ahead_us) as u64;
        sync.on_pong(t0, host_us, t3);

        assert_eq!(sync.rtt_us(), Some(20_000));
        assert_eq!(sync.offset_us(), Some(host_ahead_us));
    }

    #[test]
    fn latency_uses_offset() {
        let mut sync = ClockSync::new();
        let host_ahead_us: i64 = 5_000_000;
        let t0 = 1_000_000u64;
        let t3 = t0 + 20_000;
        sync.on_pong(t0, (t0 as i64 + 10_000 + host_ahead_us) as u64, t3);

        // Host capture ở thời điểm host = t3_local + offset - 15ms
        // => latency đúng phải là 15ms.
        let now_local = 2_000_000u64;
        let capture_host = (now_local as i64 + host_ahead_us - 15_000) as u64;
        let latency = sync.latency_us(capture_host, now_local).unwrap();
        assert_eq!(latency, 15_000);
    }

    #[test]
    fn keeps_sample_with_lowest_rtt() {
        let mut sync = ClockSync::new();
        sync.on_pong(0, 50_000, 100_000); // rtt 100ms
        sync.on_pong(200_000, 205_000, 210_000); // rtt 10ms — tốt hơn
        assert_eq!(sync.rtt_us(), Some(10_000));
        sync.on_pong(300_000, 340_000, 380_000); // rtt 80ms — giữ mẫu cũ
        assert_eq!(sync.rtt_us(), Some(10_000));
    }
}
