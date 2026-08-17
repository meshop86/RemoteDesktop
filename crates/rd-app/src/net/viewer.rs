//! Máy xem: tìm host, nhận video, giải mã, gửi input về.
//!
//! Thứ tự bắt tay ở đây quan trọng hơn vẻ ngoài của nó. QUIC mở stream *lười*:
//! `open_bi()` trả về ngay mà không gửi gì cả, nên `accept_bi()` bên host chỉ
//! nhả ra sau khi ta ghi byte đầu tiên. Mở kênh rồi ngồi chờ Welcome là hai bên
//! chờ nhau vĩnh viễn. Vì thế `Hello` phải đi ngay sau khi mở kênh.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context as _;
use rd_protocol::AssembledFrame;
use rd_protocol::control::{
    ChatMessage, ClipboardText, FileChunkAck, FileOffer, HostEvent, InputEvent, QualityRequest,
    ViewerCommand,
};
use rd_session::IdSpace;
use rd_signal::client::local_candidates;
use rd_signal::{RelayLink, SignalClient};
use rd_transport::{
    ClockSync, Session, VideoReceiver, client_endpoint, generate_self_signed, now_us,
    server_endpoint,
};
use rd_viewer::pipeline::{DecodeSink, PipelineInfo};
use tokio::sync::mpsc::UnboundedReceiver;

use super::link::{self, Hooks, Incoming, Wire};
use super::{AbortOnDrop, Context, NetEvent, PeerAddress, SERVER_NAME, UiCommand};

/// Chờ host trả lời bắt tay bao lâu.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Hàng đợi frame chưa giải mã.
///
/// Sâu 2: một frame đang giải mã, một frame đợi. Dồn thêm chỉ tổ tích độ trễ —
/// frame thứ ba luôn tới muộn hơn thời điểm nó còn có ích.
const DECODE_QUEUE: usize = 2;

pub struct ViewerWire;

impl Wire for ViewerWire {
    type Out = ViewerCommand;
    type In = HostEvent;

    fn space() -> IdSpace {
        IdSpace::Viewer
    }

    fn chat(message: ChatMessage) -> ViewerCommand {
        ViewerCommand::Chat(message)
    }

    fn clipboard(text: String) -> ViewerCommand {
        ViewerCommand::Clipboard(ClipboardText { text })
    }

    fn offer(offer: FileOffer) -> ViewerCommand {
        ViewerCommand::FileOffer(offer)
    }

    fn accept(transfer_id: u64) -> ViewerCommand {
        ViewerCommand::FileAccept { transfer_id }
    }

    fn reject(transfer_id: u64) -> ViewerCommand {
        ViewerCommand::FileReject { transfer_id }
    }

    fn ack(ack: FileChunkAck) -> ViewerCommand {
        ViewerCommand::FileChunkAck(ack)
    }

    fn input(event: InputEvent) -> Option<ViewerCommand> {
        Some(ViewerCommand::Input(event))
    }

    fn keyframe() -> Option<ViewerCommand> {
        Some(ViewerCommand::RequestKeyframe { monitor: 0 })
    }

    fn quality(request: QualityRequest) -> Option<ViewerCommand> {
        Some(ViewerCommand::SetQuality(request))
    }

    fn ping(sent_us: u64) -> Option<ViewerCommand> {
        Some(ViewerCommand::Ping { sent_us })
    }

    fn pong(_sent_us: u64, _host_us: u64) -> Option<ViewerCommand> {
        None
    }

