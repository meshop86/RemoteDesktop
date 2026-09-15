//! Máy cho xem màn hình: chụp, mã hoá, gửi đi, và nhận điều khiển về.
//!
//! Vòng ngoài chờ máy khác gọi tới theo hai đường cùng lúc — nối thẳng vào cổng
//! đang mở, hoặc qua rendezvous server. Phục vụ xong một phiên thì quay lại chờ
//! phiên sau; host không tự tắt khi viewer ngắt.
//!
//! Chỉ chụp màn hình *trong lúc có người xem*. Vừa đỡ tốn GPU, vừa là điều
//! người dùng mong đợi: đèn báo quay màn hình của hệ điều hành tắt đi đúng lúc
//! phiên kết thúc, chứ không sáng suốt ngày vì chương trình đang mở.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use rd_capture::ScreenCapturer as _;
use rd_codec::EncodedFrame;
use rd_input::{InputInjector as _, PlatformInjector, ScreenGeometry};
use rd_protocol::control::{
    ChatMessage, ClipboardText, FileChunkAck, FileOffer, HostEvent, InputEvent, MonitorInfo,
    QualityRequest, ViewerCommand,
};
use rd_session::IdSpace;
use rd_session::rate::{LinkSample, RateController};
use rd_signal::client::local_candidates;
use rd_signal::{Call, PeerId, RelayLink, SignalClient};
use rd_transport::{Session, VideoSender, load_or_create_identity, server_endpoint};
use rd_viewer::pipeline::{EncodeSource, PipelineInfo};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use super::link::{self, Hooks, Incoming, Wire};
use super::{AbortOnDrop, Context, NetEvent, SERVER_NAME, UiCommand};

/// Chờ viewer chào bao lâu trước khi coi như kết nối rác.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Nhịp giữ đăng ký với rendezvous server còn sống.
const SIGNAL_PING: Duration = Duration::from_secs(10);

/// Chờ bao lâu trước khi thử nối lại rendezvous server sau khi đứt.
const SIGNAL_RETRY: Duration = Duration::from_secs(3);

/// Chờ bao lâu giữa hai lần thử mở cổng UDP.
const BIND_RETRY: Duration = Duration::from_secs(3);

/// Không lấy được frame nào lâu tới mức này thì coi như chuỗi chụp đã tắc.
///
/// Màn hình đứng yên *không* rơi vào đây: chuỗi chụp vẫn nộp lại frame cũ theo
/// nhịp riêng của nó, nên frame đã nén vẫn đều đặn chảy ra.
const STALL_LIMIT: Duration = Duration::from_secs(5);

/// Nghỉ giữa hai lần dựng lại chuỗi chụp — đủ để driver đồ hoạ hoặc phiên đăng
/// nhập vừa đổi kịp ổn định.
const RESTART_DELAY: Duration = Duration::from_secs(1);

/// Nghỉ dài nhất giữa hai lần dựng lại.
///
/// Có trần vì hỏng lâu không có nghĩa là hỏng vĩnh viễn: máy vừa có điện lại
/// còn đang dựng màn hình, người dùng còn đang ở màn hình khoá, card đồ hoạ
/// vừa khởi động lại — tất cả đều tự hết. Bỏ cuộc hẳn là người ở xa ngồi nhìn
/// một tấm hình chết cho tới khi có ai đó ngồi vào máy, đúng thứ mà chế độ tự
/// chia sẻ sinh ra để tránh.
const RESTART_MAX: Duration = Duration::from_secs(10);

/// Câu báo sai mật khẩu. Cả bên viewer cũng dò chuỗi này để biết có nên thử
/// lại hay không, nên đổi chữ ở đây thì phải đổi cả `viewer::fatal`.
const SAI_MAT_KHAU: &str = "sai mật khẩu phiên";

/// Cho gõ sai bấy nhiêu lần rồi mới bắt chờ.
///
/// Mật khẩu mặc định chỉ có 6 chữ số, mà cổng của host thì ai dò ra cũng gọi
/// vào được. Không có cái phanh này thì một máy dò tự động cứ thế thử hết một
/// triệu khả năng — chỉ là chuyện thời gian.
const AUTH_FREE_TRIES: u32 = 5;

/// Mỗi lần sai thêm thì thời gian chờ nhân đôi, tối đa ngần này.
const AUTH_LOCK_MAX: Duration = Duration::from_secs(300);

/// Im lặng bấy lâu thì xoá tiền án: người dùng thật gõ nhầm vài lần rồi đi pha
/// cà phê không đáng bị nhớ mãi.
/// Số địa chỉ tối đa giữ trong bảng phạt trước khi dọn dòng hết hạn.
const AUTH_GATE_MAX: usize = 4096;

const AUTH_FORGET: Duration = Duration::from_secs(600);

/// Chạy liên tục bấy lâu thì coi như chuỗi đã lành, xoá bộ đếm dựng lại.
const HEALTHY_STREAK: Duration = Duration::from_secs(60);

/// Hàng đợi frame đã mã hoá giữa luồng chụp và task gửi.
///
/// Sâu 2 chứ không phải 1: một chỗ cho frame đang gửi, một chỗ cho frame kế
/// tiếp. Sâu hơn nữa chỉ tích thêm frame cũ, mà frame cũ đã hết giá trị.
const ENCODED_QUEUE: usize = 2;

/// Nhịp dò lại bitrate.
///
/// Một giây là khoảng cân bằng: ngắn hơn thì mỗi lần đo chỉ bắt được vài chục
/// gói nên tỉ lệ mất gói nhiễu tới mức vô dụng, dài hơn thì đường tụt băng
/// thông mà ta còn bơm nguyên mức cũ thêm mấy giây nữa.
const RATE_INTERVAL: Duration = Duration::from_secs(1);

pub struct HostWire;

impl Wire for HostWire {
    type Out = HostEvent;
    type In = ViewerCommand;

    fn space() -> IdSpace {
        IdSpace::Host
    }

    fn chat(message: ChatMessage) -> HostEvent {
        HostEvent::Chat(message)
    }

    fn clipboard(text: String) -> HostEvent {
        HostEvent::Clipboard(ClipboardText { text })
    }

    fn offer(offer: FileOffer) -> HostEvent {
        HostEvent::FileOffer(offer)
    }

    fn accept(transfer_id: u64) -> HostEvent {
        HostEvent::FileAccept { transfer_id }
    }

    fn reject(transfer_id: u64) -> HostEvent {
        HostEvent::FileReject { transfer_id }
    }

    fn cancel(transfer_id: u64) -> HostEvent {
        HostEvent::FileCancel { transfer_id }
    }

    fn ack(ack: FileChunkAck) -> HostEvent {
        HostEvent::FileChunkAck(ack)
    }

    fn input(_event: InputEvent) -> Option<HostEvent> {
        None
    }

