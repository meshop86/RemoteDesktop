//! Phần kênh điều khiển giống nhau ở cả hai đầu: chat, truyền file, đồng hồ.
//!
//! Giao thức thì *không* đối xứng — viewer gửi [`ViewerCommand`], host gửi
//! [`HostEvent`] — nhưng luật chơi của chat và file thì đối xứng hoàn toàn. Nếu
//! viết hai lần, hai bản sẽ lệch nhau ngay ở lần sửa lỗi đầu tiên, mà lỗi truyền
//! file lại là loại chỉ lộ ra khi chạy thật. Nên chỗ khác nhau bị ép xuống đúng
//! một [`Wire`]: cách dựng và cách đọc thông điệp. Phần còn lại viết một lần.
//!
//! [`ViewerCommand`]: rd_protocol::control::ViewerCommand
//! [`HostEvent`]: rd_protocol::control::HostEvent

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rd_protocol::control::{ChatMessage, FileChunkAck, FileOffer, InputEvent, QualityRequest};
use rd_session::IdSpace;
use rd_session::transfer::MAX_PENDING_INCOMING;
use rd_transport::{ClockSync, ControlReceiver, ControlSender, Session, now_us};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinSet;

use super::{Context, NetEvent, UiCommand};

/// Báo cho bên gửi biết đã ghi tới đâu sau mỗi chừng này byte.
///
/// Bên gửi vẽ tiến độ bằng con số này chứ không bằng số byte nó đã đẩy vào
/// stream — số đó chạy trước thực tế đúng bằng cửa sổ nghẽn, nên thanh tiến độ
/// sẽ chạm 100% rồi đứng đó chờ. Một lần báo mỗi 1 MB là đủ mượt mắt và không
/// đáng kể so với chính dữ liệu.
const ACK_EVERY: u64 = 1024 * 1024;

/// Nhịp gửi ping đo đồng hồ và nhịp báo số liệu đường truyền lên giao diện.
const PING_INTERVAL: Duration = Duration::from_secs(1);

/// Thông điệp nhận được, đã quy về nghĩa chung cho cả hai chiều.
pub enum Incoming {
    Chat(ChatMessage),
    Offer(FileOffer),
    Accept(u64),
    Reject(u64),
    Ack(FileChunkAck),
    /// Chỉ host nhận được.
    Input(InputEvent),
    /// Chỉ host nhận được.
    Keyframe,
    /// Chỉ host nhận được.
    Quality(QualityRequest),
    /// Chỉ host nhận được.
    Ping {
        sent_us: u64,
    },
    /// Chỉ viewer nhận được.
    Pong {
        sent_us: u64,
        host_us: u64,
    },
    /// Đầu kia báo lỗi và sẽ đóng kết nối.
    Error(String),
    /// Đầu kia chủ động ngắt.
    Disconnect,
    /// Hợp lệ nhưng tầng này không quan tâm (Welcome, MonitorsChanged...).
    Ignored,
}

/// Chỗ duy nhất mà hai chiều khác nhau.
pub trait Wire: Send + Sync + 'static {
    /// Thông điệp bên này gửi đi.
    /// `Sync` vì bộ gửi mượn `&Out` xuyên qua một điểm `await` khi mã hoá rồi
    /// ghi xuống stream; tokio đòi mọi thứ sống qua `await` phải gửi được sang
    /// luồng khác.
    type Out: Serialize + Send + Sync + 'static;
    /// Thông điệp bên này nhận về.
    type In: DeserializeOwned + Send + 'static;

    /// Nửa không gian số hiệu file mà bên này được cấp phát.
    fn space() -> IdSpace;

    fn chat(message: ChatMessage) -> Self::Out;
    fn offer(offer: FileOffer) -> Self::Out;
    fn accept(transfer_id: u64) -> Self::Out;
    fn reject(transfer_id: u64) -> Self::Out;
    fn ack(ack: FileChunkAck) -> Self::Out;

    /// `None` nghĩa là chiều này không có thông điệp tương ứng — ví dụ host
    /// không gửi input sang viewer.
    fn input(event: InputEvent) -> Option<Self::Out>;
    fn keyframe() -> Option<Self::Out>;
    fn quality(request: QualityRequest) -> Option<Self::Out>;
    fn ping(sent_us: u64) -> Option<Self::Out>;
    fn pong(sent_us: u64, host_us: u64) -> Option<Self::Out>;

    fn classify(message: Self::In) -> Incoming;
}

