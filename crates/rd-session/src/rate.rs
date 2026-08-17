//! Tự chỉnh bitrate video theo đường truyền thật.
//!
//! Người dùng đặt một con số bitrate rồi để đó, nhưng đường truyền thì không
//! đứng yên: chuyển từ Wi-Fi sang 4G, ai đó trong nhà bật tải phim, hay đơn
//! giản là đi qua relay ở xa. Đặt 30 Mbps trên một đường chỉ tải nổi 5 Mbps thì
//! kết quả không phải "hình đẹp hơn" mà là hàng đợi phình ra, gói rơi hàng loạt,
//! và hình đứng vài giây một lần — tệ hơn hẳn so với gửi đúng 5 Mbps.
//!
//! Nên con số người dùng đặt được hiểu là **trần**, còn mức thật thì bộ này dò
//! lấy. Cách dò là AIMD nhìn hai dấu hiệu:
//!
//! * **Mất gói** — dấu hiệu cổ điển, nhưng chỉ xuất hiện *sau khi* hàng đợi đã
//!   đầy và tràn.
//! * **RTT phình** — thứ xảy ra *trước* đó. Router đời mới đệm cả trăm mili
//!   giây trước khi chịu vứt gói (bufferbloat), nên chờ tới lúc mất gói mới lùi
//!   là đã trễ cả một quãng dài mà người xem cảm nhận rõ.
//!
//! Cố ý **không** dùng cửa sổ nghẽn của QUIC làm trần: BBR ước lượng băng thông
//! từ lượng dữ liệu ta thật sự gửi, nên khi ta đang gửi ít, cửa sổ cũng nhỏ
//! theo. Lấy nó làm trần là tự khoá mình ở mức thấp và không bao giờ leo lên
//! lại được.

use std::time::Duration;

/// Sàn bitrate. Thấp hơn nữa thì hình nát tới mức không đọc nổi chữ, mà mục
/// đích của phần mềm này là nhìn được màn hình máy kia.
pub const MIN_KBPS: u32 = 600;

/// Mức khởi điểm khi vào phiên, nếu trần còn cao hơn.
///
/// Không mở hết cỡ ngay: giây đầu tiên là lúc chưa biết gì về đường truyền, mà
/// bắn 30 Mbps vào một đường 3 Mbps thì chính cái keyframe đầu tiên — thứ quyết
/// định bao lâu người xem thấy hình — là thứ bị vứt.
const START_KBPS: u32 = 8_000;

/// Hệ số leo mỗi nhịp khi đường sạch.
const UP: f32 = 1.3;

/// Hệ số lùi mỗi nhịp khi đường tắc. Lùi mạnh hơn leo, theo đúng lẽ thường của
/// điều khiển tắc nghẽn: leo nhầm chỉ tốn thêm một nhịp, lùi chậm thì cả chục
/// nhịp sau đều hỏng.
const DOWN: f32 = 0.75;

/// Tỉ lệ mất gói coi là tắc.
const LOSS_HIGH: f32 = 0.02;

/// Tỉ lệ mất gói coi là sạch, được phép leo.
const LOSS_LOW: f32 = 0.005;

/// RTT vượt quá `rtt_min * RTT_INFLATION + RTT_MARGIN` thì coi như hàng đợi
/// đang phình. Cộng thêm một khoảng cố định vì trong mạng nhà RTT gốc chỉ
/// khoảng 1 ms — nhân hệ số thôi thì mọi dao động vặt đều thành báo động.
const RTT_INFLATION: f32 = 1.5;
const RTT_MARGIN: Duration = Duration::from_millis(30);

/// Đổi ít hơn ngần này thì không báo ra: dựng lại tham số bộ mã hoá cũng có giá
/// của nó, mà 3% bitrate thì mắt không thấy.
const CHANGE_THRESHOLD: f32 = 0.05;

