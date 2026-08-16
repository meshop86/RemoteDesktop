//! Phía máy người dùng nói chuyện với rendezvous server.
//!
//! Điểm cốt lõi của cả file này nằm ở tham số `endpoint` của [`SignalClient::connect`]:
//! phải là **đúng cái endpoint sẽ dùng để nối với máy kia**, không phải một
//! endpoint riêng.
//!
//! Lý do: NAT cấp cổng công cộng theo từng socket. Nếu ta nối tới server bằng
//! socket A rồi nối tới peer bằng socket B, địa chỉ mà server nhìn thấy là của
//! A, còn lỗ NAT cần thủng lại nằm ở B — báo cho bên kia một địa chỉ chẳng dẫn
//! tới đâu. Dùng chung một socket thì địa chỉ server quan sát được chính là lỗ
//! sẽ dùng. Đây đúng là việc của giao thức STUN, và ở đây ta có nó miễn phí.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::Duration;

use rd_transport::quinn::Endpoint;
use rd_transport::{ControlReceiver, ControlSender, Session};

use crate::proto::{Candidates, FromServer, PeerId, RelayToken, SIGNAL_ALPN, ToServer};
use crate::{Result, SignalError};

/// Chờ server trả lời bao lâu thì bỏ cuộc. Server chỉ tra một HashMap rồi trả
/// lời ngay, nên quá 10 giây là mạng đứt chứ không phải server bận.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// Nhịp ping giữ đăng ký sống. Bản ghi hết hạn sau 30 giây nên 10 giây cho phép
/// lỡ hai nhịp vẫn không sao.
pub const PING_INTERVAL: Duration = Duration::from_secs(10);

/// Một viewer đang gọi tới host, theo một trong hai đường.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// Nối thẳng: cùng lúc bắn gói về phía các địa chỉ này để đục lỗ NAT.
    Direct(Candidates),
    /// Viewer đã thử đục và không được: hẹn gặp ở relay bằng vé này.
    ViaRelay {
        relay: SocketAddr,
        token: RelayToken,
    },
}

pub struct SignalClient {
    session: Session,
    tx: ControlSender<ToServer>,
    rx: ControlReceiver<FromServer>,
    /// Lời gọi lỡ đến trong lúc ta đang chờ một trả lời khác. Không được vứt:
    /// đó là một viewer đang chờ ta bắn gói lại.
    pending: VecDeque<Call>,
    id: Option<PeerId>,
    public: Option<SocketAddr>,
}

impl SignalClient {
    /// Nối tới rendezvous server bằng `endpoint` — cùng endpoint sẽ dùng cho
    /// peer (xem ghi chú đầu file).
    ///
    /// `fingerprint` là vân tay chứng chỉ của server. `None` thì chấp nhận bất
    /// kỳ chứng chỉ nào: chỉ dùng khi tự chạy server trong mạng nhà, vì kẻ đứng
    /// giữa lúc đó giả làm server được và sẽ biết ai đang gọi ai.
    pub async fn connect(
        endpoint: &Endpoint,
        server: SocketAddr,
        server_name: &str,
        fingerprint: Option<[u8; 32]>,
    ) -> Result<Self> {
        let config = rd_transport::client_config_for(fingerprint, SIGNAL_ALPN)?;
        let conn = endpoint
            .connect_with(config, server, server_name)
            .map_err(rd_transport::TransportError::from)?
            .await
            .map_err(rd_transport::TransportError::from)?;

        let session = Session::new(conn);
        let (tx, rx) = session.open_control::<ToServer, FromServer>().await?;
        Ok(Self {
            session,
            tx,
            rx,
            pending: VecDeque::new(),
            id: None,
            public: None,
        })
    }

    /// Mã của máy này, có sau khi [`register`](Self::register) thành công.
    pub fn id(&self) -> Option<PeerId> {
        self.id
    }

    /// Địa chỉ công cộng mà server nhìn thấy — chính là lỗ NAT của ta.
    pub fn public_addr(&self) -> Option<SocketAddr> {
        self.public
    }

    pub fn server_addr(&self) -> SocketAddr {
        self.session.remote_address()
    }

    /// Xin một mã để người dùng đọc cho bên kia.
    pub async fn register(&mut self, local: Vec<SocketAddr>) -> Result<PeerId> {
        self.tx
            .send(&ToServer::Register {
                version: rd_protocol::PROTOCOL_VERSION,
                local,
            })
            .await?;
        match self.wait_reply().await? {
            FromServer::Registered { id, public } => {
                self.id = Some(id);
                self.public = Some(public);
                Ok(id)
            }
            other => Err(unexpected(other, "đang chờ Registered")),
        }
    }

    /// Hỏi địa chỉ của máy mang mã `target`.
    ///
    /// Server đồng thời báo cho máy đó biết địa chỉ của ta, nên ngay khi hàm
    /// này trả về là cả hai bên đã sẵn sàng cùng bắn gói — đừng chờ gì thêm,
    /// gọi [`punch`](crate::punch) ngay.
    pub async fn request(&mut self, target: PeerId, local: Vec<SocketAddr>) -> Result<Candidates> {
        self.tx
            .send(&ToServer::Connect {
                version: rd_protocol::PROTOCOL_VERSION,
                target,
                local,
            })
            .await?;
        match self.wait_reply().await? {
            FromServer::Peer { candidates, .. } => Ok(candidates),
            // Server không phân biệt "sai mã" với "máy đã tắt" — với người dùng
            // thì hai thứ đó là một.
            FromServer::Error { .. } => Err(SignalError::UnknownPeer(target)),
            other => Err(unexpected(other, "đang chờ Peer")),
        }
    }

