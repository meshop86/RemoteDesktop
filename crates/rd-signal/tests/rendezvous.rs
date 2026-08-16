//! Chạy trọn vòng: server thật, hai máy thật, nối được thật.
//!
//! Tất cả trên loopback nên **không có NAT** — test này không chứng minh được
//! hole punching qua NAT thật. Cái nó chứng minh là phần còn lại đúng: cấp mã,
//! tra mã, đẩy thông báo ngược xuống host, và hai bên cùng lúc gọi ra/nghe vào
//! mà không dẫm chân nhau. Phần NAT chỉ đo được bằng hai máy ở hai mạng khác
//! nhau, ghi trong task #9.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rd_signal::client::{SignalClient, local_candidates};
use rd_signal::punch::{PunchConfig, punch};
use rd_signal::server::RendezvousServer;
use rd_signal::{Call, PeerId, RelayLink, RelayServer, SignalError};
use rd_transport::quinn::Endpoint;

struct Harness {
    server: Arc<RendezvousServer>,
    addr: SocketAddr,
    fingerprint: [u8; 32],
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Dựng server trên cổng ngẫu nhiên. Giữ lại `Arc` để test soi được sổ đăng ký
/// trong lúc server đang chạy.
fn start_server() -> Harness {
    spawn_server(
        RendezvousServer::bind("127.0.0.1:0".parse().expect("địa chỉ hợp lệ"))
            .expect("mở được server"),
    )
}

/// Như trên nhưng có relay đi kèm, cho những cặp máy không đục được lỗ.
async fn start_server_with_relay() -> Harness {
    let relay = Arc::new(
        RelayServer::bind("127.0.0.1:0".parse().expect("địa chỉ hợp lệ"))
            .await
            .expect("mở được relay"),
    );
    // Loopback nên địa chỉ quảng cáo trùng địa chỉ bind; server thật thì đây là
    // IP công cộng do người vận hành khai.
    let advertised = relay.local_addr().expect("đọc được địa chỉ relay");
    spawn_server(
        RendezvousServer::bind("127.0.0.1:0".parse().expect("địa chỉ hợp lệ"))
            .expect("mở được server")
            .with_relay(relay, advertised),
    )
}

fn spawn_server(server: RendezvousServer) -> Harness {
    let server = Arc::new(server);
    let addr = server.local_addr().expect("đọc được địa chỉ");
    let fingerprint = server.fingerprint();
    let task = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { server.run().await })
    };
    Harness {
        server,
        addr,
        fingerprint,
        task,
    }
}

fn peer_endpoint() -> Endpoint {
    let identity = rd_transport::generate_self_signed("rd-peer").expect("sinh chứng chỉ");
    rd_transport::server_endpoint("127.0.0.1:0".parse().expect("địa chỉ hợp lệ"), &identity)
        .expect("mở endpoint")
}

async fn connect_signal(
    endpoint: &Endpoint,
    server: SocketAddr,
    fingerprint: [u8; 32],
) -> SignalClient {
    SignalClient::connect(endpoint, server, "rd-rendezvous", Some(fingerprint))
        .await
        .expect("nối được tới rendezvous")
}