    fn classify(message: HostEvent) -> Incoming {
        match message {
            HostEvent::Chat(message) => Incoming::Chat(message),
            HostEvent::Clipboard(data) => Incoming::Clipboard(data.text),
            HostEvent::FileOffer(offer) => Incoming::Offer(offer),
            HostEvent::FileAccept { transfer_id } => Incoming::Accept(transfer_id),
            HostEvent::FileReject { transfer_id } => Incoming::Reject(transfer_id),
            HostEvent::FileChunkAck(ack) => Incoming::Ack(ack),
            HostEvent::Pong { sent_us, host_us } => Incoming::Pong { sent_us, host_us },
            HostEvent::Error { message } => Incoming::Error(message),
            HostEvent::Welcome { .. }
            | HostEvent::MonitorsChanged { .. }
            | HostEvent::QualityChanged(_) => Incoming::Ignored,
        }
    }
}

pub async fn run(
    ctx: Arc<Context>,
    mut commands: UnboundedReceiver<UiCommand>,
) -> anyhow::Result<()> {
    let peer = ctx
        .config
        .peer
        .clone()
        .context("chưa cho biết nối tới đâu")?;

    // `_relay` phải sống bằng tuổi thọ phiên: nó sở hữu endpoint mà kết nối
    // đang chạy trên đó.
    let (session, relayed, _relay) = match peer {
        PeerAddress::Direct(addr) => (connect_direct(&ctx, addr).await?, false, None),
        PeerAddress::Code(code) => connect_via_rendezvous(&ctx, code).await?,
    };

    let (mut tx, mut rx) = session.open_control::<ViewerCommand, HostEvent>().await?;
    // Gửi ngay, xem ghi chú đầu file.
    // Khai đúng những gì máy này giải mã được. Host chốt codec theo danh sách
    // này, nên khai thừa là phiên nối xong rồi tắt ngay vì không dựng nổi bộ
    // giải mã, còn khai thiếu chỉ là mất chút băng thông.
    let codecs = rd_codec::decodable();
    tracing::info!(?codecs, "khai codec giải mã được");
    tx.send(&ViewerCommand::Hello {
        version: rd_protocol::PROTOCOL_VERSION,
        viewer_name: ctx.config.name.clone(),
        auth: ctx.auth(),
        wants_10bit: ctx.config.allow_10bit,
        codecs,
    })
    .await?;

    let (host_name, info) = handshake(&mut rx).await?;
    ctx.events.send(NetEvent::Connected {
        peer: session.remote_address(),
        name: host_name,
        relayed,
    });
    ctx.events.send(NetEvent::Info(Box::new(info.clone())));

    let clock = Arc::new(Mutex::new(ClockSync::new()));
    let video = AbortOnDrop(tokio::spawn(video_loop(
        ctx.clone(),
        session.clone(),
        info,
        clock.clone(),
    )));

    let hooks = Hooks {
        clock: Some(clock),
        ..Hooks::default()
    };

    let mut video = video;
    tokio::select! {
        result = link::run::<ViewerWire>(ctx.clone(), session.clone(), tx, rx, &mut commands, hooks) => result,
        result = &mut video.0 => result?,
    }
}

async fn connect_direct(
    ctx: &Arc<Context>,
    addr: std::net::SocketAddr,
) -> anyhow::Result<Session> {
    ctx.events.status(format!("đang nối tới {addr}"));
    let endpoint = client_endpoint(ctx.config.bind, ctx.config.peer_fingerprint)
        .context("không mở được cổng UDP")?;
    let conn = endpoint.connect(addr, SERVER_NAME)?.await?;
    Ok(Session::new(conn))
}