/// Những thứ chỉ một trong hai đầu có.
#[derive(Default)]
pub struct Hooks {
    /// Host: nơi nhận sự kiện input để bơm xuống hệ điều hành.
    pub input: Option<std::sync::mpsc::Sender<InputEvent>>,
    /// Host: cờ xin keyframe, luồng mã hoá đọc và tự xoá.
    pub keyframe: Option<Arc<AtomicBool>>,
    /// Host: bitrate mới viewer yêu cầu; 0 nghĩa là chưa có yêu cầu nào.
    pub bitrate: Option<Arc<AtomicU32>>,
    /// Viewer: đồng hồ đã hiệu chỉnh theo host, để tính độ trễ một chiều.
    pub clock: Option<Arc<Mutex<ClockSync>>>,
}

pub struct Link<W: Wire> {
    ctx: Arc<Context>,
    session: Session,
    outbox: UnboundedSender<W::Out>,
    hooks: Hooks,
    /// Lời mời ta đã gửi, đang chờ đầu kia trả lời.
    pending_out: Mutex<HashMap<u64, (FileOffer, PathBuf)>>,
    /// Lời mời đầu kia gửi sang, đang chờ người dùng bên này bấm nhận.
    offered_in: Mutex<HashMap<u64, FileOffer>>,
    /// Đã bấm nhận, đang chờ stream dữ liệu tới.
    accepted_in: Mutex<HashMap<u64, FileOffer>>,
    counter: AtomicU64,
}

impl<W: Wire> Link<W> {
    fn emit(&self, event: NetEvent) {
        self.ctx.events.send(event);
    }

    fn send(&self, message: W::Out) {
        let _ = self.outbox.send(message);
    }
}

/// Chạy kênh điều khiển cho tới khi đứt.
///
/// `commands` mượn chứ không sở hữu: host phục vụ nhiều phiên nối tiếp nhau
/// trên cùng một kênh lệnh của giao diện.
pub async fn run<W: Wire>(
    ctx: Arc<Context>,
    session: Session,
    tx: ControlSender<W::Out>,
    rx: ControlReceiver<W::In>,
    commands: &mut UnboundedReceiver<UiCommand>,
    hooks: Hooks,
) -> anyhow::Result<()> {
    let (out_tx, mut out_rx) = unbounded_channel::<W::Out>();
    let link = Arc::new(Link::<W> {
        ctx,
        session: session.clone(),
        outbox: out_tx,
        hooks,
        pending_out: Mutex::new(HashMap::new()),
        offered_in: Mutex::new(HashMap::new()),
        accepted_in: Mutex::new(HashMap::new()),
        counter: AtomicU64::new(0),
    });

    let mut tasks: JoinSet<anyhow::Result<()>> = JoinSet::new();

    // Một task duy nhất sở hữu đầu gửi. Mọi nơi khác chỉ đẩy vào hộp thư —
    // nhờ đó hàm đồng bộ (như callback tiến độ của `recv_file`) cũng gửi được.
    tasks.spawn(async move {
        let mut tx = tx;
        while let Some(message) = out_rx.recv().await {
            tx.send(&message).await?;
        }
        Ok(())
    });

    tasks.spawn({
        let link = link.clone();
        async move { read_loop(link, rx).await }
    });
    tasks.spawn({
        let link = link.clone();
        async move { uni_loop(link).await }
    });
    tasks.spawn({
        let link = link.clone();
        async move { heartbeat(link).await }
    });

    tokio::select! {
        finished = tasks.join_next() => match finished {
            Some(Ok(result)) => result,
            Some(Err(err)) => Err(err.into()),
            None => Ok(()),
        },
        result = command_loop(link.clone(), commands) => result,
    }
    // `tasks` bị thả ở đây và JoinSet huỷ mọi task còn lại.
}

