//! Relay: đường vòng khi không đục được lỗ NAT.
//!
//! Khoảng 8-10% cặp máy không thủng được, gần như luôn vì **NAT đối xứng** —
//! loại NAT cấp cổng công cộng khác nhau cho từng đích, nên cổng mà rendezvous
//! server nhìn thấy không phải cổng dành cho peer. Không có mẹo nào cứu được,
//! chỉ còn cách cho cả hai cùng gửi tới một máy thứ ba mà cả hai đều gọi ra
//! được.
//!
//! # Relay không đọc được gì
//!
//! Nó chỉ chuyển tiếp **byte thô**. QUIC + TLS vẫn chạy nguyên vẹn giữa hai
//! máy, khoá phiên hai đầu tự thoả thuận, relay không tham gia bắt tay nên
//! không có khoá. Với nó mọi gói đều là rác không đọc được.
//!
//! Điều đó có được là nhờ cách ghép ở [`RelaySocket`]: quinn tưởng nó đang nói
//! chuyện thẳng với relay, còn socket này lặng lẽ dán vé vào đầu mỗi gói gửi
//! đi. Không có tầng QUIC thứ hai, không giải mã rồi mã lại.
//!
//! ```text
//!   máy A ──[vé A][gói QUIC]──▶ relay ──[gói QUIC]──▶ máy B
//!            (mã hoá đầu-cuối A↔B, relay không có khoá)
//! ```
//!
//! # Cái giá
//!
//! Đường đi dài hơn: A → relay → B thay vì A → B. Độ trễ cộng thêm đúng bằng
//! quãng đường vòng, nên relay nên đặt gần người dùng. Đây là lý do đục lỗ
//! luôn được thử trước.

use std::collections::HashMap;
use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rd_transport::quinn::udp::{RecvMeta, Transmit};
use rd_transport::quinn::{AsyncUdpSocket, Connection, Endpoint, Runtime, UdpPoller};

use crate::proto::RelayToken;
use crate::{Result, SignalError};

/// Vé hết hạn sau bao lâu không có gói nào. Ngắn hơn TTL của sổ đăng ký vì vé
/// chỉ sống trong lúc hai máy đang nối; nối xong thì QUIC keep-alive giữ nó.
pub const SLOT_TTL: Duration = Duration::from_secs(30);

/// Trần kích thước gói mà relay chịu chuyển. Đủ cho MTU thường (1500) cộng lề.
const MAX_PACKET: usize = 2048;

// ───────────────────────── phía máy người dùng ─────────────────────────

/// Socket giả lập: quinn nghĩ nó đang nói chuyện thẳng với relay.
///
/// Hai việc duy nhất nó làm khác socket thường:
/// * **gửi** — dán vé vào đầu gói rồi mới đẩy xuống socket thật;
/// * **nhận** — không làm gì cả, vì relay gửi về gói trần (nó đã biết địa chỉ
///   ta rồi, không cần vé nữa).
#[derive(Debug)]
struct RelaySocket {
    inner: Arc<dyn AsyncUdpSocket>,
    token: RelayToken,
    relay: SocketAddr,
}

impl AsyncUdpSocket for RelaySocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Arc::clone(&self.inner).create_io_poller()
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        // Một lần cấp phát mỗi gói. Đường relay đã chậm sẵn vì đi vòng, thêm
        // một memcpy 1200 byte ở đây không đáng kể so với quãng đường mạng.
        let mut framed = Vec::with_capacity(RelayToken::LEN + transmit.contents.len());
        framed.extend_from_slice(&self.token.0);
        framed.extend_from_slice(transmit.contents);

        self.inner.try_send(&Transmit {
            destination: self.relay,
            ecn: transmit.ecn,
            contents: &framed,
            // GSO gộp nhiều gói vào một lần ghi; dán một vé vào cả cụm thì
            // relay sẽ đọc sai. `max_transmit_segments` đã tắt GSO, chỗ này
            // chỉ khẳng định lại.
            segment_size: None,
            src_ip: transmit.src_ip,
        })
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        self.inner.poll_recv(cx, bufs, meta)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        1
    }

    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}