#[tokio::test]
async fn hai_may_tim_thay_nhau_qua_ma_roi_noi_thang() {
    let rendezvous = start_server();
    let (server_addr, fingerprint) = (rendezvous.addr, rendezvous.fingerprint);

    // --- Host: đăng ký và lấy mã ---
    let host_ep = peer_endpoint();
    let mut host = connect_signal(&host_ep, server_addr, fingerprint).await;
    let code = host
        .register(local_candidates(&host_ep))
        .await
        .expect("đăng ký được");
    assert_eq!(
        host.public_addr(),
        Some(host_ep.local_addr().expect("có địa chỉ")),
        "địa chỉ server thấy phải đúng là socket của host"
    );

    // --- Host ngồi chờ, rồi đục lỗ ngay khi có người gọi ---
    let host_side = {
        let host_ep = host_ep.clone();
        tokio::spawn(async move {
            // Bọc timeout vì `next_caller` cố tình chờ vô hạn: nếu server quên
            // đẩy thông báo xuống host thì test phải đỏ, không được treo.
            let caller = tokio::time::timeout(Duration::from_secs(10), host.next_caller())
                .await
                .expect("server phải báo cho host biết có người gọi")
                .expect("phải có người gọi");
            let Call::Direct(caller) = caller else {
                panic!("viewer nối thẳng thì host phải nhận lời gọi thẳng, không phải relay");
            };
            let conn = punch(&host_ep, &caller, &PunchConfig::default())
                .await
                .expect("host phải nối được");
            (host, conn)
        })
    };

    // --- Viewer: gõ mã, hỏi địa chỉ, đục lỗ ---
    let viewer_ep = peer_endpoint();
    let mut viewer = connect_signal(&viewer_ep, server_addr, fingerprint).await;
    let host_candidates = viewer
        .request(code, local_candidates(&viewer_ep))
        .await
        .expect("tra được mã");
    let viewer_conn = punch(&viewer_ep, &host_candidates, &PunchConfig::default())
        .await
        .expect("viewer phải nối được");

    let (_host, host_conn) = host_side.await.expect("task host không panic");

    // Hai đầu phải là hai socket peer, không phải server.
    assert_eq!(
        viewer_conn.remote_address(),
        host_ep.local_addr().expect("có địa chỉ")
    );
    assert_eq!(
        host_conn.remote_address(),
        viewer_ep.local_addr().expect("có địa chỉ")
    );
    assert_ne!(viewer_conn.remote_address(), server_addr);

    // Kênh đã nối phải chở được dữ liệu thật, không chỉ bắt tay xong là thôi.
    let mut send = host_conn.open_uni().await.expect("mở được stream");
    send.write_all(b"xin chao").await.expect("ghi được");
    send.finish().expect("đóng được stream");
    let mut recv = viewer_conn.accept_uni().await.expect("nhận được stream");
    let body = recv.read_to_end(64).await.expect("đọc được");
    assert_eq!(&body, b"xin chao");
}

/// Đường vòng: viewer xin relay, server cấp cặp vé, hai máy gặp nhau ở đó.
///
/// Đây là kịch bản NAT đối xứng, chỉ khác là ở đây ta bỏ qua bước đục lỗ thay
/// vì để nó thất bại thật — loopback thì đục lúc nào cũng trúng.
#[tokio::test]
async fn khong_duc_duoc_lo_thi_di_vong_qua_relay() {
    let rendezvous = start_server_with_relay().await;
    let (server_addr, fingerprint) = (rendezvous.addr, rendezvous.fingerprint);

    let host_ep = peer_endpoint();
    let mut host = connect_signal(&host_ep, server_addr, fingerprint).await;
    let code = host
        .register(local_candidates(&host_ep))
        .await
        .expect("đăng ký được");

    // Chứng chỉ của host, viewer sẽ ghim đúng vân tay này. Ghim được nghĩa là
    // TLS chạy thẳng host↔viewer: relay không thể tráo chứng chỉ vào giữa.
    let host_identity = rd_transport::generate_self_signed("rd-peer").expect("sinh chứng chỉ");
    let host_fingerprint = host_identity.fingerprint;

    let host_side = tokio::spawn(async move {
        let call = tokio::time::timeout(Duration::from_secs(10), host.next_caller())
            .await
            .expect("server phải đẩy lời mời relay xuống host")
            .expect("phải có người gọi");
        let Call::ViaRelay { relay, token } = call else {
            panic!("viewer xin relay thì host phải nhận vé relay, không phải lời gọi thẳng");
        };
        let link = RelayLink::open(relay, token, &host_identity).expect("mở được đường relay");
        let conn = link
            .accept(Duration::from_secs(15))
            .await
            .expect("host phải nhận được kết nối qua relay");
        (host, link, conn)
    });

    let viewer_ep = peer_endpoint();
    let mut viewer = connect_signal(&viewer_ep, server_addr, fingerprint).await;
    let (relay_addr, token) = viewer.request_relay(code).await.expect("xin được vé relay");
    assert_eq!(
        relay_addr.ip(),
        server_addr.ip(),
        "relay phải ở địa chỉ đã quảng cáo"
    );

    // Endpoint riêng cho đường relay: lỗ NAT cũ vô dụng khi mọi gói đều tới relay.
    let viewer_identity = rd_transport::generate_self_signed("rd-peer").expect("sinh chứng chỉ");
    let viewer_link =
        RelayLink::open(relay_addr, token, &viewer_identity).expect("mở được đường relay");
    let viewer_conn = viewer_link
        .connect("rd-peer", Some(host_fingerprint), Duration::from_secs(15))
        .await
        .expect("viewer phải nối được qua relay");

    let (_host, _host_link, host_conn) = host_side.await.expect("task host không panic");

    // Dữ liệu thật phải chạy được cả hai chiều, không chỉ bắt tay xong là thôi.
    let mut send = host_conn.open_uni().await.expect("mở được stream");
    send.write_all(b"qua relay").await.expect("ghi được");
    send.finish().expect("đóng được stream");
    let mut recv = viewer_conn.accept_uni().await.expect("nhận được stream");
    let body = recv.read_to_end(64).await.expect("đọc được");
    assert_eq!(&body, b"qua relay");

    let mut send = viewer_conn.open_uni().await.expect("mở được stream");
    send.write_all(b"nghe ro").await.expect("ghi được");
    send.finish().expect("đóng được stream");
    let mut recv = host_conn.accept_uni().await.expect("nhận được stream");
    let body = recv.read_to_end(64).await.expect("đọc được");
    assert_eq!(&body, b"nghe ro");
}

