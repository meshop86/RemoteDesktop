//! Máy cho xem màn hình: chụp, mã hoá, gửi đi, và nhận điều khiển về.
//!
//! Vòng ngoài chờ máy khác gọi tới theo hai đường cùng lúc — nối thẳng vào cổng
//! đang mở, hoặc qua rendezvous server. Phục vụ xong một phiên thì quay lại chờ
//! phiên sau; host không tự tắt khi viewer ngắt.
//!
//! Chỉ chụp màn hình *trong lúc có người xem*. Vừa đỡ tốn GPU, vừa là điều
//! người dùng mong đợi: đèn báo quay màn hình của hệ điều hành tắt đi đúng lúc
//! phiên kết thúc, chứ không sáng suốt ngày vì chương trình đang mở.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use anyhow::Context as _;
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
use rd_transport::{
    Session, VideoSender, generate_self_signed, server_endpoint,
};
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

/// Không lấy được frame nào lâu tới mức này thì coi như chuỗi chụp đã tắc.
///
/// Màn hình đứng yên *không* rơi vào đây: chuỗi chụp vẫn nộp lại frame cũ theo
/// nhịp riêng của nó, nên frame đã nén vẫn đều đặn chảy ra.
const STALL_LIMIT: Duration = Duration::from_secs(5);

/// Nghỉ giữa hai lần dựng lại chuỗi chụp — đủ để driver đồ hoạ hoặc phiên đăng
/// nhập vừa đổi kịp ổn định.
const RESTART_DELAY: Duration = Duration::from_secs(1);

/// Dựng lại quá số lần này mà vẫn không có hình thì báo hẳn ra thay vì thử mãi.
const MAX_RESTARTS: u32 = 5;

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
            ViewerCommand::FileChunkAck(ack) => Incoming::Ack(ack),
            ViewerCommand::Input(event) => Incoming::Input(event),
            ViewerCommand::RequestKeyframe { .. } => Incoming::Keyframe,
            ViewerCommand::SetQuality(request) => Incoming::Quality(request),
            ViewerCommand::Ping { sent_us } => Incoming::Ping { sent_us },
            ViewerCommand::Disconnect => Incoming::Disconnect,
            // Hello đã xử lý lúc bắt tay; Feedback và SelectMonitor chưa dùng.
            ViewerCommand::Hello { .. }
            | ViewerCommand::SelectMonitor { .. }
            | ViewerCommand::Feedback { .. } => Incoming::Ignored,
        }
    }
}