impl RelaySocket {
    /// Gói rỗng chỉ mang vé, để relay học địa chỉ của ta.
    ///
    /// Relay không biết ta ở đâu cho tới khi nhận được gói đầu tiên. Bên ngồi
    /// chờ chưa gửi gì cả, nên nếu thiếu bước này thì gói đầu của bên gọi bị
    /// relay vứt vì không biết chuyển đi đâu.
    fn announce(&self) -> io::Result<()> {
        self.try_send(&Transmit {
            destination: self.relay,
            ecn: None,
            contents: &[],
            segment_size: None,
            src_ip: None,
        })
    }
}

/// Đường đi qua relay: một endpoint QUIC cộng cái nút bấm khai địa chỉ.
pub struct RelayLink {
    endpoint: Endpoint,
    socket: Arc<RelaySocket>,
}

/// Nhịp khai địa chỉ trong lúc chờ. Gói khai có thể mất như mọi gói UDP khác,
/// nên phải nhắc lại chứ không gửi một lần rồi tin.
const ANNOUNCE_INTERVAL: Duration = Duration::from_millis(200);

impl RelayLink {
    /// Mở một endpoint QUIC đi qua relay.
    ///
    /// Endpoint này **mới hoàn toàn**, không dùng lại endpoint đã đục lỗ: lỗ
    /// NAT cũ không còn ý nghĩa gì khi mọi gói đều đi tới cùng một địa chỉ.
    ///
    /// `identity` cho phép endpoint vừa gọi ra vừa nhận vào.
    pub fn open(
        relay: SocketAddr,
        token: RelayToken,
        identity: &rd_transport::SelfSignedIdentity,
    ) -> Result<Self> {
        // Bind theo họ địa chỉ của relay: socket IPv4 không gửi được tới relay
        // IPv6 và ngược lại.
        let bind: SocketAddr = if relay.is_ipv6() {
            "[::]:0".parse().expect("địa chỉ hợp lệ")
        } else {
            "0.0.0.0:0".parse().expect("địa chỉ hợp lệ")
        };
        let std_socket = std::net::UdpSocket::bind(bind)?;
        std_socket.set_nonblocking(true)?;

        let runtime = rd_transport::quinn::TokioRuntime;
        let inner = runtime.wrap_udp_socket(std_socket)?;
        let socket = Arc::new(RelaySocket {
            inner,
            token,
            relay,
        });

        let mut config = rd_transport::server_config(identity)?;
        // Vé chiếm 16 byte đầu mỗi gói nên gói QUIC phải nhỏ đi đúng chừng đó;
        // để nguyên 1350 thì gói ra dây thành 1366 và có đường sẽ cắt mất.
        let mut transport = rd_transport::transport_config();
        transport.initial_mtu(1200);
        transport.min_mtu(1200);
        config.transport_config(Arc::new(transport));

        let endpoint = Endpoint::new_with_abstract_socket(
            rd_transport::quinn::EndpointConfig::default(),
            Some(config),
            Arc::clone(&socket) as Arc<dyn AsyncUdpSocket>,
            Arc::new(runtime),
        )?;
        Ok(Self { endpoint, socket })
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Nối tới peer, phía chủ động gọi.
    ///
    /// Không cần khai địa chỉ riêng: gói QUIC Initial đã mang vé rồi.
    pub async fn connect(
        &self,
        server_name: &str,
        fingerprint: Option<[u8; 32]>,
        timeout: Duration,
    ) -> Result<Connection> {
        let config = rd_transport::client_config(fingerprint)?;
        let connecting = self
            .endpoint
            .connect_with(config, self.socket.relay, server_name)
            .map_err(rd_transport::TransportError::from)?;
        let conn = tokio::time::timeout(timeout, connecting)
            .await
            .map_err(|_| SignalError::Timeout)?
            .map_err(rd_transport::TransportError::from)?;
        tracing::info!(relay = %self.socket.relay, "nối qua relay xong");
        Ok(conn)
    }

    /// Chờ peer gọi tới, phía bị gọi. Vừa chờ vừa nhắc relay địa chỉ của mình.
    pub async fn accept(&self, timeout: Duration) -> Result<Connection> {
        let announcer = {
            let socket = Arc::clone(&self.socket);
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(ANNOUNCE_INTERVAL);
                loop {
                    ticker.tick().await;
                    if let Err(err) = socket.announce() {
                        tracing::debug!(?err, "khai địa chỉ với relay hỏng");
                    }
                }
            })
        };

