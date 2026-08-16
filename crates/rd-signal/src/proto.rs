//! Thông điệp giữa máy người dùng và rendezvous server.
//!
//! Chạy trên một QUIC bi stream mở một lần rồi giữ suốt phiên. Giữ stream mở là
//! có chủ ý, không phải để tiết kiệm: server cần **đẩy ngược** thông báo "có
//! người muốn kết nối" xuống host bất cứ lúc nào, mà host thì nằm sau NAT nên
//! không nhận được kết nối mới từ ngoài vào.

use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Nhãn giao thức riêng cho kênh signalling, để không lẫn với kênh video.
pub const SIGNAL_ALPN: &[u8] = b"rd-signal/1";

/// Mã máy 9 chữ số.
///
/// Chín chữ số là điểm cân bằng: đủ thưa để đoán mò không trúng (một tỉ khả
/// năng), mà vẫn đọc qua điện thoại được. Luôn đủ 9 chữ số, không có số 0 đứng
/// đầu — người dùng hay đánh rơi số 0 đầu khi gõ lại.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PeerId(u32);

impl PeerId {
    pub const MIN: u32 = 100_000_000;
    pub const MAX: u32 = 999_999_999;

    /// Sinh mã ngẫu nhiên. Dùng nguồn ngẫu nhiên của hệ điều hành chứ không
    /// phải bộ đếm: mã tăng dần thì đoán mã của người khác quá dễ.
    pub fn random(rng: &mut impl rand::Rng) -> Self {
        Self(rng.random_range(Self::MIN..=Self::MAX))
    }

    pub fn new(value: u32) -> Option<Self> {
        (Self::MIN..=Self::MAX)
            .contains(&value)
            .then_some(Self(value))
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for PeerId {
    /// Chia nhóm ba chữ số cho dễ đọc to: `123 456 789`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let digits = self.0;
        write!(
            f,
            "{:03} {:03} {:03}",
            digits / 1_000_000,
            (digits / 1_000) % 1_000,
            digits % 1_000
        )
    }
}

impl FromStr for PeerId {
    type Err = ParseIdError;

    /// Chấp nhận mọi cách người dùng gõ lại mã: có khoảng trắng, gạch ngang,
    /// hay dính liền. Người ta chép mã từ màn hình bên kia nên định dạng vào
    /// rất tuỳ hứng.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let digits: String = text
            .chars()
            .filter(|c| !matches!(c, ' ' | '-' | '.' | '\t' | '_'))
            .collect();
        if digits.len() != 9 || !digits.chars().all(|c| c.is_ascii_digit()) {
            return Err(ParseIdError);
        }
        digits
            .parse::<u32>()
            .ok()
            .and_then(Self::new)
            .ok_or(ParseIdError)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("mã máy phải gồm đúng 9 chữ số")]
pub struct ParseIdError;

/// Những địa chỉ mà bên kia có thể thử gọi tới.
///
/// Có cả địa chỉ nội bộ vì trường hợp phổ biến nhất lại là hai máy cùng một
/// mạng LAN: khi đó gọi thẳng địa chỉ nội bộ nhanh hơn và không phải nhờ NAT.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidates {
    /// Địa chỉ công cộng, do server nhìn thấy từ gói tin gửi tới.
    pub public: SocketAddr,
    /// Địa chỉ trong mạng nội bộ, do chính máy đó tự khai.
    pub local: Vec<SocketAddr>,
}

impl Candidates {
    /// Danh sách để thử lần lượt: nội bộ trước vì nếu trúng thì đường đi ngắn
    /// nhất, rồi mới tới địa chỉ công cộng.
    pub fn ordered(&self) -> Vec<SocketAddr> {
        let mut all: Vec<SocketAddr> = self.local.clone();
        if !all.contains(&self.public) {
            all.push(self.public);
        }
        all
    }
}

/// Vé đi qua relay.
///
/// Relay không biết ai là ai — nó chỉ thấy vé. Nhận gói mang vé nào thì chuyển
/// sang địa chỉ của vé bạn cùng cặp. Vé phải đủ dài để không đoán được: đoán
/// trúng thì tuy không đọc được gì (bên trong vẫn là QUIC mã hoá đầu-cuối)
/// nhưng nhét được gói rác vào đường của người khác.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RelayToken(pub [u8; RelayToken::LEN]);

impl RelayToken {
    pub const LEN: usize = 16;