pub async fn run(
    ctx: Arc<Context>,
    mut commands: UnboundedReceiver<UiCommand>,
) -> anyhow::Result<()> {
    let identity = generate_self_signed(SERVER_NAME).context("không tạo được chứng chỉ")?;
    let endpoint = server_endpoint(ctx.config.bind, &identity).context("không mở được cổng UDP")?;
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
        let ctx = ctx.clone();
        let endpoint = endpoint.clone();
        AbortOnDrop(tokio::spawn(async move {
            signal_loop(ctx, endpoint, server, call_tx).await;
        }))
    });

    loop {
        let (session, relayed, _keep) = tokio::select! {
            incoming = endpoint.accept() => {
                let incoming = incoming.context("endpoint đã đóng")?;
                match incoming.await {
                    Ok(conn) => (Session::new(conn), false, None),
                    Err(err) => {
                        tracing::warn!(%err, "kết nối vào hỏng lúc bắt tay");
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
                match answer(&ctx, &endpoint, &identity, call).await {
                    Ok(result) => result,
                    Err(err) => {
                        ctx.events.status(format!("không nối được với viewer: {err}"));
                        continue;
                    }
                }
            }
        };

        let peer = session.remote_address();
        ctx.events.status(format!(
            "viewer {peer} đã nối{}",
            if relayed { " (qua relay)" } else { "" }
        ));

        match serve(&ctx, session, &mut commands, relayed).await {
            Ok(()) => ctx.events.status("viewer đã ngắt, quay lại chờ"),
            Err(err) => ctx.events.status(format!("phiên kết thúc: {err}")),
        }
        ctx.events.send(NetEvent::PeerLeft);
        if ctx.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
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
    let hello = match check_hello(ctx, hello) {
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

    let keyframe = Arc::new(AtomicBool::new(true));
    // Hai con số khác nhau: `ceiling` là mức người dùng đặt (viewer gửi sang),
    // `bitrate` là mức bộ dò thật sự chốt và luồng mã hoá đọc.
    let ceiling = Arc::new(AtomicU32::new(0));
    let bitrate = Arc::new(AtomicU32::new(0));
    let rate = RateController::new(ctx.config.bitrate_kbps);
    // Chỉ dùng 4:2:2 10-bit khi *cả hai* đầu làm được: bên này mã hoá được và
    // bên kia dựng hình được.
    let allow_10bit = ctx.config.allow_10bit && hello.wants_10bit;
    let (info, mut encoded_rx) = start_encoder(
        ctx,
        rate.current(),
        allow_10bit,
        hello.codecs,
        keyframe.clone(),
        bitrate.clone(),
    )
    .await?;

    tx.send(&HostEvent::Welcome {
        version: rd_protocol::PROTOCOL_VERSION,
        host_name: ctx.config.name.clone(),
        monitors: vec![MonitorInfo {
            id: 0,
            name: info.source.clone(),
            width: info.width,
            height: info.height,
            scale: 1.0,
            refresh_hz: info.target_fps,
            is_primary: true,
        }],
        active_monitor: 0,
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
    ctx.events.send(NetEvent::Info(Box::new(info.clone())));

    let input = spawn_injector(ctx, info.width, info.height);

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
        bitrate,
    )));

    let hooks = Hooks {
        input: Some(input),
        keyframe: Some(keyframe),
        bitrate: Some(ceiling),
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
fn check_hello(ctx: &Arc<Context>, hello: ViewerCommand) -> anyhow::Result<Hello> {
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
    // So sánh thường, không phải so sánh thời gian hằng định: đây là hash của
    // một mật khẩu ngẫu nhiên đổi mỗi phiên, không phải khoá dài hạn, và kênh
    // đã có TLS nên không đo được thời gian từ ngoài.
    if auth != ctx.auth() {
        anyhow::bail!("sai mật khẩu phiên");
    }
    Ok(Hello {
        viewer_name,
        wants_10bit,
        codecs,
    })
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
    keyframe: Arc<AtomicBool>,
    bitrate: Arc<AtomicU32>,
) -> anyhow::Result<(PipelineInfo, tokio::sync::mpsc::Receiver<EncodedFrame>)> {
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<anyhow::Result<PipelineInfo>>();
    let (frame_tx, frame_rx) = tokio::sync::mpsc::channel::<EncodedFrame>(ENCODED_QUEUE);
    let target_fps = ctx.config.target_fps;
    // Mức bộ dò đang chốt. Giữ lại để lúc dựng lại chuỗi còn mở đúng ở mức
    // đường truyền chịu được, chứ không quay về mức khởi điểm.
    let mut bitrate_kbps = start_kbps;
    let events = ctx.events.clone();

    std::thread::Builder::new()
        .name("rd-encode".into())
        .spawn(move || {
            let mut source = match EncodeSource::start(
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
            let mut first_frame = true;
            let mut last_frame = std::time::Instant::now();
            loop {
                if keyframe.swap(false, Ordering::Relaxed) {
                    source.request_keyframe();
                }
                let kbps = bitrate.swap(0, Ordering::Relaxed);
                if kbps > 0 {
                    bitrate_kbps = kbps;
                    if let Err(err) = source.set_bitrate(kbps) {
                        tracing::warn!(%err, kbps, "không đổi được bitrate");
                    }
                }

                // Lý do phải dựng lại cả chuỗi, nếu có.
                let broken = match source.next_encoded(Duration::from_millis(500)) {
                    Ok(Some(frame)) => {
                        errors = 0;
                        last_frame = std::time::Instant::now();
                        if first_frame {
                            first_frame = false;
                            let info = source.info();
                            events.status(format!(
                                "đang gửi hình {}x{} {:?}",
                                info.width, info.height, info.codec
                            ));
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
                if restarts > MAX_RESTARTS {
                    events.status(format!("dừng chụp màn hình: {reason}"));
                    break;
                }
                events.status(format!("mất hình ({reason}), đang dựng lại…"));
                source.stop();
                std::thread::sleep(RESTART_DELAY);
                match EncodeSource::start(target_fps, bitrate_kbps, allow_10bit, &viewer_codecs) {
                    Ok(fresh) => {
                        source = fresh;
                        errors = 0;
                        first_frame = true;
                        last_frame = std::time::Instant::now();
                        // Người xem đang giữ một tấm hình chết; chuỗi mới phải
                        // mở đầu bằng keyframe thì hình mới sống lại được.
                        source.request_keyframe();
                    }
                    Err(err) => {
                        events.status(format!("không dựng lại được chuỗi chụp: {err}"));
                        break;
                    }
                }
            }
            source.stop();
        })
        .context("không tạo được luồng mã hoá")?;

    let info = ready_rx.await.context("luồng mã hoá chết khi khởi động")??;
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
    bitrate: Arc<AtomicU32>,
) {
    let mut ticker = tokio::time::interval(RATE_INTERVAL);
    // Máy ngủ dậy thì bỏ qua các nhịp đã lỡ: dồn chúng lại chỉ tạo một chuỗi
    // phép đo trên cùng một ảnh chụp đường truyền.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;

        let requested = ceiling.swap(0, Ordering::Relaxed);
        let forced = (requested > 0).then(|| rate.set_ceiling(requested)).flatten();

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
) -> std::sync::mpsc::Sender<InputEvent> {
    let (tx, rx) = std::sync::mpsc::channel::<InputEvent>();
    let events = ctx.events.clone();

    std::thread::Builder::new()
        .name("rd-inject".into())
        .spawn(move || {
            // Kích thước thật của màn hình host, không phải kích thước frame:
            // trên màn Retina hai số này lệch nhau đúng hệ số scale, và lấy
            // nhầm thì chuột chỉ chạy trong một phần tư màn hình.
            let geometry = match host_geometry() {
                Ok(geometry) => geometry,
                Err(err) => {
                    tracing::warn!(%err, "không đọc được kích thước màn hình, dùng cỡ frame");
                    ScreenGeometry {
                        width: width as f32,
                        height: height as f32,
                    }
                }
            };
            let mut injector = match PlatformInjector::open(geometry) {
                Ok(injector) => injector,
                Err(err) => {
                    events.status(format!("không bơm được input: {err}"));
                    return;
                }
            };

            let mut errors = 0u32;
            while let Ok(event) = rx.recv() {
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

#[cfg(target_os = "macos")]
fn host_geometry() -> rd_input::Result<ScreenGeometry> {
    rd_input::macos::main_screen_geometry()
}

#[cfg(target_os = "windows")]
fn host_geometry() -> rd_input::Result<ScreenGeometry> {
    rd_input::windows::main_screen_geometry()
}

fn short(identity: &rd_transport::SelfSignedIdentity) -> String {
    rd_transport::fingerprint_short(&identity.fingerprint)
}

/// Mã 9 chữ số chia thành 3 nhóm cho dễ đọc qua điện thoại.
pub fn pretty(id: PeerId) -> String {
    let raw = id.get().to_string();
    format!("{} {} {}", &raw[0..3], &raw[3..6], &raw[6..9])
}
