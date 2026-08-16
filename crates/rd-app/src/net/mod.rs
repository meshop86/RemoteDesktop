//! Nối phần mạng vào giao diện.
//!
//! Giao diện chạy trên luồng chính và không được phép chờ; phần mạng là async
//! và chờ suốt. Chỗ nối giữa hai thế giới đó gom hết vào [`NetHandle`]: một
//! runtime tokio nằm trên luồng riêng, giao tiếp với giao diện bằng đúng ba
//! kênh — lệnh đi xuống, sự kiện đi lên, và frame video đi lên theo đường riêng.
//!
//! Frame đi đường riêng vì nó có luật riêng: giao diện chỉ cần frame *mới nhất*.
//! Kênh sự kiện thì ngược lại, mất một sự kiện là mất luôn (một lời mời file
//! không đến nơi thì người dùng ngồi chờ mãi), nên nó không giới hạn kích thước.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

use rd_protocol::control::{ChatMessage, FileOffer, InputEvent};
use rd_signal::PeerId;
use rd_transport::LinkStats;
use rd_viewer::pipeline::{PipelineFrame, PipelineInfo};

pub mod host;
pub mod link;
pub mod viewer;

/// Tên server dùng khi bắt tay TLS.
///
/// Chứng chỉ là tự ký và ta ghim theo vân tay (hoặc chấp nhận bất kỳ, xem
/// [`NetConfig::peer_fingerprint`]), nên tên này chỉ cần hai đầu thống nhất.
pub const SERVER_NAME: &str = "rd-host";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Máy cho xem màn hình và nhận điều khiển.
    Host,
    /// Máy xem và điều khiển máy kia.
    Viewer,
}

/// Cách viewer tìm tới host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerAddress {
    /// Biết thẳng địa chỉ — dùng trong mạng nhà hoặc khi host có IP công cộng.
    Direct(SocketAddr),
    /// Chỉ biết mã 9 chữ số; phải hỏi rendezvous server.
    Code(PeerId),
}

#[derive(Debug, Clone)]
pub struct NetConfig {
    pub role: Role,
    pub bind: SocketAddr,
    /// Rendezvous server. Không có thì chỉ nối thẳng được.
    pub rendezvous: Option<SocketAddr>,
    /// Vân tay chứng chỉ của rendezvous server. `None` là chấp nhận bất kỳ —
    /// chỉ nên dùng khi tự chạy server trong mạng nhà.
    pub rendezvous_fingerprint: Option<[u8; 32]>,
    /// Vân tay chứng chỉ của host (viewer ghim). `None` là chấp nhận bất kỳ;
    /// lúc đó thứ duy nhất chặn kẻ đứng giữa là mật khẩu phiên.
    pub peer_fingerprint: Option<[u8; 32]>,
    /// Nơi cần tới, chỉ dùng khi [`Role::Viewer`].
    pub peer: Option<PeerAddress>,
    /// Tên hiện cho đầu kia thấy.
    pub name: String,
    /// Mật khẩu phiên. Host sinh ra và hiện lên màn hình, viewer phải gõ đúng.
    pub password: String,
    pub target_fps: u32,
    pub bitrate_kbps: u32,
    /// Card đồ hoạ của **máy đang xem** có hiển thị được 10-bit không.
    pub allow_10bit: bool,
    pub download_dir: PathBuf,
}

/// Việc xảy ra ở phần mạng mà giao diện cần biết.
#[derive(Debug)]
pub enum NetEvent {
    /// Câu trạng thái cho người dùng đọc ("đang đục lỗ NAT...").
    Status(String),
    /// Mã của máy này, để đọc cho người bên kia.
    Code(PeerId),
    Connected {
        peer: SocketAddr,
        name: String,
        /// Đường đi thật: nối thẳng hay phải qua relay.
        relayed: bool,
    },
    /// Thông số video, có sau khi bắt tay xong.
    Info(Box<PipelineInfo>),
    OutgoingOffer(FileOffer),
    IncomingOffer(FileOffer),
    TransferAccepted(u64),
    TransferRejected(u64),
    TransferProgress {
        id: u64,
        done: u64,
    },
    TransferDone {
        id: u64,
        path: PathBuf,
    },
    TransferFailed {
        id: u64,
        reason: String,
    },
    Chat(ChatMessage),
    Link(LinkStats),
    /// Độ trễ một chiều đo được sau khi đồng bộ đồng hồ, tính bằng micro giây.
    Latency(u64),
    /// Đầu kia rời đi nhưng phần mạng vẫn sống. Chỉ host gửi cái này: nó quay
    /// lại chờ máy tiếp theo chứ không tắt như viewer.
    PeerLeft,
    /// Phiên kết thúc. Host quay lại chờ máy khác, viewer thì dừng hẳn.
    Disconnected(String),
}

