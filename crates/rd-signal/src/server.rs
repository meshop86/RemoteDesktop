//! Rendezvous server: nơi hai máy hẹn gặp nhau.
//!
//! Mỗi máy giữ **một** kết nối QUIC tới đây suốt phiên. Giữ kết nối vì hai lý
//! do, cả hai đều bắt buộc:
//!
//! * Địa chỉ công cộng của máy chỉ tồn tại chừng nào lỗ NAT còn mở. Kết nối đứt
//!   là lỗ đóng, địa chỉ đã báo cho bên kia thành vô dụng.
//! * Host nằm sau NAT nên không nhận được kết nối mới từ ngoài. Muốn báo cho
//!   host "có người đang gọi" thì phải đẩy xuống đường đã mở sẵn.
//!
//! Server này cố tình **không biết gì**: không mật khẩu, không nội dung, không
//! cả việc hai máy có nối được với nhau hay không. Nó chỉ nói cho mỗi bên biết
//! địa chỉ của bên kia. Ai vào được máy ai là chuyện hai máy tự thoả thuận trên
//! kênh mã hoá đầu-cuối sau đó.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rd_transport::quinn::{Endpoint, VarInt};
use rd_transport::{ControlSender, SelfSignedIdentity, Session};

use crate::proto::{Candidates, FromServer, PeerId, SIGNAL_ALPN, ToServer};
use crate::registry::{DEFAULT_TTL, Registry};
use crate::relay::RelayServer;

/// Chu kỳ quét bản ghi chết. Chỉ là lưới an toàn: đường dọn chính là lúc kết
/// nối đứt. Lưới này bắt trường hợp task xử lý kết nối chết bất thường.
const SWEEP_INTERVAL: Duration = Duration::from_secs(10);

/// Relay gắn kèm, cùng địa chỉ để quảng cáo cho client.
///
/// Địa chỉ quảng cáo tách khỏi `local_addr()` vì relay thường bind `0.0.0.0` —
/// địa chỉ đó vô nghĩa với máy ở ngoài. Người vận hành phải khai IP công cộng.
#[derive(Clone)]
struct RelayHandle {
    server: Arc<RelayServer>,
    advertised: SocketAddr,
}

pub struct RendezvousServer {
    endpoint: Endpoint,
    registry: Arc<Mutex<Registry>>,
    fingerprint: [u8; 32],
    relay: Option<RelayHandle>,
}

impl RendezvousServer {
    /// Mở server trên `bind`. Chứng chỉ tự ký, in vân tay ra để người vận hành
    /// dán vào cấu hình client — client ghim vân tay đó là hết cửa cho kẻ đứng
    /// giữa giả làm rendezvous server.
    pub fn bind(bind: SocketAddr) -> crate::Result<Self> {
        let identity: SelfSignedIdentity = rd_transport::generate_self_signed("rd-rendezvous")?;
        let fingerprint = identity.fingerprint;
        let config = rd_transport::server_config_for(&identity, SIGNAL_ALPN)?;
        let endpoint = Endpoint::server(config, bind)?;
        Ok(Self {
            endpoint,
            registry: Arc::new(Mutex::new(Registry::new(DEFAULT_TTL))),
            fingerprint,
            relay: None,
        })
    }