/// Ảnh chụp đường truyền tại một thời điểm — đúng những gì QUIC đếm được.
///
/// Hai bộ đếm gói là cộng dồn từ đầu phiên; bộ điều khiển tự lấy hiệu.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LinkSample {
    pub rtt: Duration,
    pub lost_packets: u64,
    pub sent_packets: u64,
}

#[derive(Debug)]
pub struct RateController {
    ceiling: u32,
    current: u32,
    rtt_min: Option<Duration>,
    last: Option<LinkSample>,
}

impl RateController {
    pub fn new(ceiling_kbps: u32) -> Self {
        let ceiling = ceiling_kbps.max(MIN_KBPS);
        Self {
            ceiling,
            current: START_KBPS.min(ceiling).max(MIN_KBPS),
            rtt_min: None,
            last: None,
        }
    }

    pub fn current(&self) -> u32 {
        self.current
    }

    pub fn ceiling(&self) -> u32 {
        self.ceiling
    }

    /// Đổi trần (người dùng kéo thanh bitrate). Trả về mức mới nếu việc này ép
    /// mức hiện tại phải đổi theo.
    pub fn set_ceiling(&mut self, kbps: u32) -> Option<u32> {
        self.ceiling = kbps.max(MIN_KBPS);
        let clamped = self.current.min(self.ceiling);
        (clamped != self.current).then(|| {
            self.current = clamped;
            clamped
        })
    }

    /// Nhận một ảnh chụp đường truyền. Trả về bitrate mới nếu đáng đổi.
    pub fn update(&mut self, sample: LinkSample) -> Option<u32> {
        if !sample.rtt.is_zero() {
            self.rtt_min = Some(match self.rtt_min {
                Some(min) => min.min(sample.rtt),
                None => sample.rtt,
            });
        }

        // Ảnh đầu tiên chỉ để làm mốc: chưa có hiệu số thì chưa biết gì về mất gói.
        let previous = self.last.replace(sample)?;

        let sent = sample.sent_packets.saturating_sub(previous.sent_packets);
        let lost = sample.lost_packets.saturating_sub(previous.lost_packets);
        let loss = if sent == 0 {
            0.0
        } else {
            lost as f32 / sent as f32
        };
        let queueing = self.queueing(sample.rtt);

        let target = if loss > LOSS_HIGH || queueing {
            self.current as f32 * DOWN
        } else if loss < LOSS_LOW {
            self.current as f32 * UP
        } else {
            // Vùng giữa: đường không sạch hẳn nhưng cũng chưa tắc. Giữ nguyên
            // còn hơn dao động lên xuống quanh một mức vốn đã đúng.
            return None;
        };

        let next = (target as u32).clamp(MIN_KBPS, self.ceiling);
        self.commit(next)
    }

    /// Hàng đợi trên đường có đang phình không.
    fn queueing(&self, rtt: Duration) -> bool {
        match self.rtt_min {
            Some(min) if !rtt.is_zero() => rtt > min.mul_f32(RTT_INFLATION) + RTT_MARGIN,
            _ => false,
        }
    }