/// Lệnh từ giao diện xuống phần mạng.
#[derive(Debug)]
pub enum UiCommand {
    Input(Vec<InputEvent>),
    Chat(ChatMessage),
    SendFile(PathBuf),
    AcceptFile(u64),
    RejectFile(u64),
    RequestKeyframe,
    SetBitrate(u32),
}

/// Khoảng cách tối thiểu giữa hai lần tự xin keyframe.
///
/// Keyframe nặng gấp nhiều lần frame thường, mà một lời xin phải đi hết một
/// vòng mạng rồi mới có hình về — xin dồn dập chỉ làm nghẽn đúng lúc đường
/// truyền đang yếu. 250 ms đủ cho gần như mọi đường truyền thực tế đáp lại.
const KEYFRAME_COOLDOWN_US: u64 = 250_000;

/// Ngữ cảnh dùng chung cho mọi task của phần mạng.
pub struct Context {
    pub config: NetConfig,
    pub events: EventSink,
    pub frames: SyncSender<PipelineFrame>,
    pub stop: Arc<AtomicBool>,
    /// Đường vòng lại chính kênh lệnh của giao diện. Nhờ nó, chỗ nào trong phần
    /// mạng cũng tự phát được lệnh mà không cần biết đường đi tới bộ gửi.
    commands: tokio::sync::mpsc::UnboundedSender<UiCommand>,
    /// Lần tự xin keyframe gần nhất, tính bằng micro giây đồng hồ hệ thống.
    last_keyframe_us: AtomicU64,
}

impl Context {
    /// Băm mật khẩu phiên. Đây là thứ đi trên dây, không phải mật khẩu trần.
    pub fn auth(&self) -> [u8; 32] {
        *blake3::hash(self.config.password.as_bytes()).as_bytes()
    }

    /// Xin host phát keyframe vì chuỗi dự đoán đã đứt (mất gói, hoặc ta phải
    /// bỏ frame vì giải mã không kịp). Trả `true` nếu lời xin thực sự được gửi.
    ///
    /// Không có hàm này thì mất *một* gói là hình đứng yên cho tới keyframe
    /// định kỳ kế tiếp — tới 10 giây. Trên Internet thật, mất gói là chuyện
    /// thường xuyên chứ không phải sự cố.
    pub fn ask_keyframe(&self) -> bool {
        let now = now_us();
        let last = self.last_keyframe_us.load(Ordering::Relaxed);
        if now.saturating_sub(last) < KEYFRAME_COOLDOWN_US {
            return false;
        }
        // Đổi có điều kiện: hai luồng cùng phát hiện đứt chuỗi trong cùng một
        // frame thì chỉ một lời xin được gửi đi.
        if self
            .last_keyframe_us
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        let _ = self.commands.send(UiCommand::RequestKeyframe);
        true
    }

    /// Đẩy frame lên giao diện, bỏ frame cũ nếu giao diện chưa vẽ kịp.
    pub fn push_frame(&self, frame: PipelineFrame) {
        match self.frames.try_send(frame) {
            Ok(()) | Err(std::sync::mpsc::TrySendError::Full(_)) => {}
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                self.stop.store(true, Ordering::Relaxed);
            }
        }
    }
}

/// Đầu gửi sự kiện lên giao diện.
///
/// Bọc lại thay vì dùng thẳng `Sender` để chỗ gọi không phải bận tâm chuyện
/// giao diện đã đóng hay chưa — lúc đó gửi thất bại là chuyện đúng, không phải
/// lỗi cần xử lý.
#[derive(Clone)]
pub struct EventSink(std::sync::mpsc::Sender<NetEvent>);