    fn keyframe() -> Option<HostEvent> {
        None
    }

    fn quality(_request: QualityRequest) -> Option<HostEvent> {
        None
    }

    fn select_monitor(_monitor: u8) -> Option<HostEvent> {
        None
    }

    fn ping(_sent_us: u64) -> Option<HostEvent> {
        None
    }

    fn pong(sent_us: u64, host_us: u64) -> Option<HostEvent> {
        Some(HostEvent::Pong { sent_us, host_us })
    }

    fn classify(message: ViewerCommand) -> Incoming {
        match message {
            ViewerCommand::Chat(message) => Incoming::Chat(message),
            ViewerCommand::Clipboard(data) => Incoming::Clipboard(data.text),
            ViewerCommand::FileOffer(offer) => Incoming::Offer(offer),
            ViewerCommand::FileAccept { transfer_id } => Incoming::Accept(transfer_id),
            ViewerCommand::FileReject { transfer_id } => Incoming::Reject(transfer_id),
            ViewerCommand::FileCancel { transfer_id } => Incoming::Cancel(transfer_id),
            ViewerCommand::FileChunkAck(ack) => Incoming::Ack(ack),
            ViewerCommand::Input(event) => Incoming::Input(event),
            ViewerCommand::RequestKeyframe { .. } => Incoming::Keyframe,
            ViewerCommand::SetQuality(request) => Incoming::Quality(request),
            ViewerCommand::SelectMonitor { monitor } => Incoming::Monitor(monitor),
            ViewerCommand::Ping { sent_us } => Incoming::Ping { sent_us },
            ViewerCommand::Disconnect => Incoming::Disconnect,
            // Hello đã xử lý lúc bắt tay; Feedback chưa dùng.
            ViewerCommand::Hello { .. } | ViewerCommand::Feedback { .. } => Incoming::Ignored,
        }
    }
}

/// Chờ và phục vụ, mãi mãi.
///
/// Không có đường nào ra khỏi hàm này ngoài việc người dùng bấm dừng: host mà
/// tự tắt vì một lỗi thoáng qua thì máy ở xa nằm im tới khi có người tới bấm.
/// Vòng lặp ở đây dựng lại cả cổng UDP, nên nó qua được cả những lỗi mà một
/// phiên riêng lẻ không cứu nổi.
pub async fn run(
    ctx: Arc<Context>,
    mut commands: UnboundedReceiver<UiCommand>,
) -> anyhow::Result<()> {
    loop {
        match listen(&ctx, &mut commands).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                if ctx.stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
                ctx.events
                    .status(format!("phần chia sẻ dừng ({err}), mở lại sau vài giây"));
                ctx.events.send(NetEvent::PeerLeft);
                tokio::time::sleep(BIND_RETRY).await;
            }
        }
    }
}

async fn listen(
    ctx: &Arc<Context>,
    commands: &mut UnboundedReceiver<UiCommand>,
) -> anyhow::Result<()> {
    let identity = host_identity().context("không tạo được chứng chỉ")?;
    // Mở cổng có thể hỏng vì những lý do rồi sẽ tự hết: máy vừa khởi động lại
    // sau khi mất điện nên card mạng chưa lên, hoặc bản cũ của chính phần mềm
    // này còn giữ cổng thêm vài giây. Bỏ cuộc ngay thì máy ở xa nằm im chờ
    // người tới bấm — đúng thứ chế độ chạy ngầm sinh ra để tránh.
    let mut endpoint = None;
    for lan in 0u32.. {
        match server_endpoint(ctx.config.bind, &identity) {
            Ok(open) => {
                endpoint = Some(open);
                break;
            }
            Err(err) => {
                if ctx.stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
                if lan == 0 || lan % 10 == 0 {
                    ctx.events
                        .status(format!("chưa mở được cổng, đang chờ: {err}"));
                }
                tokio::time::sleep(BIND_RETRY).await;
            }
        }
    }
    let endpoint = endpoint.context("không mở được cổng UDP")?;
    let local = endpoint.local_addr()?;
    ctx.events
        .status(format!("đang chờ ở {local} — vân tay {}", short(&identity)));

    // Đăng ký với rendezvous server trên task riêng. Ép nó vào cùng vòng
    // `select!` với `endpoint.accept()` sẽ hỏng: đọc thông điệp từ server không
    // an toàn khi bị huỷ giữa chừng, mà `select!` huỷ nhánh thua cuộc.
    let (call_tx, mut call_rx) = unbounded_channel::<Call>();
    // Giữ một đầu gửi ở đây suốt vòng lặp. Không có rendezvous thì task đăng ký
    // không được sinh ra; thả nốt đầu gửi là `recv()` trả `None` ngay lập tức,
    // nhánh `select!` đó thắng mọi vòng và `accept()` không bao giờ tới lượt —
    // host im lặng không nhận ai cả.
    let _call_tx = call_tx.clone();
    // Cũng phải giữ tới hết vòng lặp: thả là task đăng ký bị huỷ và mã trên tay
    // người dùng thành vô nghĩa.
    let _signal = ctx.config.rendezvous.map(|server| {
        let ctx = Arc::clone(ctx);
        let endpoint = endpoint.clone();
        AbortOnDrop(tokio::spawn(async move {
            signal_loop(ctx, endpoint, server, call_tx).await;
        }))
    });

    let mut gate = AuthGate::default();
    loop {
        let (session, relayed, _keep) = tokio::select! {
            incoming = endpoint.accept() => {
                let incoming = incoming.context("endpoint đã đóng")?;
                match incoming.await {
                    Ok(conn) => (Session::new(conn), false, None),
                    Err(err) => {
                        // Nhãn giao thức không khớp là phép dò của máy khác
                        // đang hỏi "ở đây có phần mềm này không" — chuyện bình
                        // thường, không phải sự cố đáng cảnh báo.
                        if la_phep_do(&err) {
                            tracing::debug!(%err, "có máy dò xem cổng này có ai nghe");
                        } else {
                            tracing::warn!(%err, "kết nối vào hỏng lúc bắt tay");
                        }
                        continue;
                    }
                }
            }
            call = call_rx.recv() => {
                let Some(call) = call else {
                    // Task rendezvous chết hẳn; vẫn nhận nối thẳng được.
                    std::future::pending::<()>().await;
                    unreachable!()
                };
                match answer(ctx, &endpoint, &identity, call).await {
                    Ok(result) => result,
                    Err(err) => {
                        ctx.events.status(format!("không nối được với viewer: {err}"));
                        continue;
                    }
                }
            }
        };

        let peer = session.remote_address();
        // Đang trong thời gian phạt thì đóng ngay tại cửa, chưa đọc lấy một
        // thông điệp nào: mỗi lần thử của máy dò phải tốn của nó cả một lần
        // bắt tay QUIC mà không đổi lấy được thông tin gì.
        let cho = gate.con_phai_cho(peer.ip(), Instant::now());
        if !cho.is_zero() {
            tracing::warn!(%peer, ?cho, "chặn tạm vì gõ sai mật khẩu nhiều lần");
            session.close(&format!("sai mật khẩu nhiều lần, thử lại sau {cho:?}"));
            continue;
        }
        ctx.events.status(format!(
            "viewer {peer} đã nối{}",
            if relayed { " (qua relay)" } else { "" }
        ));

        // Trong lúc phục vụ, vòng này không gọi `accept()` nữa. Ai gõ cửa lúc
        // đó sẽ treo tới khi hết hạn bắt tay rồi mới biết là hỏng — mười mấy
        // giây nhìn vào màn hình trắng. Cử một task ra mở cửa nói "đang bận"
        // cho gọn; nó chết theo `serve` nhờ `AbortOnDrop`.
        let _ban = AbortOnDrop(tokio::spawn(tu_choi_vi_ban(endpoint.clone())));

        match serve(ctx, session, commands, relayed).await {
            Ok(()) => {
                gate.ghi_dung(peer.ip());
                ctx.events.status("viewer đã ngắt, quay lại chờ");
            }
            Err(err) => {
                if format!("{err:#}").contains(SAI_MAT_KHAU) {
                    gate.ghi_sai(peer.ip(), Instant::now());
                } else {
                    // Phiên chạy được rồi mới đứt: mật khẩu đã đúng.
                    gate.ghi_dung(peer.ip());
                }
                ctx.events.status(format!("phiên kết thúc: {err}"));
            }
        }
        ctx.events.send(NetEvent::PeerLeft);
        if ctx.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
    }
}