    pub fn random(rng: &mut impl rand::Rng) -> Self {
        let mut bytes = [0u8; Self::LEN];
        rng.fill_bytes(&mut bytes);
        Self(bytes)
    }

    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        bytes.try_into().ok().map(Self)
    }
}

impl fmt::Debug for RelayToken {
    /// Chỉ in bốn byte đầu: vé đầy đủ nằm trong log là vé bị lộ.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "RelayToken({:02x}{:02x}{:02x}{:02x}…)",
            self.0[0], self.0[1], self.0[2], self.0[3]
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToServer {
    /// Host xin một mã và đăng ký địa chỉ của mình.
    Register {
        version: u16,
        local: Vec<SocketAddr>,
    },
    /// Viewer hỏi địa chỉ của máy mang mã này.
    Connect {
        version: u16,
        target: PeerId,
        local: Vec<SocketAddr>,
    },
    /// Đục lỗ không xong, xin đi đường vòng qua relay.
    Relay { version: u16, target: PeerId },
    /// Giữ đăng ký khỏi hết hạn, đồng thời giữ lỗ NAT tới server luôn mở.
    Ping,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FromServer {
    /// Trả lời `Register`. `public` chính là địa chỉ NAT đã cấp cho host.
    Registered {
        id: PeerId,
        public: SocketAddr,
    },
    /// Trả lời `Connect`: đây là chỗ để gọi tới.
    Peer {
        id: PeerId,
        candidates: Candidates,
    },
    /// Đẩy xuống host: có viewer đang gọi, bắn gói về phía nó ngay để mở lỗ NAT.
    Incoming {
        candidates: Candidates,
    },
    /// Trả lời `Relay`: vé của bên xin.
    RelayReady {
        relay: SocketAddr,
        token: RelayToken,
    },
    /// Đẩy xuống host: bên kia không đục lỗ được, gặp nhau ở relay bằng vé này.
    RelayOffer {
        relay: SocketAddr,
        token: RelayToken,
    },
    Pong {
        public: SocketAddr,
    },
    Error {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ma_hien_thi_theo_nhom_ba_chu_so() {
        let id = PeerId::new(123_456_789).expect("mã hợp lệ");
        assert_eq!(id.to_string(), "123 456 789");
        // Số 0 ở giữa không được nuốt mất.
        let id = PeerId::new(100_000_007).expect("mã hợp lệ");
        assert_eq!(id.to_string(), "100 000 007");
    }

    #[test]
    fn doc_lai_ma_theo_moi_kieu_go() {
        let expected = PeerId::new(123_456_789).expect("mã hợp lệ");
        for text in ["123456789", "123 456 789", "123-456-789", " 123.456.789 "] {
            assert_eq!(text.parse::<PeerId>().expect(text), expected, "{text}");
        }
    }

    #[test]
    fn ma_sai_dinh_dang_thi_tu_choi() {
        // Thiếu số, thừa số, có chữ, và số 0 đứng đầu (mã thật không bao giờ có).
        for text in ["12345678", "1234567890", "12345678a", "012345678", ""] {
            assert!(text.parse::<PeerId>().is_err(), "{text} lẽ ra phải sai");
        }
    }

    #[test]
    fn ma_ngau_nhien_luon_du_chin_chu_so() {
        let mut rng = rand::rng();
        for _ in 0..1000 {
            let id = PeerId::random(&mut rng);
            let text = id.to_string();
            assert_eq!(text.replace(' ', "").len(), 9, "{text}");
            // Vòng lại qua chuỗi phải ra đúng mã cũ.
            assert_eq!(text.parse::<PeerId>().expect(&text), id);
        }
    }

    #[test]
    fn thu_dia_chi_noi_bo_truoc() {
        let public: SocketAddr = "203.0.113.9:41000".parse().expect("địa chỉ hợp lệ");
        let local: SocketAddr = "192.168.1.20:41000".parse().expect("địa chỉ hợp lệ");
        let candidates = Candidates {
            public,
            local: vec![local],
        };
        assert_eq!(candidates.ordered(), vec![local, public]);
    }

    #[test]
    fn khong_thu_mot_dia_chi_hai_lan() {
        // Máy có IP công cộng thật: địa chỉ nội bộ và công cộng trùng nhau.
        let addr: SocketAddr = "203.0.113.9:41000".parse().expect("địa chỉ hợp lệ");
        let candidates = Candidates {
            public: addr,
            local: vec![addr],
        };
        assert_eq!(candidates.ordered(), vec![addr]);
    }
}