impl EventSink {
    pub fn send(&self, event: NetEvent) {
        let _ = self.0.send(event);
    }

    pub fn status(&self, text: impl Into<String>) {
        let text = text.into();
        tracing::info!("{text}");
        self.send(NetEvent::Status(text));
    }
}

/// Task bị huỷ khi guard rời phạm vi.
///
/// `JoinHandle` bị thả *không* dừng task — nó chạy tiếp trong nền, giữ nguyên
/// kết nối và tiếp tục ghi vào kênh. Với vòng phục vụ nối tiếp nhiều phiên,
/// đó là rò rỉ tích luỹ theo số lần đứt nối.
pub struct AbortOnDrop<T>(pub tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Tay cầm phía giao diện.
pub struct NetHandle {
    events: Receiver<NetEvent>,
    frames: Receiver<PipelineFrame>,
    commands: tokio::sync::mpsc::UnboundedSender<UiCommand>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl NetHandle {
    pub fn spawn(config: NetConfig) -> anyhow::Result<Self> {
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let (frame_tx, frame_rx) = sync_channel(1);
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("rd-net")
            .build()?;

        let ctx = Arc::new(Context {
            config,
            events: EventSink(event_tx),
            frames: frame_tx,
            stop: stop.clone(),
            commands: cmd_tx.clone(),
            last_keyframe_us: AtomicU64::new(0),
        });

        let thread = std::thread::Builder::new()
            .name("rd-net".into())
            .spawn(move || {
                let events = ctx.events.clone();
                let stop = ctx.stop.clone();
                runtime.block_on(async move {
                    let work = async {
                        match ctx.config.role {
                            Role::Host => host::run(ctx.clone(), cmd_rx).await,
                            Role::Viewer => viewer::run(ctx.clone(), cmd_rx).await,
                        }
                    };
                    tokio::select! {
                        result = work => {
                            let reason = match result {
                                Ok(()) => "kết thúc".to_string(),
                                Err(err) => {
                                    tracing::error!("phần mạng dừng: {err:#}");
                                    // `{:#}` để lấy cả chuỗi nguyên nhân của
                                    // anyhow; chỉ câu ngoài cùng thường vô dụng
                                    // ("không nối được") mà giấu mất lý do thật.
                                    format!("{err:#}")
                                }
                            };
                            events.send(NetEvent::Disconnected(reason));
                        }
                        _ = watch_stop(&stop) => {}
                    }
                });
                // Không chờ task nào dọn xong: chúng chỉ giữ socket, mà tiến
                // trình đang thoát thì hệ điều hành đóng hộ.
                runtime.shutdown_background();
            })?;

        Ok(Self {
            events: event_rx,
            frames: frame_rx,
            commands: cmd_tx,
            stop,
            thread: Some(thread),
        })
    }

    /// Rút hết sự kiện đang chờ. Gọi mỗi lần vẽ.
    pub fn poll_events(&self) -> Vec<NetEvent> {
        self.events.try_iter().collect()
    }

    /// Frame mới nhất, bỏ mọi frame cũ hơn.
    pub fn latest_frame(&self) -> Option<PipelineFrame> {
        let mut newest = None;
        while let Ok(frame) = self.frames.try_recv() {
            newest = Some(frame);
        }
        newest
    }

    pub fn send(&self, command: UiCommand) {
        let _ = self.commands.send(command);
    }
}

impl Drop for NetHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Rút kênh frame: luồng mạng có thể đang chặn ở `try_send`... không
        // chặn thật, nhưng rút vẫn giúp thả sớm các texture GPU đang giữ.
        while self.frames.try_recv().is_ok() {}
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

/// Chờ tới khi có lệnh dừng. Hỏi theo nhịp thay vì dùng tín hiệu vì lệnh dừng
/// chỉ xảy ra đúng một lần trong đời chương trình — độ trễ 100 ms không ai thấy.
async fn watch_stop(stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