/// Nối qua rendezvous server: hỏi địa chỉ, đục lỗ NAT, hỏng thì đi vòng relay.
async fn connect_via_rendezvous(
    ctx: &Arc<Context>,
    code: rd_signal::PeerId,
) -> anyhow::Result<(Session, bool, Option<RelayLink>)> {
    let server = ctx
        .config
        .rendezvous
        .context("cần địa chỉ rendezvous server mới dùng được mã")?;

    // Endpoint phải nhận được kết nối vào, không chỉ gọi ra: đục lỗ NAT là hai
    // bên cùng gọi, ai tới trước thì bên kia là phía nhận.
    let identity = generate_self_signed("rd-peer").context("không tạo được chứng chỉ")?;
    let endpoint = server_endpoint(ctx.config.bind, &identity).context("không mở được cổng UDP")?;

    ctx.events.status("đang hỏi rendezvous server");
    let mut client = SignalClient::connect(
        &endpoint,
        server,
        "rd-signal",
        ctx.config.rendezvous_fingerprint,
    )
    .await?;
    let candidates = client.request(code, local_candidates(&endpoint)).await?;

    ctx.events.status("đang đục lỗ NAT");
    let config = rd_signal::punch::PunchConfig {
        server_name: SERVER_NAME.into(),
        fingerprint: ctx.config.peer_fingerprint,
        ..Default::default()
    };
    match rd_signal::punch::punch(&endpoint, &candidates, &config).await {
        Ok(conn) => Ok((Session::new(conn), false, None)),
        Err(rd_signal::SignalError::Timeout) => {
            // NAT đối xứng ở ít nhất một đầu. Relay chậm hơn nhưng luôn chạy.
            ctx.events.status("không đục được, chuyển sang relay");
            let (relay, token) = client.request_relay(code).await?;
            let link = RelayLink::open(relay, token, &identity)?;
            let conn = link
                .connect(
                    SERVER_NAME,
                    ctx.config.peer_fingerprint,
                    rd_signal::punch::DEFAULT_TIMEOUT,
                )
                .await?;
            Ok((Session::new(conn), true, Some(link)))
        }
        Err(err) => Err(err.into()),
    }
}

/// Chờ Welcome rồi QualityChanged.
///
/// Cần cả hai mới dựng được bộ giải mã: Welcome mang kích thước màn hình (Media
/// Foundation phải biết trước khi dựng), QualityChanged mang codec và mức lấy
/// mẫu màu *thật* mà bộ mã hoá chốt được — không phải mức ta yêu cầu.
async fn handshake(
    rx: &mut rd_transport::ControlReceiver<HostEvent>,
) -> anyhow::Result<(String, PipelineInfo)> {
    let mut host_name = None;
    let mut size = None;
    let mut fps = 60;

    loop {
        let message = tokio::time::timeout(HANDSHAKE_TIMEOUT, rx.recv())
            .await
            .map_err(|_| anyhow::anyhow!("host không trả lời trong {HANDSHAKE_TIMEOUT:?}"))??;
        match message {
            HostEvent::Welcome {
                version,
                host_name: name,
                monitors,
                active_monitor,
            } => {
                if version != rd_protocol::PROTOCOL_VERSION {
                    anyhow::bail!(
                        "phiên bản giao thức lệch: host {version}, viewer {}",
                        rd_protocol::PROTOCOL_VERSION
                    );
                }
                let monitor = monitors
                    .iter()
                    .find(|m| m.id == active_monitor)
                    .or_else(|| monitors.first())
                    .context("host không khai màn hình nào")?;
                size = Some((monitor.name.clone(), monitor.width, monitor.height));
                fps = monitor.refresh_hz.max(1);
                host_name = Some(name);
            }
            HostEvent::QualityChanged(quality) => {
                let (source, width, height) = size
                    .clone()
                    .context("host gửi QualityChanged trước Welcome")?;
                return Ok((
                    host_name.unwrap_or_default(),
                    PipelineInfo {
                        source,
                        width,
                        height,
                        codec: quality.codec,
                        chroma: quality.chroma,
                        // Bộ mã hoá nằm ở máy kia; ta không biết nó chạy phần
                        // cứng hay phần mềm, và cũng không cần biết.
                        hardware: true,
                        target_fps: fps,
                    },
                ));
            }
            HostEvent::Error { message } => anyhow::bail!("{message}"),
            other => tracing::debug!(?other, "bỏ qua thông điệp trong lúc bắt tay"),
        }
    }
}

