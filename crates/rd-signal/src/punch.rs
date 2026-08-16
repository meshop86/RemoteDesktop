//! Hole punching: đục lỗ qua NAT để hai máy nối thẳng với nhau.
//!
//! NAT chặn mọi gói tin đi vào mà nó không nhận ra. Nhưng khi máy trong nhà gửi
//! một gói **ra ngoài**, NAT ghi lại "cổng 41000 đang nói chuyện với địa chỉ
//! kia" và từ đó cho gói của địa chỉ kia đi vào. Cái lỗ đó là do gói đi ra tạo
//! ra, không phải xin được.
//!
//! Nên cách duy nhất để hai máy sau NAT gặp nhau là **cùng lúc** bắn về phía
//! nhau. Gói đầu tiên của mỗi bên gần như chắc chắn bị NAT bên kia vứt (lỗ chưa
//! kịp mở), nhưng nó đã kịp mở lỗ của chính mình — nên gói sau đó qua được. Vì
//! vậy ở đây phải thử đi thử lại chứ không phải thử một lần rồi kết luận.
//!
//! Ta không phân biệt ai gọi ai: cả hai bên chạy đúng hàm này, vừa gọi ra vừa
//! nghe vào, bên nào bắt tay xong trước thì lấy kết nối đó. Muốn vậy thì cả hai
//! endpoint đều phải có cấu hình server (dùng [`rd_transport::server_endpoint`]).
//!
//! Thất bại thì thường là **NAT đối xứng**: loại NAT đổi cổng công cộng theo
//! từng đích, nên cổng mà rendezvous server nhìn thấy không phải cổng dành cho
//! peer. Trường hợp đó bắt buộc phải đi qua relay.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use rd_transport::quinn::{ClientConfig, Connection, Endpoint};
use tokio::task::JoinSet;

use crate::proto::Candidates;
use crate::{Result, SignalError};

/// Bỏ cuộc sau bao lâu. Thủng được thì thường xong trong dưới một giây; kéo dài
/// hơn 10 giây chỉ làm người dùng ngồi nhìn màn hình chờ trong vô vọng, chuyển
/// sang relay còn nhanh hơn.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Khoảng cách giữa hai đợt bắn. Đủ ngắn để bù cho lệch pha giữa hai máy, đủ
/// dài để không bị NAT coi là quét cổng mà chặn luôn.
pub const RETRY_INTERVAL: Duration = Duration::from_millis(300);

pub struct PunchConfig {
    /// Tên trong chứng chỉ của peer. Chứng chỉ tự ký nên tên này chỉ là hình
    /// thức, xác thực thật nằm ở `fingerprint`.
    pub server_name: String,
    /// Vân tay chứng chỉ của máy đã ghép cặp trước đó. `None` cho lần đầu.
    pub fingerprint: Option<[u8; 32]>,
    pub timeout: Duration,
    pub retry_interval: Duration,
}

impl Default for PunchConfig {
    fn default() -> Self {
        Self {
            server_name: "rd-peer".to_string(),
            fingerprint: None,
            timeout: DEFAULT_TIMEOUT,
            retry_interval: RETRY_INTERVAL,
        }
    }
}

/// Vừa gọi ra vừa nghe vào cho tới khi một bên bắt tay xong.
///
/// `endpoint` phải là endpoint đã dùng để nói chuyện với rendezvous server —
/// địa chỉ trong `candidates` được tính theo lỗ NAT của chính socket đó.
pub async fn punch(
    endpoint: &Endpoint,
    candidates: &Candidates,
    config: &PunchConfig,
) -> Result<Connection> {
    // `ordered()` luôn có ít nhất địa chỉ công cộng, nên không cần lo danh sách rỗng.
    let targets = candidates.ordered();
    // Chỉ nhận kết nối vào từ những IP mà server đã giới thiệu. Cổng thì bỏ
    // qua: NAT bên kia có thể đổi cổng, nhưng đổi cả IP thì đó là máy khác.
    let allowed: HashSet<IpAddr> = targets.iter().map(SocketAddr::ip).collect();

    let client_config = rd_transport::client_config(config.fingerprint)?;
    let mut attempts: JoinSet<Result<Connection>> = JoinSet::new();
    let mut ticker = tokio::time::interval(config.retry_interval);
    let deadline = tokio::time::Instant::now() + config.timeout;
    let mut rounds = 0u32;

    loop {
        tokio::select! {
            // Hết giờ.
            _ = tokio::time::sleep_until(deadline) => {
                tracing::info!(rounds, "không thủng được NAT, cần relay");
                return Err(SignalError::Timeout);
            }

            // Một đợt bắn mới về phía mọi địa chỉ ứng viên.
            _ = ticker.tick() => {
                rounds += 1;
                for target in &targets {
                    spawn_connect(&mut attempts, endpoint, &client_config, *target,
                                  &config.server_name);
                }
            }

            // Peer gọi vào được — nghĩa là lỗ của ta đã mở.
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else {
                    return Err(SignalError::Unexpected("endpoint đã đóng"));
                };
                let from = incoming.remote_address();
                if !allowed.contains(&from.ip()) {
                    // Không phải peer đang hẹn: từ chối nhẹ nhàng để bên kia
                    // biết ngay thay vì chờ timeout.
                    tracing::debug!(%from, "bỏ qua kết nối lạ trong lúc đục lỗ");
                    incoming.refuse();
                    continue;
                }
                attempts.spawn(async move {
                    Ok(incoming.await.map_err(rd_transport::TransportError::from)?)
                });
            }

            // Một trong các nỗ lực đã xong.
            Some(joined) = attempts.join_next() => {
                match joined {
                    Ok(Ok(conn)) => {
                        tracing::info!(peer = %conn.remote_address(), rounds, "đã thủng NAT");
                        // Bỏ các nỗ lực còn lại: nếu để chúng bắt tay xong thì
                        // ta có hai kết nối tới cùng một máy, và bên kia không
                        // biết ta sẽ dùng cái nào.
                        attempts.abort_all();
                        return Ok(conn);
                    }
                    // Từng nỗ lực hỏng là chuyện thường trong lúc đục lỗ; chỉ
                    // hết giờ mới là thất bại thật.
                    Ok(Err(err)) => tracing::trace!(?err, "một nỗ lực hỏng"),
                    Err(err) if err.is_panic() => {
                        std::panic::resume_unwind(err.into_panic())
                    }
                    Err(_) => {}
                }
            }
        }
    }
}