/// Kết nối hỏng vì bên kia không nói cùng nhãn giao thức.
///
/// Đúng dấu vết mà phép dò trong [`crate::probe`] để lại: TLS đi tới bước chọn
/// nhãn rồi dừng, cảnh báo 120 `no_application_protocol`.
fn la_phep_do(err: &rd_transport::quinn::ConnectionError) -> bool {
    use rd_transport::quinn::ConnectionError;
    // Bên nào phát hiện ra trước thì lỗi mang hình dạng khác: phía chủ động
    // đóng thấy `TransportError` của chính mình, phía kia thấy khung đóng gửi
    // sang. Mã thì vẫn là CRYPTO_ERROR mang theo cảnh báo TLS 120.
    let ma = match err {
        ConnectionError::ConnectionClosed(frame) => u64::from(frame.error_code),
        ConnectionError::TransportError(err) => u64::from(err.code),
        _ => return false,
    };
    ma == CRYPTO_ERROR_BASE + 120
}

/// QUIC gói cảnh báo TLS vào dải mã lỗi bắt đầu từ đây (RFC 9001, mục 4.8).
const CRYPTO_ERROR_BASE: u64 = 0x100;

/// Trả lời những viewer gõ cửa trong lúc host đang phục vụ người khác.
///
/// Chỉ có một màn hình để chia sẻ nên host phục vụ mỗi lần một viewer; việc ở
/// đây chỉ là nói ra điều đó thay vì im lặng.
async fn tu_choi_vi_ban(endpoint: rd_transport::quinn::Endpoint) {
    while let Some(incoming) = endpoint.accept().await {
        let Ok(conn) = incoming.await else { continue };
        tracing::info!(peer = %conn.remote_address(), "từ chối: đang phục vụ viewer khác");
        Session::new(conn).close("máy này đang chia sẻ cho một viewer khác");
    }
}

/// Bộ đếm lần gõ sai mật khẩu, tính riêng theo từng địa chỉ IP.
///
/// Tính theo IP chứ không tính chung: một kẻ dò mật khẩu không được phép khoá
/// cửa của mọi người còn lại.
#[derive(Default)]
struct AuthGate {
    fails: HashMap<IpAddr, (u32, Instant)>,
}

impl AuthGate {
    /// Còn phải chờ bao lâu nữa mới được thử tiếp.
    fn con_phai_cho(&mut self, ip: IpAddr, now: Instant) -> Duration {
        let Some((count, last)) = self.fails.get(&ip).copied() else {
            return Duration::ZERO;
        };
        if now.duration_since(last) >= AUTH_FORGET {
            self.fails.remove(&ip);
            return Duration::ZERO;
        }
        let Some(qua_han) = count.checked_sub(AUTH_FREE_TRIES) else {
            return Duration::ZERO;
        };
        let cho = Duration::from_secs(2u64.saturating_pow(qua_han.min(10))).min(AUTH_LOCK_MAX);
        cho.saturating_sub(now.duration_since(last))
    }

    fn ghi_sai(&mut self, ip: IpAddr, now: Instant) {
        // Một dải IPv6 cấp cho người dùng thường có hàng tỉ địa chỉ, đổi địa
        // chỉ mỗi lần thử là bảng này phình mãi. Dọn những dòng đã hết hạn
        // trước khi thêm dòng mới thì bảng chỉ lớn bằng số máy đang thực sự dò.
        if self.fails.len() >= AUTH_GATE_MAX {
            self.fails
                .retain(|_, (_, last)| now.duration_since(*last) < AUTH_FORGET);
        }
        let entry = self.fails.entry(ip).or_insert((0, now));
        // Sai sau một quãng dài im lặng thì đếm lại từ đầu.
        if now.duration_since(entry.1) >= AUTH_FORGET {
            *entry = (0, now);
        }
        entry.0 += 1;
        entry.1 = now;
    }

    fn ghi_dung(&mut self, ip: IpAddr) {
        self.fails.remove(&ip);
    }
}

/// Đáp lại một viewer đang gọi qua rendezvous server.
///
/// Giá trị thứ ba phải giữ sống bằng tuổi thọ phiên: đường relay sở hữu endpoint
/// mà kết nối đang chạy trên đó, thả sớm là đứt ngay.
async fn answer(
    ctx: &Arc<Context>,
    endpoint: &rd_transport::quinn::Endpoint,
    identity: &rd_transport::SelfSignedIdentity,
    call: Call,
) -> anyhow::Result<(Session, bool, Option<RelayLink>)> {
    match call {
        Call::Direct(candidates) => {
            ctx.events.status("viewer đang gọi, thử đục lỗ NAT");
            let config = rd_signal::punch::PunchConfig {
                server_name: SERVER_NAME.into(),
                fingerprint: None,
                ..Default::default()
            };
            let conn = rd_signal::punch::punch(endpoint, &candidates, &config).await?;
            Ok((Session::new(conn), false, None))
        }
        Call::ViaRelay { relay, token } => {
            ctx.events.status("đục lỗ không được, hẹn gặp ở relay");
            let link = RelayLink::open(relay, token, identity)?;
            let conn = link.accept(rd_signal::punch::DEFAULT_TIMEOUT).await?;
            Ok((Session::new(conn), true, Some(link)))
        }
    }
}