    /// Gắn relay để phục vụ những cặp máy không đục được lỗ NAT.
    ///
    /// Không gắn thì server vẫn chạy, chỉ là ai xin relay sẽ nhận lời từ chối
    /// thay vì một cái vé.
    pub fn with_relay(mut self, relay: Arc<RelayServer>, advertised: SocketAddr) -> Self {
        self.relay = Some(RelayHandle {
            server: relay,
            advertised,
        });
        self
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    pub fn registered(&self) -> usize {
        self.registry.lock().len()
    }

    /// Bản sao endpoint để tắt server từ nơi khác (Ctrl-C, test).
    pub fn endpoint(&self) -> Endpoint {
        self.endpoint.clone()
    }

    /// Vòng lặp chính. Chỉ trả về khi endpoint đóng.
    ///
    /// Mượn `&self` chứ không nuốt `self` để bên ngoài còn hỏi được
    /// [`registered`](Self::registered) trong lúc server đang chạy.
    pub async fn run(&self) {
        let sweeper = tokio::spawn(sweep_loop(Arc::clone(&self.registry)));
        let forwarder = self.relay.as_ref().map(|relay| {
            let server = Arc::clone(&relay.server);
            tokio::spawn(async move { server.run().await })
        });

        while let Some(incoming) = self.endpoint.accept().await {
            let registry = Arc::clone(&self.registry);
            let relay = self.relay.clone();
            tokio::spawn(async move {
                let conn = match incoming.await {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::debug!(?err, "bắt tay hỏng");
                        return;
                    }
                };
                let peer_addr = conn.remote_address();
                if let Err(err) = serve_peer(Session::new(conn), registry, relay).await {
                    tracing::debug!(%peer_addr, ?err, "phiên kết thúc");
                }
            });
        }

        sweeper.abort();
        if let Some(forwarder) = forwarder {
            forwarder.abort();
        }
    }
}

async fn sweep_loop(registry: Arc<Mutex<Registry>>) {
    let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
    loop {
        ticker.tick().await;
        let removed = registry.lock().sweep(Instant::now());
        if removed > 0 {
            tracing::debug!(removed, "dọn bản ghi quá hạn");
        }
    }
}

/// Phục vụ một máy từ lúc nối tới lúc đứt.
async fn serve_peer(
    session: Session,
    registry: Arc<Mutex<Registry>>,
    relay: Option<RelayHandle>,
) -> crate::Result<()> {
    let public = session.remote_address();
    let (mut tx, mut rx) = session.accept_control::<FromServer, ToServer>().await?;

    // Hộp thư để phần xử lý của *máy khác* đẩy thông báo sang máy này.
    let (notify_tx, mut notify_rx) = tokio::sync::mpsc::unbounded_channel::<FromServer>();
    // Mã của chính máy này, có sau khi nó Register. Viewer thì không bao giờ có.
    let mut own_id: Option<PeerId> = None;

    let result = loop {
        tokio::select! {
            // Thông điệp từ máy này gửi lên.
            incoming = rx.recv() => {
                let msg = match incoming {
                    Ok(msg) => msg,
                    Err(err) => break Err(crate::SignalError::Transport(err)),
                };
                if let Err(err) = handle(
                    msg, public, &mut own_id, &notify_tx, &registry, relay.as_ref(), &mut tx,
                ).await {
                    break Err(err);
                }
            }
            // Thông báo do máy khác đẩy tới.
            pushed = notify_rx.recv() => {
                // `None` không xảy ra vì `notify_tx` sống đến hết hàm này.
                let Some(msg) = pushed else { break Ok(()) };
                if let Err(err) = tx.send(&msg).await {
                    break Err(crate::SignalError::Transport(err));
                }
            }
        }
    };

    // Máy rời đi thì mã của nó phải được trả lại ngay, không chờ hết hạn: người
    // dùng tắt rồi mở lại phần mềm sẽ thấy mã mới, mà mã cũ thì không được để
    // treo đó dụ viewer gọi vào chỗ trống.
    if let Some(id) = own_id {
        registry.lock().remove(id);
        tracing::info!(%id, "máy rời đi");
    }
    result
}