fn spawn_connect(
    attempts: &mut JoinSet<Result<Connection>>,
    endpoint: &Endpoint,
    config: &ClientConfig,
    target: SocketAddr,
    server_name: &str,
) {
    let connecting = endpoint.connect_with(config.clone(), target, server_name);
    let connecting = match connecting {
        Ok(connecting) => connecting,
        Err(err) => {
            tracing::debug!(%target, ?err, "không gọi ra được");
            return;
        }
    };
    attempts.spawn(async move {
        Ok(connecting
            .await
            .map_err(rd_transport::TransportError::from)?)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rd_transport::generate_self_signed;

    fn endpoint() -> Endpoint {
        let identity = generate_self_signed("rd-peer").expect("sinh chứng chỉ");
        rd_transport::server_endpoint("127.0.0.1:0".parse().expect("địa chỉ hợp lệ"), &identity)
            .expect("mở endpoint")
    }

    fn candidates_of(endpoint: &Endpoint) -> Candidates {
        let addr = endpoint.local_addr().expect("có địa chỉ");
        Candidates {
            public: addr,
            local: vec![addr],
        }
    }

    /// Hai bên cùng chạy `punch` thì phải gặp được nhau. Loopback không có NAT,
    /// nên test này chỉ chứng minh phần "cùng lúc gọi ra và nghe vào" không tự
    /// dẫm chân nhau — phần NAT thật phải thử trên hai mạng khác nhau.
    #[tokio::test]
    async fn hai_ben_cung_duc_lo_thi_gap_nhau() {
        let a = endpoint();
        let b = endpoint();
        let (addr_a, addr_b) = (candidates_of(&a), candidates_of(&b));
        let config = || PunchConfig {
            timeout: Duration::from_secs(5),
            ..Default::default()
        };

        let side_a = {
            let a = a.clone();
            let config = config();
            tokio::spawn(async move { punch(&a, &addr_b, &config).await })
        };
        let conn_b = punch(&b, &addr_a, &config())
            .await
            .expect("bên B phải nối được");
        let conn_a = side_a
            .await
            .expect("task không panic")
            .expect("bên A phải nối được");

        assert_eq!(conn_a.remote_address(), b.local_addr().expect("có địa chỉ"));
        assert_eq!(conn_b.remote_address(), a.local_addr().expect("có địa chỉ"));
    }

    #[tokio::test]
    async fn khong_ai_tra_loi_thi_bao_het_gio() {
        let a = endpoint();
        // Cổng không có ai nghe: bắn vào đó mãi cũng không ra kết nối.
        let nowhere = Candidates {
            public: "127.0.0.1:9".parse().expect("địa chỉ hợp lệ"),
            local: vec![],
        };
        let config = PunchConfig {
            timeout: Duration::from_millis(600),
            ..Default::default()
        };
        let err = punch(&a, &nowhere, &config)
            .await
            .expect_err("phải hết giờ");
        assert!(matches!(err, SignalError::Timeout), "{err}");
    }

    /// Đang đợi peer mà có máy lạ gọi vào thì phải từ chối, không được nhầm nó
    /// là peer. Ở đây "lạ" là một IP loopback khác — cùng máy nhưng khác IP,
    /// đúng cái mà bộ lọc theo IP phải bắt được.
    #[tokio::test]
    async fn ke_la_goi_vao_thi_bi_tu_choi() {
        let victim = endpoint();
        let stranger = endpoint();
        let peer_addr: SocketAddr = "127.0.0.2:9".parse().expect("địa chỉ hợp lệ");
        let expected = Candidates {
            public: peer_addr,
            local: vec![],
        };

        let victim_addr = victim.local_addr().expect("có địa chỉ");
        let waiting = {
            let victim = victim.clone();
            tokio::spawn(async move {
                punch(
                    &victim,
                    &expected,
                    &PunchConfig {
                        timeout: Duration::from_secs(2),
                        ..Default::default()
                    },
                )
                .await
            })
        };

        let config = rd_transport::client_config(None).expect("cấu hình client");
        let refused = stranger
            .connect_with(config, victim_addr, "rd-peer")
            .expect("gọi được")
            .await;
        assert!(refused.is_err(), "kẻ lạ lẽ ra phải bị từ chối");

        let err = waiting
            .await
            .expect("task không panic")
            .expect_err("không được coi kẻ lạ là peer");
        assert!(matches!(err, SignalError::Timeout), "{err}");
    }
}