/// Server không gắn relay thì phải nói thẳng, không để client chờ vô vọng.
#[tokio::test]
async fn server_khong_co_relay_thi_bao_ngay() {
    let rendezvous = start_server();
    let (server_addr, fingerprint) = (rendezvous.addr, rendezvous.fingerprint);

    let host_ep = peer_endpoint();
    let mut host = connect_signal(&host_ep, server_addr, fingerprint).await;
    let code = host
        .register(local_candidates(&host_ep))
        .await
        .expect("đăng ký được");

    let viewer_ep = peer_endpoint();
    let mut viewer = connect_signal(&viewer_ep, server_addr, fingerprint).await;
    let err = viewer
        .request_relay(code)
        .await
        .expect_err("server không có relay mà vẫn cấp vé");
    assert!(matches!(err, SignalError::Server(_)), "{err}");
}

#[tokio::test]
async fn ma_khong_ton_tai_thi_bao_ngay_chu_khong_bat_cho() {
    let rendezvous = start_server();
    let (server_addr, fingerprint) = (rendezvous.addr, rendezvous.fingerprint);
    let viewer_ep = peer_endpoint();
    let mut viewer = connect_signal(&viewer_ep, server_addr, fingerprint).await;

    let bogus = PeerId::new(123_456_789).expect("mã hợp lệ");
    let err = tokio::time::timeout(
        Duration::from_secs(5),
        viewer.request(bogus, local_candidates(&viewer_ep)),
    )
    .await
    .expect("không được treo")
    .expect_err("mã không có thì phải lỗi");
    assert!(
        matches!(err, SignalError::UnknownPeer(id) if id == bogus),
        "{err}"
    );
}

#[tokio::test]
async fn host_roi_di_thi_ma_bi_thu_hoi_ngay() {
    let rendezvous = start_server();
    let (server_addr, fingerprint) = (rendezvous.addr, rendezvous.fingerprint);

    let host_ep = peer_endpoint();
    let mut host = connect_signal(&host_ep, server_addr, fingerprint).await;
    let code = host
        .register(local_candidates(&host_ep))
        .await
        .expect("đăng ký được");
    assert_eq!(rendezvous.server.registered(), 1);

    host.close();
    drop(host);
    drop(host_ep);

    // Server cần một nhịp để thấy kết nối đứt. Chờ bằng vòng lặp thay vì ngủ
    // một khoảng cố định — khoảng cố định luôn là nguồn test rung trên máy chậm.
    let mut left = false;
    for _ in 0..100 {
        if rendezvous.server.registered() == 0 {
            left = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(left, "bản ghi của host đã tắt vẫn nằm trong sổ");

    // Và người gõ mã đó phải được báo là không có, chứ không ngồi chờ.
    let viewer_ep = peer_endpoint();
    let mut viewer = connect_signal(&viewer_ep, server_addr, fingerprint).await;
    let err = viewer
        .request(code, local_candidates(&viewer_ep))
        .await
        .expect_err("mã của host đã tắt vẫn tra ra được");
    assert!(matches!(err, SignalError::UnknownPeer(_)), "{err}");
}

#[tokio::test]
async fn ghim_sai_van_tay_thi_khong_noi_duoc() {
    let rendezvous = start_server();
    let endpoint = peer_endpoint();

    // Vân tay của một server khác: đúng kịch bản kẻ đứng giữa giả làm rendezvous.
    let wrong = [0u8; 32];
    let result =
        SignalClient::connect(&endpoint, rendezvous.addr, "rd-rendezvous", Some(wrong)).await;
    assert!(result.is_err(), "vân tay sai mà vẫn nối được");
}