async fn handle(
    msg: ToServer,
    public: SocketAddr,
    own_id: &mut Option<PeerId>,
    notify_tx: &tokio::sync::mpsc::UnboundedSender<FromServer>,
    registry: &Arc<Mutex<Registry>>,
    relay: Option<&RelayHandle>,
    tx: &mut ControlSender<FromServer>,
) -> crate::Result<()> {
    let reply = match msg {
        ToServer::Register { version, local } => {
            if let Some(err) = version_mismatch(version) {
                err
            } else {
                // Đăng ký lại trên cùng kết nối: trả mã cũ về trước rồi mới cấp
                // mã mới, nếu không mã cũ nằm lại trong sổ mãi mãi.
                if let Some(old) = own_id.take() {
                    registry.lock().remove(old);
                }
                let candidates = Candidates { public, local };
                let allocated = registry.lock().insert(
                    &mut rand::rng(),
                    candidates,
                    notify_tx.clone(),
                    Instant::now(),
                );
                match allocated {
                    Some(id) => {
                        *own_id = Some(id);
                        tracing::info!(%id, %public, "máy đăng ký");
                        FromServer::Registered { id, public }
                    }
                    None => FromServer::Error {
                        message: "server hết mã trống, thử lại sau".into(),
                    },
                }
            }
        }

        ToServer::Connect {
            version,
            target,
            local,
        } => {
            if let Some(err) = version_mismatch(version) {
                err
            } else {
                let viewer = Candidates { public, local };
                // Một lần khoá làm cả hai việc: lấy địa chỉ host và đẩy thông
                // báo xuống host. Tách ra hai lần thì host có thể rời đi ở giữa
                // và ta gửi cho viewer một địa chỉ vừa chết.
                let host = {
                    let registry = registry.lock();
                    registry.lookup(target, Instant::now()).map(|registration| {
                        // Kênh đóng nghĩa là host vừa rời đi; coi như không có.
                        let alive = registration
                            .notify
                            .send(FromServer::Incoming {
                                candidates: viewer.clone(),
                            })
                            .is_ok();
                        (registration.candidates.clone(), alive)
                    })
                };
                match host {
                    Some((candidates, true)) => {
                        tracing::info!(%target, viewer = %public, "ghép cặp");
                        FromServer::Peer {
                            id: target,
                            candidates,
                        }
                    }
                    _ => FromServer::Error {
                        message: format!("không có máy nào mang mã {target}"),
                    },
                }
            }
        }

        ToServer::Relay { version, target } => {
            if let Some(err) = version_mismatch(version) {
                err
            } else {
                match relay {
                    None => FromServer::Error {
                        message: "server này không có relay".into(),
                    },
                    Some(relay) => {
                        // Cấp vé trước khi tra sổ: cặp vé rẻ, và nếu host đã rời
                        // đi thì vé thừa tự hết hạn sau SLOT_TTL.
                        let (asker, host_ticket) =
                            relay.server.allocate(&mut rand::rng(), Instant::now());
                        let delivered = {
                            let registry = registry.lock();
                            registry.lookup(target, Instant::now()).map(|registration| {
                                registration
                                    .notify
                                    .send(FromServer::RelayOffer {
                                        relay: relay.advertised,
                                        token: host_ticket,
                                    })
                                    .is_ok()
                            })
                        };
                        match delivered {
                            Some(true) => {
                                tracing::info!(%target, viewer = %public, "ghép cặp qua relay");
                                FromServer::RelayReady {
                                    relay: relay.advertised,
                                    token: asker,
                                }
                            }
                            _ => FromServer::Error {
                                message: format!("không có máy nào mang mã {target}"),
                            },
                        }
                    }
                }
            }
        }

        ToServer::Ping => {
            // Mã đã bị thu hồi (server khởi động lại chẳng hạn) thì báo lỗi để
            // máy kia biết đường đăng ký lại thay vì ping vào hư không.
            match *own_id {
                Some(id) if !registry.lock().touch(id, Instant::now()) => {
                    *own_id = None;
                    FromServer::Error {
                        message: "đăng ký đã hết hạn, hãy đăng ký lại".into(),
                    }
                }
                _ => FromServer::Pong { public },
            }
        }
    };

    tx.send(&reply).await?;
    Ok(())
}

/// Client cũ hơn hay mới hơn đều bị chặn ở đây với lời nhắn đọc được, thay vì
/// để nó lỗi giải mã khó hiểu ở bước sau.
fn version_mismatch(version: u16) -> Option<FromServer> {
    (version != rd_protocol::PROTOCOL_VERSION).then(|| FromServer::Error {
        message: format!(
            "phiên bản giao thức không khớp: máy bạn {version}, server {}",
            rd_protocol::PROTOCOL_VERSION
        ),
    })
}

/// Đóng server: báo cho mọi máy đang nối biết rồi mới cắt, thay vì để chúng
/// chờ hết idle timeout mới nhận ra.
pub fn shutdown(endpoint: &Endpoint) {
    endpoint.close(VarInt::from_u32(0), b"shutdown");
}
