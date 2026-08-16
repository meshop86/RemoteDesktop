//! Sổ đăng ký của rendezvous server: mã máy → chỗ gọi tới.
//!
//! Tách khỏi phần mạng để test được logic hết hạn và cấp mã mà không phải dựng
//! server thật. Thời điểm truyền vào từ ngoài (`now`) chứ không đọc đồng hồ bên
//! trong — nhờ vậy test tua được vài giờ trong một phần nghìn giây.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::proto::{Candidates, FromServer, PeerId};

/// Đăng ký hết hạn sau bao lâu không nghe thấy gì.
///
/// Host ping mỗi vài giây, nên 30 giây là đã lỡ nhiều nhịp — máy đó tắt hoặc
/// mất mạng thật. Để lâu hơn thì mã của máy đã tắt vẫn chiếm chỗ, và viewer gọi
/// vào sẽ chờ vô ích thay vì được báo "không tìm thấy".
pub const DEFAULT_TTL: Duration = Duration::from_secs(30);

/// Số lần bốc mã trước khi chịu thua. Với một tỉ mã và vài nghìn máy online thì
/// trùng gần như không xảy ra; giới hạn này chỉ để vòng lặp không chạy mãi nếu
/// một ngày nào đó sổ đăng ký đầy thật.
const MAX_ALLOC_TRIES: usize = 16;

pub struct Registration {
    pub candidates: Candidates,
    /// Đường đẩy thông báo ngược xuống máy đã đăng ký.
    pub notify: tokio::sync::mpsc::UnboundedSender<FromServer>,
    pub last_seen: Instant,
}

pub struct Registry {
    peers: HashMap<PeerId, Registration>,
    ttl: Duration,
}

impl Registry {
    pub fn new(ttl: Duration) -> Self {
        Self {
            peers: HashMap::new(),
            ttl,
        }
    }