async fn read_loop<W: Wire>(
    link: Arc<Link<W>>,
    mut rx: ControlReceiver<W::In>,
) -> anyhow::Result<()> {
    loop {
        let message = rx.recv().await?;
        match W::classify(message) {
            Incoming::Chat(message) => link.emit(NetEvent::Chat(message)),
            Incoming::Offer(offer) => on_offer(&link, offer),
            Incoming::Accept(id) => on_accept(&link, id),
            Incoming::Reject(id) => {
                link.pending_out.lock().expect("khoá lời mời").remove(&id);
                link.emit(NetEvent::TransferRejected(id));
            }
            Incoming::Ack(ack) => link.emit(NetEvent::TransferProgress {
                id: ack.transfer_id,
                done: ack.received_bytes,
            }),
            Incoming::Input(event) => {
                if let Some(sink) = &link.hooks.input {
                    // Đứt kênh bơm không đáng cắt cả phiên: người dùng vẫn xem
                    // được màn hình, chỉ là không điều khiển được nữa.
                    let _ = sink.send(event);
                }
            }
            Incoming::Keyframe => {
                if let Some(flag) = &link.hooks.keyframe {
                    flag.store(true, Ordering::Relaxed);
                }
            }
            Incoming::Quality(request) => {
                if let Some(slot) = &link.hooks.bitrate {
                    slot.store(request.target_bitrate_kbps, Ordering::Relaxed);
                }
            }
            Incoming::Ping { sent_us } => {
                if let Some(message) = W::pong(sent_us, now_us()) {
                    link.send(message);
                }
            }
            Incoming::Pong { sent_us, host_us } => {
                if let Some(clock) = &link.hooks.clock {
                    let mut clock = clock.lock().expect("khoá đồng hồ");
                    clock.on_pong(sent_us, host_us, now_us());
                    if let Some(rtt) = clock.rtt_us() {
                        link.emit(NetEvent::Latency(rtt / 2));
                    }
                }
            }
            Incoming::Error(message) => anyhow::bail!("{message}"),
            Incoming::Disconnect => return Ok(()),
            Incoming::Ignored => {}
        }
    }
}

fn on_offer<W: Wire>(link: &Arc<Link<W>>, offer: FileOffer) {
    // Số hiệu phải thuộc nửa của đầu kia. Không thì đầu kia đang giẫm lên số
    // hiệu của ta và sẽ có hai file khác nhau mang cùng một số.
    if !W::space().peer().owns(offer.transfer_id) {
        tracing::warn!(id = offer.transfer_id, "số hiệu file sai nửa, bỏ qua");
        return;
    }
    {
        let mut offered = link.offered_in.lock().expect("khoá lời mời");
        if offered.len() >= MAX_PENDING_INCOMING {
            // Không im lặng bỏ: đầu kia phải biết để thôi chờ.
            link.send(W::reject(offer.transfer_id));
            return;
        }
        offered.insert(offer.transfer_id, offer.clone());
    }
    link.emit(NetEvent::IncomingOffer(offer));
}

fn on_accept<W: Wire>(link: &Arc<Link<W>>, id: u64) {
    let Some((offer, path)) = link.pending_out.lock().expect("khoá lời mời").remove(&id) else {
        tracing::warn!(id, "nhận FileAccept cho lời mời không còn tồn tại");
        return;
    };
    link.emit(NetEvent::TransferAccepted(id));

    let session = link.session.clone();
    let events = link.ctx.events.clone();
    tokio::spawn(async move {
        // Tiến độ ở đây là số byte đã đẩy vào stream, luôn chạy trước thực tế.
        // Thanh tiến độ dùng ack của đầu kia, nên chỗ này bỏ trống.
        match rd_transport::send_file(&session, &offer, &path, |_| {}).await {
            Ok(()) => events.send(NetEvent::TransferDone { id, path }),
            Err(err) => events.send(NetEvent::TransferFailed {
                id,
                reason: err.to_string(),
            }),
        }
    });
}

