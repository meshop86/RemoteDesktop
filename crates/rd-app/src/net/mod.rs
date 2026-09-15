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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context as _;
use rd_protocol::control::{ChatMessage, FileOffer, InputEvent, MonitorInfo};
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
    /// Cho máy cùng tài khoản Tailscale vào mà không cần mật khẩu.
    ///
    /// Chỉ có nghĩa với host. Đổi lại sự tiện ấy, ranh giới an toàn chuyển sang
    /// tailnet: ai vào được tài khoản Tailscale là vào được máy này.
    pub trust_tailnet: bool,
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
    /// Đường ghi clipboard của máy này, cho phần mạng dùng thẳng.
    ///
    /// Không đi vòng qua giao diện vì giao diện có thể đang thu nhỏ, mà cửa sổ
    /// thu nhỏ thì luồng vẽ đứng im — clipboard sẽ chỉ sang được máy kia khi
    /// người dùng bấm vào cửa sổ.
    pub clipboard: crate::os::ClipboardSink,
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
    /// Các màn hình của host và màn hình đang xem.
    Monitors {
        list: Vec<MonitorInfo>,
        active: u8,
    },
    OutgoingOffer(FileOffer),
    IncomingOffer(FileOffer),
    TransferAccepted(u64),
    TransferRejected(u64),
    /// Một lần truyền bị bỏ giữa chừng. `by_peer` để giao diện nói đúng ai đổi ý.
    TransferCancelled {
        id: u64,
        by_peer: bool,
    },
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
    /// Clipboard máy này vừa đổi — đẩy sang máy kia.
    Clipboard(String),
    SendFile(PathBuf),
    AcceptFile(u64),
    RejectFile(u64),
    /// Đổi ý: bỏ lời mời chưa được trả lời, hoặc cắt ngang lần truyền đang chạy.
    CancelFile(u64),
    RequestKeyframe,
    SetBitrate(u32),
    /// Viewer xin xem màn hình khác của host.
    SelectMonitor(u8),
}

/// Khoảng cách tối thiểu giữa hai lần tự xin keyframe.
///
/// Keyframe nặng gấp nhiều lần frame thường, mà một lời xin phải đi hết một
/// vòng mạng rồi mới có hình về — xin dồn dập chỉ làm nghẽn đúng lúc đường
/// truyền đang yếu. 250 ms đủ cho gần như mọi đường truyền thực tế đáp lại.
const KEYFRAME_COOLDOWN_US: u64 = 250_000;

/// Cái chuông để phần mạng đánh thức luồng vẽ.
///
/// Không có nó thì giao diện phải vẽ lại liên tục mới kịp thấy frame mới —
/// tốn CPU cả những lúc màn hình đứng yên. Có nó, giao diện ngủ tới khi thực
/// sự có việc. `OnceLock` vì `egui::Context` chỉ tồn tại sau khi cửa sổ mở,
/// muộn hơn lúc phần mạng khởi động.
#[derive(Clone, Default)]
pub struct Waker(Arc<std::sync::OnceLock<egui::Context>>);

impl Waker {
    /// Gắn ngữ cảnh vẽ vào chuông. Gọi lại nhiều lần cũng không sao.
    pub fn attach(&self, ctx: &egui::Context) {
        if self.0.get().is_none() {
            let _ = self.0.set(ctx.clone());
        }
    }

    fn wake(&self) {
        if let Some(ctx) = self.0.get() {
            ctx.request_repaint();
        }
    }
}

/// Ngữ cảnh dùng chung cho mọi task của phần mạng.
pub struct Context {
    pub config: NetConfig,
    pub events: EventSink,
    pub frames: SyncSender<PipelineFrame>,
    pub stop: Arc<AtomicBool>,
    /// Chuông đánh thức luồng vẽ mỗi khi có frame mới.
    waker: Waker,
    /// Đường vòng lại chính kênh lệnh của giao diện. Nhờ nó, chỗ nào trong phần
    /// mạng cũng tự phát được lệnh mà không cần biết đường đi tới bộ gửi.
    commands: tokio::sync::mpsc::UnboundedSender<UiCommand>,
    /// Lần tự xin keyframe gần nhất, tính bằng micro giây đồng hồ hệ thống.
    last_keyframe_us: AtomicU64,
}