/// Giữ đăng ký với rendezvous server và chuyển tiếp mọi lời gọi.
async fn signal_loop(
    ctx: Arc<Context>,
    endpoint: rd_transport::quinn::Endpoint,
    server: std::net::SocketAddr,
    calls: tokio::sync::mpsc::UnboundedSender<Call>,
) {
    loop {
        match register(&ctx, &endpoint, server).await {
            Ok((mut client, id)) => {
                ctx.events.send(NetEvent::Code(id));
                ctx.events.status(format!("mã của máy này: {}", pretty(id)));
                loop {
                    // Hết `SIGNAL_PING` mà không ai gọi thì ping một cái để bản
                    // ghi khỏi hết hạn. Chờ dài hơn timeout của server là bị
                    // xoá đăng ký và mã đang cầm trên tay thành vô nghĩa.
                    match tokio::time::timeout(SIGNAL_PING, client.next_caller()).await {
                        Ok(Ok(call)) => {
                            if calls.send(call).is_err() {
                                return;
                            }
                        }
                        Ok(Err(err)) => {
                            ctx.events
                                .status(format!("mất kết nối tới rendezvous: {err}"));
                            break;
                        }
                        Err(_) => {
                            if let Err(err) = client.ping().await {
                                ctx.events.status(format!("rendezvous không đáp: {err}"));
                                break;
                            }
                        }
                    }
                }
            }
            Err(err) => ctx
                .events
                .status(format!("không đăng ký được với rendezvous: {err}")),
        }
        if ctx.stop.load(Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(SIGNAL_RETRY).await;
    }
}

async fn register(
    ctx: &Arc<Context>,
    endpoint: &rd_transport::quinn::Endpoint,
    server: std::net::SocketAddr,
) -> anyhow::Result<(SignalClient, PeerId)> {
    let mut client = SignalClient::connect(
        endpoint,
        server,
        "rd-signal",
        ctx.config.rendezvous_fingerprint,
    )
    .await?;
    let id = client.register(local_candidates(endpoint)).await?;
    Ok((client, id))
}

/// Phục vụ đúng một viewer từ lúc bắt tay tới lúc đứt.
async fn serve(
    ctx: &Arc<Context>,
    session: Session,
    commands: &mut UnboundedReceiver<UiCommand>,
    relayed: bool,
) -> anyhow::Result<()> {
    let (mut tx, mut rx) = session.accept_control::<HostEvent, ViewerCommand>().await?;

    let hello = tokio::time::timeout(HANDSHAKE_TIMEOUT, rx.recv())
        .await
        .map_err(|_| anyhow::anyhow!("viewer không chào trong {HANDSHAKE_TIMEOUT:?}"))??;
    let hello = match check_hello(ctx, &session, hello).await {
        Ok(hello) => hello,
        Err(err) => {
            // Nói rõ lý do rồi mới đóng: viewer gõ sai mật khẩu phải thấy được
            // "sai mật khẩu" chứ không phải "mất kết nối".
            tx.send(&HostEvent::Error {
                message: err.to_string(),
            })
            .await
            .ok();
            // Cho thông điệp kịp bay đi trước khi đóng.
            tokio::time::sleep(Duration::from_millis(200)).await;
            session.close("từ chối");
            return Err(err);
        }
    };

    // Danh sách màn hình chốt một lần cho cả phiên: `id` trên dây là *chỉ số*
    // trong danh sách này (0, 1, 2...) chứ không phải id của hệ điều hành —
    // CGDirectDisplayID trên macOS là số lớn, không nhét vừa `u8`.
    let displays = list_displays();
    let active = displays
        .iter()
        .position(|d| d.is_primary)
        .unwrap_or(0)
        .min(displays.len().saturating_sub(1)) as u32;
    let monitor = Arc::new(AtomicU32::new(active));

    let keyframe = Arc::new(AtomicBool::new(true));
    // Hai con số khác nhau: `ceiling` là mức người dùng đặt (viewer gửi sang),
    // `bitrate` là mức bộ dò thật sự chốt và luồng mã hoá đọc.
    let ceiling = Arc::new(AtomicU32::new(0));
    let bitrate = Arc::new(AtomicU32::new(0));
    // Trần do đường đi áp xuống; 0 là đang nối thẳng, không kẹp gì.
    let derp = Arc::new(AtomicU32::new(0));
    let rate = RateController::new(ctx.config.bitrate_kbps);
    // Chỉ dùng 4:2:2 10-bit khi *cả hai* đầu làm được: bên này mã hoá được và
    // bên kia dựng hình được.
    let allow_10bit = ctx.config.allow_10bit && hello.wants_10bit;
    let (info, mut encoded_rx) = start_encoder(
        ctx,
        rate.current(),
        allow_10bit,
        hello.codecs,
        EncoderControls {
            keyframe: keyframe.clone(),
            bitrate: bitrate.clone(),
            displays: displays.clone(),
            monitor: monitor.clone(),
        },
    )
    .await?;

    tx.send(&HostEvent::Welcome {
        version: rd_protocol::PROTOCOL_VERSION,
        host_name: ctx.config.name.clone(),
        monitors: monitor_list(&displays, &info, active),
        active_monitor: active as u8,
    })
    .await?;
    tx.send(&HostEvent::QualityChanged(QualityRequest {
        codec: info.codec,
        chroma: info.chroma,
        target_bitrate_kbps: rate.current(),
        target_fps: info.target_fps,
        max_dimension: None,
    }))
    .await?;

    ctx.events.send(NetEvent::Connected {
        peer: session.remote_address(),
        name: hello.viewer_name,
        relayed,
    });
    // Giữ tới hết phiên: thả sớm là task bị huỷ trước khi kịp báo.
    let _duong = super::bao_duong_di(
        ctx.events.clone(),
        session.remote_address().ip(),
        Some(derp.clone()),
    );
    // Có người xem rồi thì clipboard mới được phép chảy sang.
    let _clipboard = ctx.config.clipboard.session_guard();
    ctx.events.send(NetEvent::Info(Box::new(info.clone())));

    let input = spawn_injector(
        ctx,
        info.width,
        info.height,
        displays.clone(),
        monitor.clone(),
    );

    let video = AbortOnDrop(tokio::spawn({
        let session = session.clone();
        let codec = info.codec;
        let keyframe = keyframe.clone();
        async move {
            let mut sender = VideoSender::new(session, 0);
            while let Some(frame) = encoded_rx.recv().await {
                let sent = sender.send_frame(&frame.data, codec, frame.keyframe, frame.pts_us)?;
                // Không gửi được frame nào nghĩa là hàng đợi datagram đã đầy và
                // frame này rơi giữa chừng — chuỗi dự đoán bên kia đứt từ đây.
                // Phát keyframe ngay thay vì chờ viewer phát hiện rồi xin: chờ
                // như vậy mất trọn một vòng mạng, mà trong quãng đó người xem
                // nhìn một tấm hình vỡ.
                //
                // Trừ đúng trường hợp chính keyframe bị rơi: lúc đó đường đang
                // không tải nổi khung hình lớn nhất, phát tiếp một cái nữa chỉ
                // làm nghẽn thêm. Để bộ dò bitrate hạ mức xuống trước đã.
                if sent == 0 && !frame.keyframe {
                    keyframe.store(true, Ordering::Relaxed);
                }
            }
            Ok::<(), anyhow::Error>(())
        }
    }));

    // Giữ tới hết phiên: thả là bitrate đứng im ở mức đang có.
    let _rates = AbortOnDrop(tokio::spawn(rate_loop(
        session.clone(),
        rate,
        ceiling.clone(),
        derp.clone(),
        bitrate,
    )));

    let hooks = Hooks {
        input: Some(input),
        keyframe: Some(keyframe),
        bitrate: Some(ceiling),
        monitor: Some(monitor),
        clock: None,
    };

    let mut video = video;
    tokio::select! {
        result = link::run::<HostWire>(ctx.clone(), session.clone(), tx, rx, commands, hooks) => result,
        result = &mut video.0 => result?,
    }
}

/// Những gì host cần biết về viewer trước khi dựng chuỗi mã hoá.
struct Hello {
    viewer_name: String,
    wants_10bit: bool,
    codecs: Vec<rd_protocol::control::Codec>,
}

/// Kiểm tra lời chào, trả về phần dùng được nếu hợp lệ.
async fn check_hello(
    ctx: &Arc<Context>,
    session: &Session,
    hello: ViewerCommand,
) -> anyhow::Result<Hello> {
    let ViewerCommand::Hello {
        version,
        viewer_name,
        auth,
        wants_10bit,
        codecs,
    } = hello
    else {
        anyhow::bail!("viewer gửi sai thứ tự, mong đợi Hello");
    };
    if version != rd_protocol::PROTOCOL_VERSION {
        anyhow::bail!(
            "phiên bản giao thức lệch: viewer {version}, host {}",
            rd_protocol::PROTOCOL_VERSION
        );
    }
    // So sánh thường, không phải so sánh thời gian hằng định: con số này chỉ
    // dùng được cho đúng kết nối đang mở (xem `Context::auth`), nên đo được
    // thời gian cũng không suy ra được gì mang đi chỗ khác.
    let dung_mat_khau = auth == ctx.auth(session)?;
    let viewer_name = if dung_mat_khau {
        viewer_name
    } else {
        match tailnet_cua_minh(ctx, session.remote_address().ip()).await {
            Some(who) => {
                ctx.events.status(format!(
                    "tin máy {} ({}) vì cùng tài khoản Tailscale",
                    who.machine, who.login
                ));
                format!("{} · tailnet", who.machine)
            }
            None => anyhow::bail!("{SAI_MAT_KHAU}"),
        }
    };
    Ok(Hello {
        viewer_name,
        wants_10bit,
        codecs,
    })
}

/// Máy đang gọi tới có phải máy của chính mình trong tailnet không.
///
/// Chỉ hỏi khi người dùng đã bật, và chỉ với địa chỉ nằm trong dải Tailscale —
/// mọi kết nối khác đi thẳng về đường mật khẩu. Kẻ ở ngoài không giả được địa
/// chỉ này: giả rồi thì gói trả lời chạy vào tailnet chứ không về chỗ hắn, mà
/// QUIC không bắt tay xong thì chưa có lời chào nào để xét tới đây.
async fn tailnet_cua_minh(
    ctx: &Arc<Context>,
    ip: std::net::IpAddr,
) -> Option<crate::tailscale::Who> {
    if !ctx.config.trust_tailnet || !crate::tailscale::is_tailnet_ip(ip) {
        return None;
    }
    // `tailscale whois` là tiến trình con, chặn cỡ trăm mili giây.
    match tokio::task::spawn_blocking(move || crate::tailscale::same_account(ip)).await {
        Ok(Ok(who)) => Some(who),
        Ok(Err(err)) => {
            // Hiện lên giao diện chứ không chỉ ghi log: đây đúng là lúc người
            // dùng đứng hỏi "bật tin tailnet rồi mà sao vẫn đòi mật khẩu".
            ctx.events
                .status(format!("không cho vào thẳng: {err} — quay về hỏi mật khẩu"));
            None
        }
        Err(err) => {
            tracing::warn!(%err, "luồng hỏi tailscale hỏng");
            None
        }
    }
}

/// Những ô mà luồng mã hoá đọc chung với luồng điều khiển.
struct EncoderControls {
    /// Viewer xin keyframe.
    keyframe: Arc<AtomicBool>,
    /// Bitrate mới, 0 nghĩa là không đổi.
    bitrate: Arc<AtomicU32>,
    /// Danh sách màn hình lúc bắt tay; chỉ số trong danh sách này chính là `id`
    /// mà viewer gửi lên.
    displays: Vec<rd_capture::DisplayInfo>,
    /// Chỉ số màn hình viewer đang muốn xem.
    monitor: Arc<AtomicU32>,
}

/// Khởi động chụp + mã hoá trên luồng riêng.
///
/// Dựng ngay trên luồng đó chứ không dựng ở đây rồi chuyển sang: cả bộ chụp lẫn
/// bộ mã hoá đều nói chuyện với driver đồ hoạ, mà một số driver gắn trạng thái
/// vào luồng đã tạo ra đối tượng.
async fn start_encoder(
    ctx: &Arc<Context>,
    start_kbps: u32,
    allow_10bit: bool,
    viewer_codecs: Vec<rd_protocol::control::Codec>,
    controls: EncoderControls,
) -> anyhow::Result<(PipelineInfo, tokio::sync::mpsc::Receiver<EncodedFrame>)> {
    let EncoderControls {
        keyframe,
        bitrate,
        displays,
        monitor,
    } = controls;
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<anyhow::Result<PipelineInfo>>();
    let (frame_tx, frame_rx) = tokio::sync::mpsc::channel::<EncodedFrame>(ENCODED_QUEUE);
    let target_fps = ctx.config.target_fps;
    // Mức bộ dò đang chốt. Giữ lại để lúc dựng lại chuỗi còn mở đúng ở mức
    // đường truyền chịu được, chứ không quay về mức khởi điểm.
    let mut bitrate_kbps = start_kbps;
    let events = ctx.events.clone();
    // Luồng mã hoá giữ bản danh sách của mình vì nó *hỏi lại* mỗi khi dựng lại
    // chuỗi: id màn hình không cố định qua một lần card đồ hoạ khởi động lại.
    let mut displays = displays;
    let mut display_count = displays.len().max(1) as u32;
    let mut current = monitor.load(Ordering::Relaxed);

    std::thread::Builder::new()
        .name("rd-encode".into())
        .spawn(move || {
            let source = match EncodeSource::start(
                id_man_hinh(&displays, current),
                target_fps,
                bitrate_kbps,
                allow_10bit,
                &viewer_codecs,
            ) {
                Ok(source) => source,
                Err(err) => {
                    let _ = ready_tx.send(Err(err));
                    return;
                }
            };
            if ready_tx.send(Ok(source.info().clone())).is_err() {
                return;
            }

            let mut errors = 0u32;
            let mut restarts = 0u32;
            let mut last_restart = std::time::Instant::now();
            let mut first_frame = true;
            let mut last_frame = std::time::Instant::now();
            // Có lúc không có chuỗi chụp: quãng giữa hai lần dựng lại. Phiên
            // vẫn sống trong quãng ấy — chuột phím, chat, tệp đều chạy tiếp.
            let mut source = Some(source);
            // Màn hình đang xem trước khi người dùng đổi, để quay về nếu màn
            // hình mới mở không được.
            let mut man_hinh_cu: Option<u32> = None;
            loop {
                let Some(src) = source.as_mut() else {
                    // Nghỉ trước đã: dựng lại tức thì thì hỏng lại tức thì, mà
                    // vòng hỏng-dựng liên tục đốt CPU của chính máy đang chia
                    // sẻ — người xem thấy hình giật chứ không thấy hình về.
                    std::thread::sleep(cho_dung_lai(restarts));
                    if frame_tx.is_closed() {
                        break;
                    }
                    // Hỏi lại danh sách màn hình trước khi dựng. Card đồ hoạ
                    // khởi động lại, màn hình vừa bật, hay máy vừa lên sau khi
                    // mất điện đều đổi id màn hình; giữ id cũ thì mọi lần dựng
                    // lại đều trỏ vào một màn hình không còn ai nhận, và hình
                    // sẽ không bao giờ về.
                    let moi = list_displays();
                    if !moi.is_empty() {
                        displays = moi;
                        display_count = displays.len().max(1) as u32;
                        if current >= display_count {
                            current = 0;
                            monitor.store(current, Ordering::Relaxed);
                        }
                    }
                    match EncodeSource::start(
                        id_man_hinh(&displays, current),
                        target_fps,
                        bitrate_kbps,
                        allow_10bit,
                        &viewer_codecs,
                    ) {
                        Ok(mut fresh) => {
                            // Người xem đang giữ một tấm hình chết; chuỗi mới
                            // phải mở đầu bằng keyframe thì hình mới sống lại.
                            fresh.request_keyframe();
                            source = Some(fresh);
                            errors = 0;
                            first_frame = true;
                            last_frame = std::time::Instant::now();
                            last_restart = std::time::Instant::now();
                            man_hinh_cu = None;
                        }
                        Err(err) => {
                            restarts += 1;
                            // Vừa đổi màn hình mà mở không được thì quay về màn
                            // hình cũ: thà xem nhầm màn hình còn hơn mất hình.
                            if let Some(truoc) = man_hinh_cu.take() {
                                current = truoc;
                                monitor.store(truoc, Ordering::Relaxed);
                            }
                            events.status(format!(
                                "chưa mở lại được màn hình (lần {restarts}): {err}"
                            ));
                            tracing::warn!(%err, restarts, "dựng lại chuỗi chụp hỏng");
                        }
                    }
                    continue;
                };

                if keyframe.swap(false, Ordering::Relaxed) {
                    src.request_keyframe();
                }
                let kbps = bitrate.swap(0, Ordering::Relaxed);
                if kbps > 0 {
                    bitrate_kbps = kbps;
                    if let Err(err) = src.set_bitrate(kbps) {
                        tracing::warn!(%err, kbps, "không đổi được bitrate");
                    }
                }

                // Đổi màn hình cũng là dựng lại cả chuỗi, chỉ khác lý do: phân
                // giải mới, bộ mã hoá mới, nên không có đường nào nhẹ hơn.
                let want = monitor.load(Ordering::Relaxed);
                if want != current {
                    if want < display_count {
                        events.status(format!("đang chuyển sang màn hình {}", want + 1));
                        man_hinh_cu = Some(current);
                        current = want;
                        // Đây là người dùng bấm chứ không phải hỏng: dựng lại
                        // ngay, đừng bắt chờ theo nhịp giãn của lần hỏng trước.
                        restarts = 0;
                        src.stop();
                        source = None;
                        continue;
                    }
                    // Viewer xin màn hình không tồn tại (vừa rút cáp chẳng hạn).
                    monitor.store(current, Ordering::Relaxed);
                }

                // Lý do phải dựng lại cả chuỗi, nếu có.
                let broken = match src.next_encoded(Duration::from_millis(500)) {
                    Ok(Some(frame)) => {
                        errors = 0;
                        last_frame = std::time::Instant::now();
                        // Chạy êm đủ lâu thì xoá tiền án: nhịp nghỉ giãn dần là
                        // để chặn vòng hỏng-dựng-hỏng trong vài giây, chứ một
                        // phiên cả ngày thì vài lần dựng lại rải rác là thường.
                        if restarts > 0 && last_restart.elapsed() >= HEALTHY_STREAK {
                            restarts = 0;
                        }
                        if first_frame {
                            first_frame = false;
                            let info = src.info();
                            events.status(format!(
                                "đang gửi hình {}x{} {:?}",
                                info.width, info.height, info.codec
                            ));
                            // Mã hoá bằng phần mềm ăn CPU gấp nhiều lần và hình
                            // giật thấy rõ. Hay gặp nhất là lúc máy vừa khởi
                            // động: bộ mã hoá của card chưa sẵn sàng. Ghi lại để
                            // còn đối chiếu khi người dùng kêu giật.
                            if !info.hardware {
                                tracing::warn!(
                                    codec = ?info.codec,
                                    "đang mã hoá bằng phần mềm, không phải bằng card đồ hoạ"
                                );
                            }
                        }
                        // Bỏ frame khi hàng đầy thay vì chờ: chờ ở đây là để
                        // frame *sau* già đi theo, mà nó mới là frame đáng gửi.
                        match frame_tx.try_send(frame) {
                            Ok(()) => {}
                            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {}
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
                        }
                        None
                    }
                    // Không có frame là chuyện thường (màn hình đứng yên), nhưng
                    // đứng yên *quá lâu* thì không còn là màn hình đứng yên nữa:
                    // chuỗi đã tắc ở đâu đó, mà tắc thầm lặng thì người xem chỉ
                    // thấy màn hình đen và không biết vì sao.
                    Ok(None) => (last_frame.elapsed() >= STALL_LIMIT)
                        .then(|| format!("không lấy được hình nào trong {STALL_LIMIT:?}")),
                    Err(err) => {
                        errors += 1;
                        tracing::warn!(%err, errors, "frame lỗi");
                        (errors >= 30).then(|| err.to_string())
                    }
                };

                let Some(reason) = broken else {
                    continue;
                };
                // Dựng lại thay vì bỏ cuộc: capture chết vì khoá máy, đổi độ
                // phân giải hay driver đồ hoạ khởi động lại đều là chuyện tự hồi
                // phục được, và phiên vẫn đang sống — chuột phím bên kia vẫn
                // chạy, chỉ mỗi hình là mất.
                restarts += 1;
                events.status(format!("mất hình ({reason}), đang dựng lại…"));
                tracing::warn!(reason, restarts, "chuỗi chụp tắc, dựng lại");
                // Thả hẳn chuỗi cũ *trước khi* dựng chuỗi mới. Giữ cả hai một
                // lúc là hỏng: DXGI chỉ cho một bản sao màn hình mỗi tiến trình,
                // nên bản mới sẽ hỏng ngay từ đầu.
                src.stop();
                source = None;
            }
            if let Some(mut src) = source {
                src.stop();
            }
        })
        .context("không tạo được luồng mã hoá")?;

    let info = ready_rx
        .await
        .context("luồng mã hoá chết khi khởi động")??;
    tracing::info!(?info, "chuỗi chụp + mã hoá đã chạy");
    Ok((info, frame_rx))
}

/// Dò bitrate suốt phiên.
///
/// Mỗi nhịp hỏi QUIC xem đường truyền đang thế nào rồi để [`RateController`]
/// quyết mức mới; mức đó ghi vào ô nhớ mà luồng mã hoá đọc.
///
/// Con số viewer gửi sang được hiểu là **trần** chứ không phải mức phát: người
/// dùng nói "tối đa 30 Mbps", còn đường truyền có tải nổi 30 Mbps hay không thì
/// chỉ đo mới biết.
async fn rate_loop(
    session: Session,
    mut rate: RateController,
    ceiling: Arc<AtomicU32>,
    derp: Arc<AtomicU32>,
    bitrate: Arc<AtomicU32>,
) {
    let mut ticker = tokio::time::interval(RATE_INTERVAL);
    // Máy ngủ dậy thì bỏ qua các nhịp đã lỡ: dồn chúng lại chỉ tạo một chuỗi
    // phép đo trên cùng một ảnh chụp đường truyền.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;

        let requested = ceiling.swap(0, Ordering::Relaxed);
        let forced = (requested > 0)
            .then(|| rate.set_ceiling(requested))
            .flatten();
        // Đọc chứ không lấy đi: trần của đường vòng còn hiệu lực tới khi task
        // dò đường báo ngược lại, không phải chỉ một nhịp.
        let kbps = derp.load(Ordering::Relaxed);
        let forced = rate
            .set_relay_ceiling((kbps > 0).then_some(kbps))
            .or(forced);

        let stats = session.link_stats();
        let probed = rate.update(LinkSample {
            rtt: stats.rtt,
            lost_packets: stats.lost_packets,
            sent_packets: stats.sent_packets,
        });

        // Bộ dò chạy *sau* khi trần đã đổi nên nó nhìn con số đã kẹp rồi; ý nó
        // mới hơn, lấy nó trước.
        if let Some(kbps) = probed.or(forced) {
            tracing::debug!(
                kbps,
                rtt_ms = stats.rtt.as_millis(),
                loss = stats.loss_ratio(),
                "đổi bitrate theo đường truyền"
            );
            bitrate.store(kbps, Ordering::Relaxed);
        }
    }
}