/// Nhận file: mỗi file là một uni stream mới, mở đầu bằng số hiệu.
async fn uni_loop<W: Wire>(link: Arc<Link<W>>) -> anyhow::Result<()> {
    loop {
        let mut stream = link.session.accept_uni().await?;
        let id = rd_transport::read_transfer_id(&mut stream).await?;

        let Some(offer) = link.accepted_in.lock().expect("khoá lời mời").remove(&id) else {
            // Đầu kia gửi file ta chưa đồng ý nhận. Dừng stream để nó khỏi
            // đẩy tiếp cả gigabyte vào chỗ không ai đọc.
            stream.stop(0u32.into()).ok();
            tracing::warn!(id, "bỏ stream file chưa được chấp nhận");
            continue;
        };

        let events = link.ctx.events.clone();
        let outbox = link.outbox.clone();
        let dir = link.ctx.config.download_dir.clone();
        let size = offer.size;
        tokio::spawn(async move {
            let mut acked = 0u64;
            let result = rd_transport::recv_file(stream, &offer, &dir, |done| {
                events.send(NetEvent::TransferProgress { id, done });
                if done - acked >= ACK_EVERY || done == size {
                    acked = done;
                    let _ = outbox.send(W::ack(FileChunkAck {
                        transfer_id: id,
                        received_bytes: done,
                    }));
                }
            })
            .await;

            match result {
                Ok(path) => events.send(NetEvent::TransferDone { id, path }),
                Err(err) => events.send(NetEvent::TransferFailed {
                    id,
                    reason: err.to_string(),
                }),
            }
        });
    }
}

/// Ping đo đồng hồ và báo số liệu đường truyền lên giao diện.
async fn heartbeat<W: Wire>(link: Arc<Link<W>>) -> anyhow::Result<()> {
    let mut ticker = tokio::time::interval(PING_INTERVAL);
    loop {
        ticker.tick().await;
        link.emit(NetEvent::Link(link.session.link_stats()));
        // Chỉ viewer ping: một chiều là đủ để tính lệch đồng hồ, và bên đo phải
        // là bên cần con số (viewer mới là bên hiển thị độ trễ).
        if link.hooks.clock.is_some() {
            if let Some(message) = W::ping(now_us()) {
                link.send(message);
            }
        }
    }
}

async fn command_loop<W: Wire>(
    link: Arc<Link<W>>,
    commands: &mut UnboundedReceiver<UiCommand>,
) -> anyhow::Result<()> {
    while let Some(command) = commands.recv().await {
        match command {
            UiCommand::Input(events) => {
                for event in events {
                    if let Some(message) = W::input(event) {
                        link.send(message);
                    }
                }
            }
            UiCommand::Chat(message) => link.send(W::chat(message)),
            UiCommand::SendFile(path) => spawn_offer(&link, path),
            UiCommand::AcceptFile(id) => {
                let offer = link.offered_in.lock().expect("khoá lời mời").remove(&id);
                if let Some(offer) = offer {
                    link.accepted_in
                        .lock()
                        .expect("khoá lời mời")
                        .insert(id, offer);
                    link.send(W::accept(id));
                }
            }
            UiCommand::RejectFile(id) => {
                link.offered_in.lock().expect("khoá lời mời").remove(&id);
                link.send(W::reject(id));
            }
            UiCommand::RequestKeyframe => {
                if let Some(message) = W::keyframe() {
                    link.send(message);
                }
            }
            UiCommand::SetBitrate(kbps) => {
                if let Some(message) = W::quality(QualityRequest {
                    target_bitrate_kbps: kbps,
                    ..QualityRequest::default()
                }) {
                    link.send(message);
                }
            }
        }
    }
    // Giao diện đã đóng kênh lệnh: chương trình đang thoát.
    Ok(())
}

/// Băm file rồi gửi lời mời.
///
/// Chạy trong task riêng vì băm một file vài GB mất vài giây; làm ngay trong
/// vòng lệnh thì chat và input đứng hình suốt quãng đó.
fn spawn_offer<W: Wire>(link: &Arc<Link<W>>, path: PathBuf) {
    let id = W::space().make_id(link.counter.fetch_add(1, Ordering::Relaxed) + 1);
    let link = link.clone();
    tokio::spawn(async move {
        match rd_transport::prepare_offer(&path, id).await {
            Ok(offer) => {
                link.pending_out
                    .lock()
                    .expect("khoá lời mời")
                    .insert(id, (offer.clone(), path));
                link.emit(NetEvent::OutgoingOffer(offer.clone()));
                link.send(W::offer(offer));
            }
            Err(err) => link.emit(NetEvent::TransferFailed {
                id,
                reason: format!("không đọc được file: {err}"),
            }),
        }
    });
}