    fn commit(&mut self, next: u32) -> Option<u32> {
        if next == self.current {
            return None;
        }
        let change = (next as f32 - self.current as f32).abs() / self.current as f32;
        // Chạm sàn hoặc trần thì báo dù bước nhỏ: đó là hai mốc mà phía gọi cần
        // biết mình đã tới, không phải một bước dò giữa đường.
        if change < CHANGE_THRESHOLD && next != MIN_KBPS && next != self.ceiling {
            return None;
        }
        self.current = next;
        Some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Một đường sạch: RTT đứng yên, không mất gói nào.
    fn clean(tick: u64) -> LinkSample {
        LinkSample {
            rtt: Duration::from_millis(20),
            lost_packets: 0,
            sent_packets: tick * 1000,
        }
    }

    #[test]
    fn clean_link_climbs_to_the_ceiling_and_stops() {
        let mut rate = RateController::new(30_000);
        assert_eq!(rate.current(), START_KBPS);
        for tick in 1..=10 {
            rate.update(clean(tick));
        }
        assert_eq!(rate.current(), 30_000, "đường sạch mà không leo tới trần");
        // Tới trần rồi thì thôi, không báo đổi nữa.
        assert_eq!(rate.update(clean(11)), None);
    }

    #[test]
    fn packet_loss_backs_off_but_never_below_the_floor() {
        let mut rate = RateController::new(30_000);
        rate.update(clean(1));
        let before = rate.current();

        // Mất 10% gói trong nhịp này.
        let hit = rate
            .update(LinkSample {
                rtt: Duration::from_millis(20),
                lost_packets: 100,
                sent_packets: 2000,
            })
            .expect("mất gói mà không lùi");
        assert!(hit < before, "{hit} không nhỏ hơn {before}");

        // Mất gói liên tục vẫn không được rơi xuống dưới sàn.
        for tick in 3..40 {
            rate.update(LinkSample {
                rtt: Duration::from_millis(20),
                lost_packets: tick * 100,
                sent_packets: tick * 1000,
            });
        }
        assert_eq!(rate.current(), MIN_KBPS);
    }

    #[test]
    fn growing_rtt_backs_off_even_without_any_loss() {
        // Bufferbloat: router đệm chứ không vứt gói, nên bộ đếm mất gói im lặng
        // trong khi độ trễ leo lên cả trăm mili giây.
        let mut rate = RateController::new(30_000);
        rate.update(clean(1));
        rate.update(clean(2));
        let before = rate.current();

        let after = rate
            .update(LinkSample {
                rtt: Duration::from_millis(220),
                lost_packets: 0,
                sent_packets: 3000,
            })
            .expect("RTT phình mà không lùi");
        assert!(after < before, "{after} không nhỏ hơn {before}");
    }

    #[test]
    fn small_rtt_jitter_is_not_congestion() {
        let mut rate = RateController::new(30_000);
        // Mạng nhà: RTT gốc 1 ms, nhảy lên 5 ms là chuyện bình thường.
        rate.update(LinkSample {
            rtt: Duration::from_millis(1),
            lost_packets: 0,
            sent_packets: 1000,
        });
        let before = rate.current();
        let after = rate.update(LinkSample {
            rtt: Duration::from_millis(5),
            lost_packets: 0,
            sent_packets: 2000,
        });
        assert!(
            after.is_none_or(|kbps| kbps > before),
            "dao động RTT vặt bị hiểu thành tắc nghẽn"
        );
    }

    #[test]
    fn lowering_the_ceiling_pulls_the_current_rate_down_at_once() {
        let mut rate = RateController::new(30_000);
        for tick in 1..=10 {
            rate.update(clean(tick));
        }
        assert_eq!(rate.current(), 30_000);

        assert_eq!(rate.set_ceiling(4_000), Some(4_000));
        assert_eq!(rate.current(), 4_000);
        // Trần cao lên thì không nhảy vọt theo — vẫn phải dò lại từng nhịp.
        assert_eq!(rate.set_ceiling(30_000), None);
        assert_eq!(rate.current(), 4_000);
    }

    #[test]
    fn the_first_sample_is_only_a_baseline() {
        let mut rate = RateController::new(30_000);
        // Bộ đếm cộng dồn từ đầu phiên: coi ảnh đầu là hiệu số thì mọi gói mất
        // từ trước lúc bắt tay đều bị tính vào nhịp này.
        assert_eq!(
            rate.update(LinkSample {
                rtt: Duration::from_millis(20),
                lost_packets: 5_000,
                sent_packets: 10_000,
            }),
            None
        );
        assert_eq!(rate.current(), START_KBPS);
    }

    #[test]
    fn a_ceiling_below_the_floor_still_leaves_a_usable_rate() {
        let rate = RateController::new(10);
        assert_eq!(rate.current(), MIN_KBPS);
        assert_eq!(rate.ceiling(), MIN_KBPS);
    }
}