/// Luồng bơm input. Trả về đầu gửi để kênh điều khiển đẩy sự kiện vào.
///
/// Luồng riêng vì `inject` là lời gọi chặn xuống hệ điều hành: gọi thẳng trong
/// task async sẽ chặn cả executor, kéo theo cả video đứng hình.
fn spawn_injector(
    ctx: &Arc<Context>,
    width: u32,
    height: u32,
    displays: Vec<rd_capture::DisplayInfo>,
    monitor: Arc<AtomicU32>,
) -> std::sync::mpsc::Sender<InputEvent> {
    let (tx, rx) = std::sync::mpsc::channel::<InputEvent>();
    let events = ctx.events.clone();

    std::thread::Builder::new()
        .name("rd-inject".into())
        .spawn(move || {
            let mut current = monitor.load(Ordering::Relaxed);
            // Kích thước thật của màn hình host, không phải kích thước frame:
            // trên màn Retina hai số này lệch nhau đúng hệ số scale, và lấy
            // nhầm thì chuột chỉ chạy trong một phần tư màn hình.
            let fallback = || match host_geometry() {
                Ok(geometry) => geometry,
                Err(err) => {
                    tracing::warn!(%err, "không đọc được kích thước màn hình, dùng cỡ frame");
                    ScreenGeometry::new(width as f32, height as f32)
                }
            };
            let geometry = geometry_of(&displays, current).unwrap_or_else(fallback);
            let mut injector = match PlatformInjector::open(geometry) {
                Ok(injector) => injector,
                Err(err) => {
                    events.status(format!("không bơm được input: {err}"));
                    return;
                }
            };

            let mut errors = 0u32;
            while let Ok(event) = rx.recv() {
                // Đổi màn hình thì gốc toạ độ đổi theo, nếu không thì chuột vẫn
                // rơi vào màn hình cũ trong khi hình đã là màn hình khác.
                let want = monitor.load(Ordering::Relaxed);
                if want != current {
                    current = want;
                    if let Some(geometry) = geometry_of(&displays, current) {
                        injector.set_geometry(geometry);
                    }
                }
                if let Err(err) = injector.inject(&event) {
                    errors += 1;
                    if errors <= 3 {
                        tracing::warn!(%err, ?event, "bơm sự kiện thất bại");
                    }
                }
            }
            // Kênh đóng nghĩa là phiên hết. Nhả sạch phím đang giữ, không thì
            // người ngồi ở máy host thấy Ctrl kẹt vĩnh viễn.
            if let Err(err) = injector.release_all() {
                tracing::warn!(%err, "không nhả hết phím");
            }
        })
        .expect("tạo được luồng bơm input");

    tx
}