impl Context {
    /// Bằng chứng biết mật khẩu, dành riêng cho *một* kết nối.
    ///
    /// Không gửi mật khẩu trần, mà cũng không gửi băm trần: băm trần thì kẻ
    /// đứng giữa (một rendezvous server bị chiếm chẳng hạn) chỉ cần bắt lại
    /// rồi phát sang host thật là vào được mà chẳng cần biết mật khẩu. Trộn
    /// thêm ràng buộc kênh của chính kết nối này thì con số bắt được ở kết nối
    /// A không dùng lại được ở kết nối B.
    pub fn auth(&self, session: &rd_transport::Session) -> anyhow::Result<[u8; 32]> {
        let binding = session
            .channel_binding()
            .context("không lấy được ràng buộc kênh của kết nối")?;
        let key = blake3::hash(self.config.password.as_bytes());
        Ok(*blake3::keyed_hash(key.as_bytes(), &binding).as_bytes())
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

    /// Xin keyframe bất kể thời gian chờ. Dùng khi bộ giải mã vừa được dựng
    /// lại: nó không còn khung tham chiếu nào, chờ hết cooldown là chờ vô ích.
    pub fn force_keyframe(&self) {
        self.last_keyframe_us.store(now_us(), Ordering::Relaxed);
        let _ = self.commands.send(UiCommand::RequestKeyframe);
    }

    /// Đẩy frame lên giao diện, bỏ frame cũ nếu giao diện chưa vẽ kịp.
    pub fn push_frame(&self, frame: PipelineFrame) {
        match self.frames.try_send(frame) {
            Ok(()) => self.waker.wake(),
            Err(std::sync::mpsc::TrySendError::Full(_)) => {}
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
pub struct EventSink {
    tx: std::sync::mpsc::Sender<NetEvent>,
    waker: Waker,
}

impl EventSink {
    pub fn send(&self, event: NetEvent) {
        let _ = self.tx.send(event);
        self.waker.wake();
    }

    pub fn status(&self, text: impl Into<String>) {
        let text = text.into();
        tracing::info!("{text}");
        self.send(NetEvent::Status(text));
    }
}

/// Báo lên giao diện phiên này đang đi thẳng hay vòng qua máy chủ DERP.
///
/// Đây là thứ quyết định chất lượng hình khi chạy trên Tailscale: nối thẳng thì
/// độ trễ bằng mạng thật, còn vòng qua DERP là gói tin phải chạy tới máy chủ
/// trung chuyển của Tailscale rồi mới quay về, thường thêm vài chục ms và giới
/// hạn băng thông. Người dùng không thấy được điều đó nếu không nói ra.
///
/// Chờ vài giây rồi mới hỏi vì Tailscale luôn khởi động bằng DERP và chỉ nâng
/// cấp lên nối thẳng sau khi đục NAT xong — hỏi ngay lúc vừa nối thì lần nào
/// cũng thấy DERP.
/// `derp` nhận trần bitrate của đường vòng, tính bằng kbps; 0 nghĩa là không
/// kẹp. Chỉ phía host truyền vào — nó là bên chọn mức mã hoá.
pub fn bao_duong_di(
    events: EventSink,
    ip: std::net::IpAddr,
    derp: Option<Arc<AtomicU32>>,
) -> AbortOnDrop<()> {
    AbortOnDrop(tokio::spawn(async move {
        if !crate::tailscale::is_tailnet_ip(ip) {
            return;
        }
        // Hỏi lại vài lượt: Tailscale hay bắt đầu bằng DERP rồi mới nâng lên
        // nối thẳng, mà lần nâng ấy có khi tới sau cả chục giây. Hỏi một lần
        // là báo sai rồi hạ bitrate oan cho cả phiên.
        let mut truoc = None;
        for lan in 0..DO_DUONG_SO_LAN {
            tokio::time::sleep(DO_DUONG_CACH * (lan + 1)).await;
            let Ok(Some(path)) =
                tokio::task::spawn_blocking(move || crate::tailscale::path_to(ip)).await
            else {
                continue;
            };
            let vong = matches!(path, crate::tailscale::Path::Derp(_));
            if let Some(derp) = &derp {
                let kbps = if vong { rd_session::rate::DERP_KBPS } else { 0 };
                derp.store(kbps, Ordering::Relaxed);
            }
            // Chỉ nói khi đường đổi, không nhắc lại điều đã nói.
            if truoc == Some(vong) {
                continue;
            }
            truoc = Some(vong);
            match path {
                crate::tailscale::Path::Direct(addr) => {
                    events.status(format!("đường đi: nối thẳng tới {addr}"));
                }
                crate::tailscale::Path::Derp(vung) => {
                    let vung = if vung.is_empty() {
                        String::new()
                    } else {
                        format!(" ({vung})")
                    };
                    events.status(format!(
                        "đường đi: vòng qua máy trung chuyển DERP{vung} — hạ bitrate cho đỡ giật"
                    ));
                }
            }
        }
    }))
}

/// Hỏi đường đi bao nhiêu lần sau khi nối, và cách nhau bao lâu. Khoảng cách
/// nhân dần theo lượt nên lượt cuối rơi vào khoảng nửa phút sau khi vào phiên.
const DO_DUONG_SO_LAN: u32 = 4;
const DO_DUONG_CACH: std::time::Duration = std::time::Duration::from_secs(3);

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
    waker: Waker,
    thread: Option<JoinHandle<()>>,
}

impl NetHandle {
    pub fn spawn(config: NetConfig) -> anyhow::Result<Self> {
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let (frame_tx, frame_rx) = sync_channel(1);
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let waker = Waker::default();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("rd-net")
            .build()?;

        let ctx = Arc::new(Context {
            config,
            events: EventSink {
                tx: event_tx,
                waker: waker.clone(),
            },
            frames: frame_tx,
            stop: stop.clone(),
            waker: waker.clone(),
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
            waker,
            thread: Some(thread),
        })
    }

    /// Cho phần mạng biết đường đánh thức luồng vẽ. Gọi mỗi lần vẽ cho gọn,
    /// lần đầu là lần duy nhất có tác dụng.
    pub fn attach(&self, ctx: &egui::Context) {
        self.waker.attach(ctx);
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

    /// Đầu gửi lệnh, sao chép được để luồng khác dùng.
    ///
    /// Dùng cho những việc không được chờ luồng vẽ giao diện — clipboard chẳng
    /// hạn, vì cửa sổ thu nhỏ là luồng ấy đứng im.
    pub fn commands(&self) -> tokio::sync::mpsc::UnboundedSender<UiCommand> {
        self.commands.clone()
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