    /// Xin đường vòng qua relay sau khi đục lỗ thất bại.
    ///
    /// Server đồng thời đẩy vé bạn cùng cặp xuống host, nên gọi xong là mở
    /// [`RelayLink`](crate::RelayLink) nối ngay.
    pub async fn request_relay(&mut self, target: PeerId) -> Result<(SocketAddr, RelayToken)> {
        self.tx
            .send(&ToServer::Relay {
                version: rd_protocol::PROTOCOL_VERSION,
                target,
            })
            .await?;
        match self.wait_reply().await? {
            FromServer::RelayReady { relay, token } => Ok((relay, token)),
            FromServer::Error { message } => Err(SignalError::Server(message)),
            other => Err(unexpected(other, "đang chờ RelayReady")),
        }
    }

    /// Giữ đăng ký sống và giữ lỗ NAT tới server mở.
    pub async fn ping(&mut self) -> Result<SocketAddr> {
        self.tx.send(&ToServer::Ping).await?;
        match self.wait_reply().await? {
            FromServer::Pong { public } => {
                self.public = Some(public);
                Ok(public)
            }
            FromServer::Error { message } => {
                self.id = None;
                Err(SignalError::Server(message))
            }
            other => Err(unexpected(other, "đang chờ Pong")),
        }
    }

    /// Chờ một viewer gọi tới. Host gọi hàm này trong vòng lặp.
    ///
    /// Không có timeout: host ngồi chờ cả ngày là chuyện bình thường.
    pub async fn next_caller(&mut self) -> Result<Call> {
        if let Some(call) = self.pending.pop_front() {
            return Ok(call);
        }
        loop {
            match self.rx.recv().await? {
                FromServer::Incoming { candidates } => return Ok(Call::Direct(candidates)),
                FromServer::RelayOffer { relay, token } => {
                    return Ok(Call::ViaRelay { relay, token });
                }
                FromServer::Error { message } => return Err(SignalError::Server(message)),
                // Pong lạc nhịp: bỏ qua, không phải lỗi.
                FromServer::Pong { public } => self.public = Some(public),
                other => return Err(unexpected(other, "đang chờ lời gọi")),
            }
        }
    }

    pub fn close(&self) {
        self.session.close("xong");
    }

    /// Đọc tới khi gặp trả lời cho yêu cầu vừa gửi, cất lời gọi chen ngang vào
    /// hàng đợi thay vì để rơi mất.
    async fn wait_reply(&mut self) -> Result<FromServer> {
        let deadline = tokio::time::Instant::now() + REPLY_TIMEOUT;
        loop {
            let msg = tokio::time::timeout_at(deadline, self.rx.recv())
                .await
                .map_err(|_| SignalError::Timeout)??;
            match msg {
                FromServer::Incoming { candidates } => {
                    self.pending.push_back(Call::Direct(candidates));
                }
                FromServer::RelayOffer { relay, token } => {
                    self.pending.push_back(Call::ViaRelay { relay, token });
                }
                other => return Ok(other),
            }
        }
    }
}

fn unexpected(msg: FromServer, context: &'static str) -> SignalError {
    tracing::warn!(?msg, context, "thông điệp không đúng lúc");
    SignalError::Unexpected(context)
}

/// Địa chỉ nội bộ của máy để khai với server.
///
/// Chỉ lấy IP của các card mạng thật, bỏ loopback: báo `127.0.0.1` cho máy khác
/// là bảo nó tự gọi chính nó. Cổng lấy từ endpoint vì đó mới là cổng đang mở.
pub fn local_candidates(endpoint: &Endpoint) -> Vec<SocketAddr> {
    let Ok(bound) = endpoint.local_addr() else {
        return Vec::new();
    };
    let port = bound.port();

    // Nếu endpoint đã bind vào một IP cụ thể thì đó là câu trả lời duy nhất.
    if !bound.ip().is_unspecified() {
        return vec![bound];
    }

    match local_ips() {
        Ok(ips) => ips
            .into_iter()
            .map(|ip| SocketAddr::new(ip, port))
            .collect(),
        Err(err) => {
            tracing::warn!(?err, "không đọc được địa chỉ card mạng");
            Vec::new()
        }
    }
}

/// Liệt kê IP của các card mạng đang hoạt động.
///
/// Không dùng thư viện ngoài: mẹo cũ là mở một UDP socket "nối" tới một địa chỉ
/// ngoài Internet rồi hỏi hệ điều hành nó đã chọn card nào. Không có gói tin
/// nào được gửi — `connect` trên UDP chỉ ghi lại đích — nên cách này không cần
/// mạng thật sự thông.
fn local_ips() -> std::io::Result<Vec<std::net::IpAddr>> {
    use std::net::{IpAddr, UdpSocket};

    let mut ips = Vec::new();
    // Thử cả IPv4 lẫn IPv6: máy chỉ có một trong hai vẫn ra được địa chỉ.
    for (bind, probe) in [
        ("0.0.0.0:0", "203.0.113.1:9"),
        ("[::]:0", "[2001:db8::1]:9"),
    ] {
        let Ok(socket) = UdpSocket::bind(bind) else {
            continue;
        };
        if socket.connect(probe).is_err() {
            continue;
        }
        if let Ok(addr) = socket.local_addr() {
            let ip: IpAddr = addr.ip();
            if !ip.is_loopback() && !ip.is_unspecified() && !ips.contains(&ip) {
                ips.push(ip);
            }
        }
    }
    Ok(ips)
}