/// Chỉ số trong danh sách → id mà tầng capture hiểu. Ngoài tầm thì trả 0, tức
/// là "để capture tự chọn màn hình mặc định".
fn id_man_hinh(displays: &[rd_capture::DisplayInfo], index: u32) -> u32 {
    displays.get(index as usize).map(|d| d.id).unwrap_or(0)
}

/// Nghỉ bao lâu trước lần dựng lại thứ `lan`.
///
/// Lần đầu thử ngay: phần lớn cú chết là nhất thời (khoá máy, đổi độ phân
/// giải) và dựng lại là xong. Hỏng thêm thì giãn dần, vì hỏng thêm nghĩa là
/// máy đang bận với thứ gì đó lâu hơn — vừa khởi động, hay card đồ hoạ đang
/// dựng lại driver.
fn cho_dung_lai(lan: u32) -> Duration {
    if lan == 0 {
        return Duration::ZERO;
    }
    (RESTART_DELAY * 2u32.saturating_pow((lan - 1).min(4))).min(RESTART_MAX)
}

/// Danh sách màn hình của máy này, hoặc một mục giả nếu không hỏi được.
///
/// Không hỏi được thì vẫn phải có đúng một mục: cả luồng mã hoá lẫn giao diện
/// bên kia đều đánh số theo danh sách này, danh sách rỗng là chia cho không.
fn list_displays() -> Vec<rd_capture::DisplayInfo> {
    match rd_capture::PlatformCapturer::list_displays() {
        Ok(list) if !list.is_empty() => list,
        Ok(_) => Vec::new(),
        Err(err) => {
            tracing::warn!(%err, "không liệt kê được màn hình");
            Vec::new()
        }
    }
}