    pub fn len(&self) -> usize {
        self.peers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// Cấp mã còn trống rồi ghi đăng ký. `None` khi bốc mãi không ra mã trống.
    pub fn insert(
        &mut self,
        rng: &mut impl rand::Rng,
        candidates: Candidates,
        notify: tokio::sync::mpsc::UnboundedSender<FromServer>,
        now: Instant,
    ) -> Option<PeerId> {
        // Dọn trước khi cấp: mã của máy đã tắt phải được trả lại vòng quay, nếu
        // không thì sổ cứ phình ra theo số lần chạy chứ không theo số máy online.
        self.sweep(now);

        for _ in 0..MAX_ALLOC_TRIES {
            let id = PeerId::random(rng);
            if self.peers.contains_key(&id) {
                continue;
            }
            self.peers.insert(
                id,
                Registration {
                    candidates,
                    notify,
                    last_seen: now,
                },
            );
            return Some(id);
        }
        None
    }

    /// Ghi nhận máy vẫn còn sống. Trả về `false` nếu mã đã bị xoá — khi đó máy
    /// phải đăng ký lại và sẽ nhận mã mới.
    pub fn touch(&mut self, id: PeerId, now: Instant) -> bool {
        match self.peers.get_mut(&id) {
            Some(registration) => {
                registration.last_seen = now;
                true
            }
            None => false,
        }
    }

    pub fn remove(&mut self, id: PeerId) {
        self.peers.remove(&id);
    }

    /// Tra mã. Bản ghi quá hạn coi như không có, kể cả khi chưa kịp dọn.
    pub fn lookup(&self, id: PeerId, now: Instant) -> Option<&Registration> {
        self.peers
            .get(&id)
            .filter(|registration| !self.expired(registration, now))
    }

    /// Xoá bản ghi quá hạn, trả về số bản đã xoá.
    pub fn sweep(&mut self, now: Instant) -> usize {
        let ttl = self.ttl;
        let before = self.peers.len();
        self.peers
            .retain(|_, registration| now.duration_since(registration.last_seen) < ttl);
        before - self.peers.len()
    }

    fn expired(&self, registration: &Registration, now: Instant) -> bool {
        now.duration_since(registration.last_seen) >= self.ttl
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new(DEFAULT_TTL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn candidates(port: u16) -> Candidates {
        Candidates {
            public: SocketAddr::from(([203, 0, 113, 9], port)),
            local: vec![SocketAddr::from(([192, 168, 1, 20], port))],
        }
    }

    fn channel() -> tokio::sync::mpsc::UnboundedSender<FromServer> {
        // Giữ lại đầu nhận thì test phải nhớ nó; ở đây chỉ cần đầu gửi tồn tại.
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        std::mem::forget(rx);
        tx
    }

    #[test]
    fn cap_ma_roi_tra_lai_dung_dia_chi() {
        let mut registry = Registry::default();
        let mut rng = rand::rng();
        let now = Instant::now();

        let id = registry
            .insert(&mut rng, candidates(41000), channel(), now)
            .expect("phải cấp được mã");
        let found = registry.lookup(id, now).expect("vừa đăng ký xong");
        assert_eq!(found.candidates, candidates(41000));
    }

    #[test]
    fn qua_han_thi_coi_nhu_khong_co() {
        let mut registry = Registry::new(Duration::from_secs(30));
        let mut rng = rand::rng();
        let now = Instant::now();
        let id = registry
            .insert(&mut rng, candidates(41000), channel(), now)
            .expect("phải cấp được mã");

        // Ngay trước hạn thì vẫn còn.
        assert!(registry.lookup(id, now + Duration::from_secs(29)).is_some());
        // Qua hạn thì tra không ra, dù chưa ai chạy dọn dẹp.
        assert!(registry.lookup(id, now + Duration::from_secs(31)).is_none());
        assert_eq!(registry.len(), 1, "chưa dọn nên bản ghi vẫn nằm đó");

        assert_eq!(registry.sweep(now + Duration::from_secs(31)), 1);
        assert!(registry.is_empty());
    }

    #[test]
    fn ping_giu_dang_ky_song() {
        let mut registry = Registry::new(Duration::from_secs(30));
        let mut rng = rand::rng();
        let now = Instant::now();
        let id = registry
            .insert(&mut rng, candidates(41000), channel(), now)
            .expect("phải cấp được mã");

        // Ping lúc giây thứ 20 đẩy hạn về sau 20+30 = 50.
        assert!(registry.touch(id, now + Duration::from_secs(20)));
        assert!(registry.lookup(id, now + Duration::from_secs(45)).is_some());
        assert!(registry.lookup(id, now + Duration::from_secs(51)).is_none());
    }

    #[test]
    fn ping_ma_da_bi_xoa_thi_bao_that_bai() {
        let mut registry = Registry::default();
        let mut rng = rand::rng();
        let now = Instant::now();
        let id = registry
            .insert(&mut rng, candidates(41000), channel(), now)
            .expect("phải cấp được mã");

        registry.remove(id);
        // Máy phải biết là mình đã bị quên để còn đăng ký lại.
        assert!(!registry.touch(id, now));
    }

    #[test]
    fn ma_cua_may_da_tat_duoc_thu_hoi() {
        let mut registry = Registry::new(Duration::from_secs(30));
        let mut rng = rand::rng();
        let now = Instant::now();
        registry
            .insert(&mut rng, candidates(41000), channel(), now)
            .expect("phải cấp được mã");

        // Máy sau đăng ký lúc sổ đã đầy toàn bản ghi chết.
        let later = now + Duration::from_secs(60);
        registry
            .insert(&mut rng, candidates(41001), channel(), later)
            .expect("phải cấp được mã");
        assert_eq!(registry.len(), 1, "bản ghi cũ phải bị dọn khi cấp mã mới");
    }

    #[test]
    fn hai_may_khong_bao_gio_trung_ma() {
        let mut registry = Registry::default();
        let mut rng = rand::rng();
        let now = Instant::now();

        let mut seen = std::collections::HashSet::new();
        for port in 0..200u16 {
            let id = registry
                .insert(&mut rng, candidates(41000 + port), channel(), now)
                .expect("phải cấp được mã");
            assert!(seen.insert(id), "mã {id} bị cấp hai lần");
        }
        assert_eq!(registry.len(), 200);
    }
}
