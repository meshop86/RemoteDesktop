//! Rendezvous server chạy độc lập.
//!
//! ```text
//! rd-rendezvous [địa_chỉ_bind]      # mặc định 0.0.0.0:7000
//! ```
//!
//! Đây là tiến trình duy nhất cần đặt trên máy chủ có IP công cộng. Nó nhẹ:
//! chỉ giữ một HashMap và một kết nối QUIC cho mỗi máy đang online, không đụng
//! tới video hay file.
//!
//! Khi khởi động nó in ra **vân tay chứng chỉ**. Dán chuỗi đó vào cấu hình
//! client để client ghim — nếu không, kẻ đứng giữa dựng một server giả sẽ biết
//! ai đang gọi ai (dù vẫn không đọc được nội dung, vì kênh giữa hai máy mã hoá
//! đầu-cuối riêng).
//!
//! Relay chạy kèm trên **cổng kế tiếp** (mặc định 7001), cùng giao thức UDP.
//! Nó chỉ dùng cho những cặp máy không đục được lỗ NAT, nhưng phải mở sẵn cổng
//! đó trên tường lửa.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use anyhow::Context as _;
use rd_signal::RelayServer;
use rd_signal::server::RendezvousServer;

const DEFAULT_BIND: &str = "0.0.0.0:7000";

/// Biến môi trường khai IP công cộng của máy chủ, để quảng cáo địa chỉ relay.
const PUBLIC_IP_ENV: &str = "RD_PUBLIC_IP";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let bind: SocketAddr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_BIND.to_string())
        .parse()
        .context("địa chỉ bind không hợp lệ, ví dụ đúng: 0.0.0.0:7000")?;

    let server = RendezvousServer::bind(bind).context("không mở được cổng UDP")?;
    let addr = server
        .local_addr()
        .context("không đọc được địa chỉ đã bind")?;
    let fingerprint = rd_transport::fingerprint_short(&server.fingerprint());
    let server = attach_relay(server, addr).await?;

    tracing::info!(%addr, %fingerprint, "rendezvous server sẵn sàng");

    let endpoint = server.endpoint();
    tokio::spawn(async move {
        // Ctrl-C thì đóng tử tế: mỗi máy đang nối nhận được thông báo ngay thay
        // vì ngồi chờ hết idle timeout mới biết server đã tắt.
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("đang tắt");
            rd_signal::server::shutdown(&endpoint);
        }
    });

    server.run().await;
    Ok(())
}

/// Mở relay trên cổng kế tiếp và gắn vào server.
///
/// Địa chỉ quảng cáo không lấy từ socket được: máy chủ thường bind `0.0.0.0`,
/// mà báo `0.0.0.0` cho client thì client chẳng gọi được đi đâu. Ưu tiên
/// `RD_PUBLIC_IP`, không có thì dùng IP đã bind nếu nó cụ thể. Không suy ra
/// được thì chạy không relay còn hơn phát cho client một địa chỉ chết.
async fn attach_relay(
    server: RendezvousServer,
    bind: SocketAddr,
) -> anyhow::Result<RendezvousServer> {
    let public_ip: Option<IpAddr> = match std::env::var(PUBLIC_IP_ENV) {
        Ok(text) => Some(
            text.parse()
                .with_context(|| format!("{PUBLIC_IP_ENV} không phải địa chỉ IP: {text}"))?,
        ),
        Err(_) if !bind.ip().is_unspecified() => Some(bind.ip()),
        Err(_) => None,
    };

    let Some(public_ip) = public_ip else {
        tracing::warn!(
            "chạy không có relay: đặt {PUBLIC_IP_ENV}=<IP công cộng> để bật đường vòng \
             cho những máy sau NAT đối xứng"
        );
        return Ok(server);
    };

    let relay_port = bind
        .port()
        .checked_add(1)
        .context("cổng rendezvous là 65535, không còn cổng kế tiếp cho relay")?;
    let relay = RelayServer::bind(SocketAddr::new(bind.ip(), relay_port))
        .await
        .with_context(|| format!("không mở được cổng relay {relay_port}"))?;
    let advertised = SocketAddr::new(public_ip, relay_port);

    tracing::info!(%advertised, "relay sẵn sàng");
    Ok(server.with_relay(Arc::new(relay), advertised))
}