/// Danh sách gửi cho viewer. `id` là chỉ số trong `displays`.
fn monitor_list(
    displays: &[rd_capture::DisplayInfo],
    info: &PipelineInfo,
    active: u32,
) -> Vec<MonitorInfo> {
    if displays.is_empty() {
        return vec![MonitorInfo {
            id: 0,
            name: info.source.clone(),
            width: info.width,
            height: info.height,
            scale: 1.0,
            refresh_hz: info.target_fps,
            is_primary: true,
        }];
    }
    displays
        .iter()
        .enumerate()
        .map(|(index, display)| MonitorInfo {
            id: index as u8,
            // Màn hình đang chụp thì lấy kích thước thật của khung hình: kích
            // thước hệ điều hành khai có thể lệch vì DPI scaling.
            width: if index as u32 == active {
                info.width
            } else {
                display.width
            },
            height: if index as u32 == active {
                info.height
            } else {
                display.height
            },
            name: display.name.clone(),
            scale: display.scale,
            refresh_hz: display.refresh_hz,
            is_primary: display.is_primary,
        })
        .collect()
}

/// Kích thước và vị trí của màn hình thứ `index`, để bơm chuột cho đúng chỗ.
fn geometry_of(displays: &[rd_capture::DisplayInfo], index: u32) -> Option<ScreenGeometry> {
    let display = displays.get(index as usize)?;
    if display.logical_width == 0 || display.logical_height == 0 {
        return None;
    }
    Some(ScreenGeometry {
        width: display.logical_width as f32,
        height: display.logical_height as f32,
        origin_x: display.origin_x as f32,
        origin_y: display.origin_y as f32,
    })
}