/// Nhận datagram, ghép frame, đẩy sang luồng giải mã.
async fn video_loop(
    ctx: Arc<Context>,
    session: Session,
    info: PipelineInfo,
    clock: Arc<Mutex<ClockSync>>,
) -> anyhow::Result<()> {
    let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel::<AssembledFrame>(DECODE_QUEUE);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<anyhow::Result<()>>();
    spawn_decoder(ctx.clone(), info, clock, raw_rx, ready_tx)?;
    ready_rx.await.context("luồng giải mã chết khi khởi động")??;

    let mut receiver = VideoReceiver::new(session, 4);
    // Id của frame cuối cùng thực sự tới được bộ giải mã. Mọi khoảng trống ở
    // đây đều làm đứt chuỗi dự đoán, dù nguyên nhân là mất gói hay là ta tự bỏ
    // frame vì giải mã chậm.
    let mut last_id: Option<u32> = None;
    loop {
        let frame = receiver.next_frame().await?;
        let frame_id = frame.frame_id;
        let keyframe = frame.keyframe;
        match raw_tx.try_send(frame) {
            Ok(()) => {
                let gap = last_id.is_some_and(|last| frame_id != last.wrapping_add(1));
                last_id = Some(frame_id);
                // Keyframe tự nó dựng lại chuỗi, không cần xin thêm.
                if gap && !keyframe && ctx.ask_keyframe() {
                    tracing::warn!(frame_id, "mất frame trên đường truyền, đang xin keyframe");
                }
            }
            // Giải mã không theo kịp. Bỏ frame là đúng: giữ lại chỉ làm mọi
            // frame sau đó đến muộn thêm.
            Err(std::sync::mpsc::TrySendError::Full(_)) => {}
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                anyhow::bail!("luồng giải mã đã dừng")
            }
        }
    }
}

fn spawn_decoder(
    ctx: Arc<Context>,
    info: PipelineInfo,
    clock: Arc<Mutex<ClockSync>>,
    frames: std::sync::mpsc::Receiver<AssembledFrame>,
    ready: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    std::thread::Builder::new()
        .name("rd-decode".into())
        .spawn(move || {
            let mut sink = match DecodeSink::new(info.codec, info.width, info.height, info.chroma) {
                Ok(sink) => {
                    let _ = ready.send(Ok(()));
                    sink
                }
                Err(err) => {
                    let _ = ready.send(Err(err));
                    return;
                }
            };

            let mut errors = 0u32;
            while let Ok(frame) = frames.recv() {
                // `encode_us` không đi trên dây — thêm nó vào mỗi datagram để
                // hiện một con số chẩn đoán là không đáng. HUD hiện "—".
                match sink.decode(&frame.data, frame.capture_us, 0, frame.keyframe) {
                    Ok(Some(mut item)) => {
                        errors = 0;
                        // Hai máy hai đồng hồ: hiệu số thô vô nghĩa. Chỉ khi
                        // đã có mẫu ping mới quy đổi được về độ trễ thật.
                        if let Some(latency) = clock
                            .lock()
                            .expect("khoá đồng hồ")
                            .latency_us(frame.capture_us, now_us())
                        {
                            item.pipeline_us = latency.max(0) as u32;
                        }
                        ctx.push_frame(item);
                    }
                    Ok(None) => {
                        // Lưới an toàn cho trường hợp id frame liền mạch mà dữ
                        // liệu vẫn hỏng (mất gói ngay trong keyframe chẳng hạn).
                        if sink.take_bad_data() && ctx.ask_keyframe() {
                            tracing::warn!("dữ liệu hỏng, đang xin keyframe");
                        }
                    }
                    Err(err) => {
                        errors += 1;
                        tracing::warn!(%err, errors, "giải mã lỗi");
                        if errors >= 60 {
                            ctx.events.status(format!("dừng giải mã: {err}"));
                            break;
                        }
                    }
                }
                if ctx.stop.load(Ordering::Relaxed) {
                    break;
                }
            }
        })
        .context("không tạo được luồng giải mã")?;
    Ok(())
}