        let result = async {
            let incoming = tokio::time::timeout(timeout, self.endpoint.accept())
                .await
                .map_err(|_| SignalError::Timeout)?
                .ok_or(SignalError::Unexpected("endpoint đã đóng"))?;
            Ok(incoming.await.map_err(rd_transport::TransportError::from)?)
        }
        .await;

        announcer.abort();
        if let Ok(conn) = &result {
            tracing::info!(peer = %conn.remote_address(), "nhận kết nối qua relay");
        }
        result
    }
}

// ───────────────────────────── phía relay ─────────────────────────────

struct Slot {
    /// Vé của bên kia trong cặp.
    peer: RelayToken,
    /// Địa chỉ học được từ gói tin gần nhất. `None` khi bên đó chưa lên tiếng.
    addr: Option<SocketAddr>,
    last_seen: Instant,
}

/// Máy chuyển tiếp. Một socket UDP, một bảng vé, không có gì khác.
pub struct RelayServer {
    socket: tokio::net::UdpSocket,
    slots: Mutex<HashMap<RelayToken, Slot>>,
}

impl RelayServer {
    pub async fn bind(addr: SocketAddr) -> Result<Self> {
        Ok(Self {
            socket: tokio::net::UdpSocket::bind(addr).await?,
            slots: Mutex::new(HashMap::new()),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub fn slots(&self) -> usize {
        self.slots.lock().len()
    }

    /// Cấp một cặp vé cho hai máy sắp gặp nhau.
    pub fn allocate(&self, rng: &mut impl rand::Rng, now: Instant) -> (RelayToken, RelayToken) {
        let (a, b) = (RelayToken::random(rng), RelayToken::random(rng));
        let mut slots = self.slots.lock();
        slots.retain(|_, slot| now.duration_since(slot.last_seen) < SLOT_TTL);
        slots.insert(
            a,
            Slot {
                peer: b,
                addr: None,
                last_seen: now,
            },
        );
        slots.insert(
            b,
            Slot {
                peer: a,
                addr: None,
                last_seen: now,
            },
        );
        (a, b)
    }

    /// Vòng lặp chuyển tiếp. Chỉ trả về khi socket hỏng.
    pub async fn run(&self) {
        let mut buf = vec![0u8; MAX_PACKET];
        loop {
            let (len, from) = match self.socket.recv_from(&mut buf).await {
                Ok(pair) => pair,
                Err(err) => {
                    tracing::error!(?err, "relay không đọc được socket");
                    return;
                }
            };
            if let Some((to, payload)) = self.route(&buf[..len], from) {
                if let Err(err) = self.socket.send_to(payload, to).await {
                    tracing::debug!(%to, ?err, "chuyển tiếp hỏng");
                }
            }
        }
    }

    /// Quyết định gói này đi đâu. Tách khỏi I/O để test được không cần mạng.
    ///
    /// Trả về `None` khi: gói quá ngắn, vé lạ, hoặc bên kia chưa lên tiếng nên
    /// chưa biết địa chỉ. Cả ba đều là chuyện bình thường lúc mới bắt đầu.
    fn route<'a>(&self, packet: &'a [u8], from: SocketAddr) -> Option<(SocketAddr, &'a [u8])> {
        let (head, payload) = packet.split_at_checked(RelayToken::LEN)?;
        let token = RelayToken::from_slice(head)?;

        let mut slots = self.slots.lock();
        let slot = slots.get_mut(&token)?;
        // Địa chỉ có thể đổi giữa chừng (NAT cấp lại cổng, máy đổi wifi sang
        // 4G): luôn tin gói mới nhất.
        slot.addr = Some(from);
        slot.last_seen = Instant::now();
        let peer_token = slot.peer;

        // Gói rỗng chỉ để khai địa chỉ, không có gì để chuyển.
        if payload.is_empty() {
            return None;
        }
        let peer_addr = slots.get(&peer_token)?.addr?;
        Some((peer_addr, payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay() -> RelayServer {
        // `bind` là async nhưng chỉ vì tokio; dựng bằng socket std cho gọn.
        let std_socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind được cổng loopback");
        std_socket
            .set_nonblocking(true)
            .expect("đặt được nonblocking");
        RelayServer {
            socket: tokio::net::UdpSocket::from_std(std_socket).expect("bọc được socket"),
            slots: Mutex::new(HashMap::new()),
        }
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn packet(token: RelayToken, body: &[u8]) -> Vec<u8> {
        let mut out = token.0.to_vec();
        out.extend_from_slice(body);
        out
    }

    #[tokio::test]
    async fn chuyen_goi_sang_dung_ben_kia() {
        let relay = relay();
        let (a, b) = relay.allocate(&mut rand::rng(), Instant::now());

        // B khai địa chỉ bằng gói rỗng, chưa có gì để chuyển.
        assert!(relay.route(&packet(b, b""), addr(2000)).is_none());

        // A gửi thật: phải sang B.
        let from_a = packet(a, b"xin chao");
        let (to, payload) = relay.route(&from_a, addr(1000)).expect("phải chuyển được");
        assert_eq!(to, addr(2000));
        assert_eq!(payload, b"xin chao");

        // Và chiều ngược lại cũng vậy.
        let from_b = packet(b, b"chao lai");
        let (to, payload) = relay.route(&from_b, addr(2000)).expect("phải chuyển được");
        assert_eq!(to, addr(1000));
        assert_eq!(payload, b"chao lai");
    }

    #[tokio::test]
    async fn ben_kia_chua_len_tieng_thi_giu_lai() {
        let relay = relay();
        let (a, _b) = relay.allocate(&mut rand::rng(), Instant::now());
        // Không biết chuyển đi đâu thì bỏ, không được đoán bừa.
        assert!(relay.route(&packet(a, b"som qua"), addr(1000)).is_none());
    }

    #[tokio::test]
    async fn ve_la_thi_khong_chuyen() {
        let relay = relay();
        let (a, b) = relay.allocate(&mut rand::rng(), Instant::now());
        relay.route(&packet(a, b""), addr(1000));
        relay.route(&packet(b, b""), addr(2000));

        let forged = RelayToken([0xAB; RelayToken::LEN]);
        assert!(
            relay
                .route(&packet(forged, b"chen ngang"), addr(3000))
                .is_none(),
            "vé không có trong bảng mà vẫn chuyển được"
        );
    }

    #[tokio::test]
    async fn goi_ngan_hon_ve_thi_bo() {
        let relay = relay();
        let (a, _b) = relay.allocate(&mut rand::rng(), Instant::now());
        // Cắt cụt vé: không được đọc lố ra ngoài mảng.
        assert!(relay.route(&a.0[..4], addr(1000)).is_none());
        assert!(relay.route(b"", addr(1000)).is_none());
    }

    #[tokio::test]
    async fn dia_chi_doi_giua_chung_thi_theo_goi_moi_nhat() {
        let relay = relay();
        let (a, b) = relay.allocate(&mut rand::rng(), Instant::now());
        relay.route(&packet(b, b""), addr(2000));
        relay.route(&packet(a, b""), addr(1000));

        // B chuyển từ wifi sang 4G: cổng công cộng đổi.
        relay.route(&packet(b, b""), addr(2999));
        let probe = packet(a, b"con do khong");
        let (to, _) = relay.route(&probe, addr(1000)).expect("phải chuyển được");
        assert_eq!(to, addr(2999), "phải theo địa chỉ mới của B");
    }

    #[tokio::test]
    async fn ve_cu_bi_thu_hoi_khi_cap_ve_moi() {
        let relay = relay();
        let now = Instant::now();
        let (a, _b) = relay.allocate(&mut rand::rng(), now);
        assert_eq!(relay.slots(), 2);

        relay.allocate(&mut rand::rng(), now + SLOT_TTL + Duration::from_secs(1));
        assert_eq!(relay.slots(), 2, "cặp vé cũ phải bị dọn");
        assert!(
            relay.route(&packet(a, b"muon qua"), addr(1000)).is_none(),
            "vé đã hết hạn mà vẫn dùng được"
        );
    }
}