#[cfg(target_os = "macos")]
fn host_geometry() -> rd_input::Result<ScreenGeometry> {
    rd_input::macos::main_screen_geometry()
}

#[cfg(target_os = "windows")]
fn host_geometry() -> rd_input::Result<ScreenGeometry> {
    rd_input::windows::main_screen_geometry()
}

/// Giữ nguyên chứng chỉ qua các lần chạy để vân tay máy chủ là thứ người dùng
/// đối chiếu được. Sinh mới mỗi lần khởi động thì vân tay đổi liên tục, ai muốn
/// kiểm tra cũng chịu, mà ghim vân tay ở phía người xem cũng thành vô nghĩa.
fn host_identity() -> Result<rd_transport::SelfSignedIdentity, rd_transport::TransportError> {
    match crate::app::app_dir() {
        Some(dir) => load_or_create_identity(&dir.join("host-identity.bin"), SERVER_NAME),
        None => rd_transport::generate_self_signed(SERVER_NAME),
    }
}

fn short(identity: &rd_transport::SelfSignedIdentity) -> String {
    rd_transport::fingerprint_short(&identity.fingerprint)
}

/// Mã 9 chữ số chia thành 3 nhóm cho dễ đọc qua điện thoại.
pub fn pretty(id: PeerId) -> String {
    let raw = id.get().to_string();
    format!("{} {} {}", &raw[0..3], &raw[3..6], &raw[6..9])
}

#[cfg(test)]
mod auth_gate_tests {
    use super::*;

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, n])
    }

    #[test]
    fn vai_lan_dau_khong_bi_phat() {
        let mut gate = AuthGate::default();
        let now = Instant::now();
        for _ in 0..AUTH_FREE_TRIES {
            assert!(gate.con_phai_cho(ip(1), now).is_zero());
            gate.ghi_sai(ip(1), now);
        }
        assert!(!gate.con_phai_cho(ip(1), now).is_zero());
    }

    #[test]
    fn cho_dai_dan_nhung_co_tran() {
        let mut gate = AuthGate::default();
        let now = Instant::now();
        for _ in 0..40 {
            gate.ghi_sai(ip(1), now);
        }
        assert_eq!(gate.con_phai_cho(ip(1), now), AUTH_LOCK_MAX);
    }

    #[test]
    fn phat_rieng_tung_dia_chi_va_xoa_khi_dung() {
        let mut gate = AuthGate::default();
        let now = Instant::now();
        for _ in 0..AUTH_FREE_TRIES + 3 {
            gate.ghi_sai(ip(1), now);
        }
        assert!(!gate.con_phai_cho(ip(1), now).is_zero());
        assert!(gate.con_phai_cho(ip(2), now).is_zero());
        gate.ghi_dung(ip(1));
        assert!(gate.con_phai_cho(ip(1), now).is_zero());
    }

    #[test]
    fn quen_sau_mot_quang_im_lang() {
        let mut gate = AuthGate::default();
        let now = Instant::now();
        for _ in 0..AUTH_FREE_TRIES + 3 {
            gate.ghi_sai(ip(1), now);
        }
        let sau = now + AUTH_FORGET + Duration::from_secs(1);
        assert!(gate.con_phai_cho(ip(1), sau).is_zero());
        assert!(gate.fails.is_empty());
    }

    #[test]
    fn cho_dung_lai_tang_dan_roi_dung_o_tran() {
        assert_eq!(cho_dung_lai(0), Duration::ZERO);
        assert_eq!(cho_dung_lai(1), RESTART_DELAY);
        assert_eq!(cho_dung_lai(2), RESTART_DELAY * 2);
        assert_eq!(cho_dung_lai(3), RESTART_DELAY * 4);
        assert_eq!(cho_dung_lai(9), RESTART_MAX);
    }

    #[test]
    fn bang_phat_khong_phinh_vo_han() {
        let mut gate = AuthGate::default();
        let now = Instant::now();
        for n in 0..AUTH_GATE_MAX {
            gate.ghi_sai(IpAddr::from((n as u32).to_be_bytes()), now);
        }
        let sau = now + AUTH_FORGET + Duration::from_secs(1);
        gate.ghi_sai(ip(9), sau);
        assert_eq!(gate.fails.len(), 1);
    }
}
