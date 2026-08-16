//! Sổ theo dõi các lần truyền file của một phiên.
//!
//! Một lần truyền đi qua nhiều bước cách nhau cả chục giây, mỗi bước là một
//! message rời rạc đến từ mạng: mời → đồng ý/từ chối → chạy → xong/hỏng. Sổ này
//! giữ chỗ đang đứng của từng lần truyền và **từ chối các bước sai thứ tự** —
//! đầu kia có thể gửi `FileAccept` hai lần, gửi cho một số hiệu không tồn tại,
//! hoặc báo tiến độ sau khi đã xong. Không chặn thì giao diện hiện số nhảy lung
//! tung, hoặc tệ hơn là mở thêm một stream cho lần truyền đã đóng.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Instant;

use rd_protocol::FileOffer;

/// Số dòng giữ trong danh sách. Cũ hơn và đã kết thúc thì rơi ra.
pub const MAX_ITEMS: usize = 64;

/// Số lời mời đến đang chờ ta trả lời. Đầu kia mời liên tục mà ta chưa bấm gì
/// thì đây là chặn duy nhất giữa nó và bộ nhớ của ta.
pub const MAX_PENDING_INCOMING: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Ta gửi đi.
    Outgoing,
    /// Ta nhận về.
    Incoming,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferState {
    /// Đã mời, đang chờ bên kia trả lời (hoặc chờ ta bấm với chiều nhận).
    Offered,
    Active {
        done: u64,
    },
    Done {
        /// Chỉ chiều nhận mới có đường dẫn — chiều gửi thì file vốn đã ở đây.
        path: Option<PathBuf>,
    },
    Rejected,
    Failed {
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub struct Transfer {
    pub id: u64,
    pub name: String,
    pub size: u64,
    pub direction: Direction,
    pub state: TransferState,
    /// Mốc lúc bắt đầu chạy thật, để tính tốc độ. `None` khi còn đang chờ.
    started: Option<Instant>,
    finished_in: Option<f64>,
}

impl Transfer {
    pub fn done_bytes(&self) -> u64 {
        match &self.state {
            TransferState::Active { done } => *done,
            TransferState::Done { .. } => self.size,
            _ => 0,
        }
    }

    /// Tỉ lệ hoàn thành trong [0.0, 1.0], dùng vẽ thanh tiến trình.
    pub fn fraction(&self) -> f32 {
        if self.size == 0 {
            // File rỗng vẫn là file: coi như xong ngay khi bắt đầu chạy.
            return match self.state {
                TransferState::Done { .. } | TransferState::Active { .. } => 1.0,
                _ => 0.0,
            };
        }
        (self.done_bytes() as f64 / self.size as f64).clamp(0.0, 1.0) as f32
    }

    /// Tốc độ trung bình tính từ lúc bắt đầu, byte/giây.
    pub fn bytes_per_sec(&self) -> Option<f64> {
        let elapsed = match self.finished_in {
            Some(seconds) => seconds,
            None => self.started?.elapsed().as_secs_f64(),
        };
        if elapsed <= 0.0 {
            return None;
        }
        Some(self.done_bytes() as f64 / elapsed)
    }

    pub fn is_finished(&self) -> bool {
        matches!(
            self.state,
            TransferState::Done { .. } | TransferState::Rejected | TransferState::Failed { .. }
        )
    }

    pub fn is_running(&self) -> bool {
        matches!(self.state, TransferState::Active { .. })
    }
}

/// Nửa không gian số hiệu mà bên này được cấp phát.
///
/// Hai đầu cùng gửi file thì cùng phải tự đặt số hiệu, mà không có ai làm trọng
/// tài ở giữa. Chia đôi theo bit cao nhất là xong: không cần thương lượng, và
/// nhìn số hiệu là biết ai đẻ ra nó.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdSpace {
    Host,
    Viewer,
}

impl IdSpace {
    const TAG: u64 = 1 << 63;

    fn tag(self) -> u64 {
        match self {
            Self::Host => 0,
            Self::Viewer => Self::TAG,
        }
    }

    pub fn peer(self) -> Self {
        match self {
            Self::Host => Self::Viewer,
            Self::Viewer => Self::Host,
        }
    }

    /// Số hiệu thứ `counter` của phe này.
    ///
    /// Công khai vì tầng mạng cũng phải cấp số hiệu: nó là bên đọc file và tính
    /// hash nên nó đẻ ra lời mời, còn sổ này chỉ ghi lại.
    pub fn make_id(self, counter: u64) -> u64 {
        self.tag() | (counter & !Self::TAG)
    }

    pub fn owns(self, id: u64) -> bool {
        id & Self::TAG == self.tag()
    }
}

#[derive(Debug)]
pub struct Transfers {
    items: VecDeque<Transfer>,
    space: IdSpace,
    counter: u64,
}

impl Transfers {
    pub fn new(space: IdSpace) -> Self {
        Self {
            items: VecDeque::new(),
            space,
            counter: 0,
        }
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Transfer> {
        self.items.iter()
    }

    pub fn get(&self, id: u64) -> Option<&Transfer> {
        self.items.iter().find(|item| item.id == id)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Số lần truyền đang chạy — dùng để quyết định có nên hạ bitrate video hay
    /// hiện cảnh báo "đang bận truyền file".
    pub fn running(&self) -> usize {
        self.items.iter().filter(|item| item.is_running()).count()
    }

    /// Cấp số hiệu mới cho một file ta sắp mời gửi.
    pub fn next_id(&mut self) -> u64 {
        self.counter += 1;
        self.space.make_id(self.counter)
    }

    /// Ghi nhận lời mời ta vừa gửi đi.
    pub fn offer_outgoing(&mut self, offer: &FileOffer) {
        self.insert(Transfer {
            id: offer.transfer_id,
            name: offer.name.clone(),
            size: offer.size,
            direction: Direction::Outgoing,
            state: TransferState::Offered,
            started: None,
            finished_in: None,
        });
    }

    /// Ghi nhận lời mời đầu kia gửi tới. Trả `false` nếu lời mời bị từ chối
    /// ngay ở đây (số hiệu sai phe, trùng, hoặc đang chờ quá nhiều).
    pub fn offer_incoming(&mut self, offer: &FileOffer) -> bool {
        if !self.space.peer().owns(offer.transfer_id) {
            tracing::warn!(
                id = offer.transfer_id,
                "bỏ lời mời có số hiệu lấn sang phe ta"
            );
            return false;
        }
        if self.get(offer.transfer_id).is_some() {
            tracing::warn!(id = offer.transfer_id, "bỏ lời mời trùng số hiệu");
            return false;
        }
        let pending = self
            .items
            .iter()
            .filter(|item| {
                item.direction == Direction::Incoming && item.state == TransferState::Offered
            })
            .count();
        if pending >= MAX_PENDING_INCOMING {
            tracing::warn!("bỏ lời mời: đang có quá nhiều lời mời chưa trả lời");
            return false;
        }

        self.insert(Transfer {
            id: offer.transfer_id,
            name: offer.name.clone(),
            size: offer.size,
            direction: Direction::Incoming,
            state: TransferState::Offered,
            started: None,
            finished_in: None,
        });
        true
    }

    /// Ta đồng ý nhận. Trả `true` nếu lời mời đúng là đang chờ ta.
    pub fn accept_incoming(&mut self, id: u64) -> bool {
        self.start_if(id, Direction::Incoming)
    }

    /// Đầu kia đồng ý nhận file ta mời. Trả `true` nếu hợp lệ.
    pub fn peer_accepted(&mut self, id: u64) -> bool {
        self.start_if(id, Direction::Outgoing)
    }

    fn start_if(&mut self, id: u64, direction: Direction) -> bool {
        let Some(item) = self.find_mut(id) else {
            tracing::debug!(id, "đồng ý cho một lần truyền không tồn tại");
            return false;
        };
        if item.direction != direction || item.state != TransferState::Offered {
            tracing::debug!(id, ?item.state, "đồng ý sai lúc, bỏ qua");
            return false;
        }
        item.state = TransferState::Active { done: 0 };
        item.started = Some(Instant::now());
        true
    }

    /// Một bên từ chối. Dùng chung cho cả ta từ chối lẫn đầu kia từ chối.
    pub fn reject(&mut self, id: u64) -> bool {
        let Some(item) = self.find_mut(id) else {
            return false;
        };
        if item.state != TransferState::Offered {
            return false;
        }
        item.state = TransferState::Rejected;
        true
    }

    /// Cập nhật số byte đã xong. Chỉ tiến, không lùi.
    pub fn progress(&mut self, id: u64, done: u64) {
        let Some(item) = self.find_mut(id) else {
            return;
        };
        let size = item.size;
        if let TransferState::Active { done: current } = &mut item.state {
            *current = done.min(size).max(*current);
        }
    }

    /// Đóng một lần truyền thành công.
    pub fn finish(&mut self, id: u64, path: Option<PathBuf>) -> bool {
        let Some(item) = self.find_mut(id) else {
            return false;
        };
        if !item.is_running() {
            return false;
        }
        item.finished_in = item.started.map(|at| at.elapsed().as_secs_f64());
        item.state = TransferState::Done { path };
        true
    }

    /// Đóng một lần truyền thất bại. Gọi được ở bất kỳ bước nào, kể cả khi mới
    /// mời — mạng đứt lúc nào cũng được.
    pub fn fail(&mut self, id: u64, reason: impl Into<String>) -> bool {
        let Some(item) = self.find_mut(id) else {
            return false;
        };
        if item.is_finished() {
            return false;
        }
        item.finished_in = item.started.map(|at| at.elapsed().as_secs_f64());
        item.state = TransferState::Failed {
            reason: reason.into(),
        };
        true
    }

    /// Đánh hỏng mọi lần truyền chưa xong. Gọi khi kết nối đứt: các stream đi
    /// theo kết nối, nên không cái nào chạy tiếp được.
    pub fn fail_all(&mut self, reason: &str) {
        let now = Instant::now();
        for item in self.items.iter_mut().filter(|item| !item.is_finished()) {
            item.finished_in = item.started.map(|at| (now - at).as_secs_f64());
            item.state = TransferState::Failed {
                reason: reason.to_string(),
            };
        }
    }

    /// Xoá một dòng đã kết thúc khỏi danh sách (người dùng bấm dấu x).
    pub fn dismiss(&mut self, id: u64) {
        self.items
            .retain(|item| item.id != id || !item.is_finished());
    }

    fn find_mut(&mut self, id: u64) -> Option<&mut Transfer> {
        self.items.iter_mut().find(|item| item.id == id)
    }

    /// Chèn vào cuối, đẩy dòng cũ đã kết thúc ra nếu danh sách đầy.
    fn insert(&mut self, transfer: Transfer) {
        if self.items.len() >= MAX_ITEMS {
            // Chỉ vứt dòng đã xong. Danh sách đầy toàn dòng đang chạy thì thà
            // để nó dài hơn trần còn hơn xoá mất một lần truyền còn sống.
            if let Some(index) = self.items.iter().position(Transfer::is_finished) {
                self.items.remove(index);
            }
        }
        self.items.push_back(transfer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(id: u64, size: u64) -> FileOffer {
        FileOffer {
            transfer_id: id,
            name: "bao-cao.pdf".into(),
            size,
            hash: [0u8; 32],
        }
    }

    #[test]
    fn two_sides_never_collide_on_ids() {
        let mut host = Transfers::new(IdSpace::Host);
        let mut viewer = Transfers::new(IdSpace::Viewer);
        let host_ids: Vec<_> = (0..100).map(|_| host.next_id()).collect();
        let viewer_ids: Vec<_> = (0..100).map(|_| viewer.next_id()).collect();
        for id in &host_ids {
            assert!(!viewer_ids.contains(id), "số hiệu {id} trùng giữa hai phe");
        }
        // Và mỗi phe nhận ra số hiệu của phe kia.
        assert!(IdSpace::Host.owns(host_ids[0]));
        assert!(IdSpace::Viewer.owns(viewer_ids[0]));
        assert!(!IdSpace::Host.owns(viewer_ids[0]));
    }

    #[test]
    fn happy_path_outgoing() {
        let mut t = Transfers::new(IdSpace::Host);
        let id = t.next_id();
        t.offer_outgoing(&offer(id, 1000));
        assert_eq!(t.get(id).unwrap().state, TransferState::Offered);

        assert!(t.peer_accepted(id));
        assert!(t.get(id).unwrap().is_running());
        t.progress(id, 400);
        assert!((t.get(id).unwrap().fraction() - 0.4).abs() < 1e-6);

        assert!(t.finish(id, None));
        assert_eq!(t.get(id).unwrap().state, TransferState::Done { path: None });
        assert_eq!(t.get(id).unwrap().fraction(), 1.0);
        assert_eq!(t.running(), 0);
    }

    #[test]
    fn happy_path_incoming() {
        let mut t = Transfers::new(IdSpace::Host);
        // Số hiệu do phe viewer đẻ ra.
        let id = IdSpace::Viewer.tag() | 5;
        assert!(t.offer_incoming(&offer(id, 10)));
        assert!(t.accept_incoming(id));
        t.progress(id, 10);
        assert!(t.finish(id, Some(PathBuf::from("/tmp/bao-cao.pdf"))));
        assert!(matches!(
            t.get(id).unwrap().state,
            TransferState::Done { path: Some(_) }
        ));
    }

    #[test]
    fn out_of_order_messages_are_rejected() {
        let mut t = Transfers::new(IdSpace::Host);
        let id = t.next_id();

        // Đồng ý cho một số hiệu chưa từng có.
        assert!(!t.peer_accepted(id));

        t.offer_outgoing(&offer(id, 100));
        assert!(t.peer_accepted(id));
        // Đồng ý lần hai không được reset tiến độ.
        t.progress(id, 50);
        assert!(!t.peer_accepted(id));
        assert_eq!(t.get(id).unwrap().done_bytes(), 50);

        // Từ chối sau khi đã chạy thì không có tác dụng.
        assert!(!t.reject(id));
        assert!(t.get(id).unwrap().is_running());

        // Tiến độ không lùi, không vượt kích thước.
        t.progress(id, 20);
        assert_eq!(t.get(id).unwrap().done_bytes(), 50);
        t.progress(id, 9999);
        assert_eq!(t.get(id).unwrap().done_bytes(), 100);

        // Xong rồi thì không xong lại được, và không hỏng ngược được.
        assert!(t.finish(id, None));
        assert!(!t.finish(id, None));
        assert!(!t.fail(id, "muộn rồi"));
    }

    #[test]
    fn hostile_peer_cannot_flood_or_spoof() {
        let mut t = Transfers::new(IdSpace::Host);

        // Số hiệu mang nhãn phe ta: đầu kia không được đặt.
        assert!(!t.offer_incoming(&offer(7, 1)));

        // Mời trùng số hiệu.
        let id = IdSpace::Viewer.tag() | 1;
        assert!(t.offer_incoming(&offer(id, 1)));
        assert!(!t.offer_incoming(&offer(id, 1)));

        // Mời ồ ạt mà ta chưa bấm gì.
        for n in 2..=MAX_PENDING_INCOMING as u64 {
            assert!(t.offer_incoming(&offer(IdSpace::Viewer.tag() | n, 1)));
        }
        assert!(!t.offer_incoming(&offer(IdSpace::Viewer.tag() | 999, 1)));

        // Trả lời một cái thì lại có chỗ.
        assert!(t.reject(id));
        assert!(t.offer_incoming(&offer(IdSpace::Viewer.tag() | 999, 1)));
    }

    #[test]
    fn broken_connection_fails_everything_unfinished() {
        let mut t = Transfers::new(IdSpace::Host);
        let running = t.next_id();
        let offered = t.next_id();
        let done = t.next_id();
        t.offer_outgoing(&offer(running, 10));
        t.offer_outgoing(&offer(offered, 10));
        t.offer_outgoing(&offer(done, 10));
        t.peer_accepted(running);
        t.peer_accepted(done);
        t.finish(done, None);

        t.fail_all("mất kết nối");

        assert!(matches!(
            t.get(running).unwrap().state,
            TransferState::Failed { .. }
        ));
        assert!(matches!(
            t.get(offered).unwrap().state,
            TransferState::Failed { .. }
        ));
        // Cái đã xong vẫn xong.
        assert_eq!(t.get(done).unwrap().state, TransferState::Done { path: None });
    }

    #[test]
    fn list_stays_bounded_but_keeps_running_transfers() {
        let mut t = Transfers::new(IdSpace::Host);
        // Một lần truyền đang chạy, đứng ở đầu danh sách.
        let alive = t.next_id();
        t.offer_outgoing(&offer(alive, 10));
        t.peer_accepted(alive);

        for _ in 0..MAX_ITEMS * 2 {
            let id = t.next_id();
            t.offer_outgoing(&offer(id, 1));
            t.peer_accepted(id);
            t.finish(id, None);
        }

        assert!(t.len() <= MAX_ITEMS);
        assert!(
            t.get(alive).is_some_and(Transfer::is_running),
            "dòng đang chạy bị đẩy ra khỏi danh sách"
        );
    }

    #[test]
    fn empty_file_shows_as_complete() {
        let mut t = Transfers::new(IdSpace::Host);
        let id = t.next_id();
        t.offer_outgoing(&offer(id, 0));
        assert_eq!(t.get(id).unwrap().fraction(), 0.0);
        t.peer_accepted(id);
        assert_eq!(t.get(id).unwrap().fraction(), 1.0);
        t.finish(id, None);
        assert_eq!(t.get(id).unwrap().fraction(), 1.0);
    }

    #[test]
    fn dismiss_only_removes_finished_rows() {
        let mut t = Transfers::new(IdSpace::Host);
        let id = t.next_id();
        t.offer_outgoing(&offer(id, 10));
        t.peer_accepted(id);
        t.dismiss(id);
        assert!(t.get(id).is_some(), "đang chạy mà bị xoá khỏi danh sách");
        t.finish(id, None);
        t.dismiss(id);
        assert!(t.get(id).is_none());
    }
}
