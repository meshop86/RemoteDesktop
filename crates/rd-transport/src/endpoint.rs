//! Cấu hình QUIC hướng độ trễ thấp.
//!
//! Những tinh chỉnh quan trọng và lý do:
//!
//! * **BBR thay vì Cubic** — Cubic đẩy hàng đợi router đầy rồi mới lùi, gây
//!   bufferbloat cả trăm mili giây. BBR ước lượng băng thông và RTT tối thiểu
//!   nên giữ hàng đợi ngắn, đúng thứ video tương tác cần.
//! * **Datagram buffer lớn** — một keyframe 4K có thể vài megabyte, chia thành
//!   hàng nghìn datagram bắn ra trong vài mili giây.
//! * **initial_mtu 1280** — vừa vặn cả đường qua Tailscale/WireGuard; quinn tự
//!   dò lên cao hơn khi đường truyền cho phép.
//! * **keep_alive 2s** — giữ lỗ NAT luôn mở khi màn hình đứng yên không gửi gì.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use quinn::{
    ClientConfig, Endpoint, IdleTimeout, ServerConfig, TransportConfig, VarInt,
    congestion::BbrConfig,
    crypto::rustls::{QuicClientConfig, QuicServerConfig},
};

use crate::{
    TransportError,
    tls::{PinnedServerVerifier, SelfSignedIdentity, default_provider},
};

/// Nhãn giao thức ứng dụng, để không nhầm với QUIC service khác trên cùng cổng.
pub const ALPN: &[u8] = b"rd/1";

pub fn transport_config() -> TransportConfig {
    let mut config = TransportConfig::default();
    config.max_concurrent_bidi_streams(VarInt::from_u32(16));
    // Mỗi lần truyền file dùng một uni stream riêng.
    config.max_concurrent_uni_streams(VarInt::from_u32(128));
    config.keep_alive_interval(Some(Duration::from_secs(2)));
    config.max_idle_timeout(Some(
        IdleTimeout::try_from(Duration::from_secs(15)).expect("15s nằm trong giới hạn QUIC"),
    ));
    config.datagram_receive_buffer_size(Some(8 * 1024 * 1024));
    config.datagram_send_buffer_size(4 * 1024 * 1024);
    config.congestion_controller_factory(Arc::new(BbrConfig::default()));
    // 1280 là MTU của card mạng ảo Tailscale/WireGuard, cũng là sàn của IPv6.
    // Đoán cao hơn thì gói đầu tiên rơi im lặng ở đó và bắt tay phải chờ hết
    // một lượt truyền lại. Không mất gì: quinn vẫn tự dò lên tới 1452 sau đó.
    config.initial_mtu(1280);
    config.min_mtu(1200);
    config
}

pub fn server_config(identity: &SelfSignedIdentity) -> Result<ServerConfig, TransportError> {
    server_config_for(identity, ALPN)
}

/// Như [`server_config`] nhưng chọn được nhãn giao thức.
///
/// Rendezvous server chạy nhãn riêng: nó nói một thứ tiếng khác hẳn kênh video,
/// và nhãn khác nhau thì một client nối nhầm sẽ bị từ chối ngay lúc bắt tay chứ
/// không phải sau khi đã gửi thông điệp mà đầu kia không hiểu.
pub fn server_config_for(
    identity: &SelfSignedIdentity,
    alpn: &[u8],
) -> Result<ServerConfig, TransportError> {
    let mut crypto = rustls::ServerConfig::builder_with_provider(default_provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| TransportError::Tls(e.to_string()))?
        .with_no_client_auth()
        .with_single_cert(identity.cert_chain.clone(), identity.key.clone_key())
        .map_err(|e| TransportError::Tls(e.to_string()))?;
    crypto.alpn_protocols = vec![alpn.to_vec()];
    crypto.max_early_data_size = u32::MAX;

    let mut config = ServerConfig::with_crypto(Arc::new(
        QuicServerConfig::try_from(crypto).map_err(|e| TransportError::Tls(e.to_string()))?,
    ));
    config.transport_config(Arc::new(transport_config()));
    Ok(config)
}

pub fn client_config(
    expected_fingerprint: Option<[u8; 32]>,
) -> Result<ClientConfig, TransportError> {
    client_config_for(expected_fingerprint, ALPN)
}

/// Như [`client_config`] nhưng chọn được nhãn giao thức.
pub fn client_config_for(
    expected_fingerprint: Option<[u8; 32]>,
    alpn: &[u8],
) -> Result<ClientConfig, TransportError> {
    let mut crypto = rustls::ClientConfig::builder_with_provider(default_provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| TransportError::Tls(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(PinnedServerVerifier::new(expected_fingerprint))
        .with_no_client_auth();
    crypto.alpn_protocols = vec![alpn.to_vec()];
    // 0-RTT: lần kết nối lại với host quen sẽ có hình gần như tức thì.
    crypto.enable_early_data = true;

    let mut config = ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(crypto).map_err(|e| TransportError::Tls(e.to_string()))?,
    ));
    config.transport_config(Arc::new(transport_config()));
    Ok(config)
}

/// Endpoint của host: vừa lắng nghe viewer gọi tới, vừa gọi ra được (cần cho
/// hole punching, khi cả hai bên cùng bắn gói qua NAT).
pub fn server_endpoint(
    bind: SocketAddr,
    identity: &SelfSignedIdentity,
) -> Result<Endpoint, TransportError> {
    let mut endpoint = Endpoint::server(server_config(identity)?, bind)?;
    endpoint.set_default_client_config(client_config(None)?);
    Ok(endpoint)
}

pub fn client_endpoint(
    bind: SocketAddr,
    expected_fingerprint: Option<[u8; 32]>,
) -> Result<Endpoint, TransportError> {
    let mut endpoint = Endpoint::client(bind)?;
    endpoint.set_default_client_config(client_config(expected_fingerprint)?);
    Ok(endpoint)
}
