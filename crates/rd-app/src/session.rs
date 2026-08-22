//! Một phiên đang chạy: giữ trạng thái và vẽ nó ra.
//!
//! Cùng một kiểu dùng cho cả hai vai. Khác nhau đúng hai chỗ: host không có
//! video để vẽ (nó là bên *bị* xem), và chỉ viewer mới bắt chuột phím gửi đi.
//! Phần chat với truyền file thì đối xứng hoàn toàn — gộp làm một chỗ để sửa
//! một lần là cả hai bên cùng đúng.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui;

#[cfg(target_os = "windows")]
use rd_codec::mediafoundation::DecodedFrame;
#[cfg(target_os = "macos")]
use rd_codec::videotoolbox::DecodedFrame;

use rd_protocol::control::{ChatMessage, FileOffer};
use rd_signal::PeerId;
use rd_transport::LinkStats;
use rd_viewer::input_capture::InputCapture;
use rd_viewer::metrics::Metrics;
use rd_viewer::pipeline::PipelineInfo;

use crate::app::Prefs;
use crate::net::{NetConfig, NetEvent, NetHandle, Role, UiCommand};
use crate::os::{Clipboard, ClipboardSink, FilePicker};
use crate::ui;

/// Một địa chỉ đọc được cho người kia gõ vào.
struct Address {
    /// Đường nào: mạng nhà, hay qua Tailscale.
    label: &'static str,
    value: String,
}

/// Địa chỉ IPv4 của máy này trong mạng nội bộ.
///
/// std không có cách nào liệt kê card mạng, nên hỏi ngược hệ điều hành: mở một
/// socket UDP rồi "nối" tới một địa chỉ ngoài internet. UDP không bắt tay nên
/// không một gói tin nào rời khỏi máy — hệ điều hành chỉ tra bảng định tuyến,
/// chọn card mạng sẽ dùng để đi ra, rồi gán địa chỉ nguồn. Đọc lại địa chỉ đó
/// là xong, và nó đúng là địa chỉ mà máy bên kia trong cùng mạng gõ vào được.
///
/// Máy nối nhiều mạng cùng lúc (vừa Wi-Fi vừa dây) thì cách này cho ra đường
/// mặc định, tức đường mà máy kia nhiều khả năng cũng đang ở.
pub fn lan_ip() -> Option<std::net::Ipv4Addr> {
    dia_chi_nguon("1.1.1.1")
}

/// Địa chỉ máy này sẽ mang khi gửi gói tin tới `dich`.
pub fn dia_chi_nguon(dich: &str) -> Option<std::net::Ipv4Addr> {
    let socket = std::net::UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    // Không có đường tới đó thì `connect` trả lỗi và ta đành chịu — thà không
    // hiện gì còn hơn hiện một địa chỉ sai.
    socket.connect((dich, 53)).ok()?;
    match socket.local_addr().ok()? {
        std::net::SocketAddr::V4(addr) => Some(*addr.ip()),
        std::net::SocketAddr::V6(_) => None,
    }
}

/// Những địa chỉ máy kia gõ vào được để tới máy này.
fn host_addresses(port: u16, tailscale_ip: Option<String>) -> Vec<Address> {
    let mut out = Vec::new();
    if let Some(ip) = lan_ip() {
        out.push(Address {
            label: "trong mạng nhà",
            value: format!("{ip}:{port}"),
        });
    }
    if let Some(ip) = tailscale_ip {
        out.push(Address {
            label: "qua Tailscale, ở mạng nào cũng được",
            value: format!("{ip}:{port}"),
        });
    }
    out
}

/// Bật/tắt HUD. Trùng với phím tắt của chế độ thử tại chỗ để khỏi phải nhớ hai
/// phím cho cùng một việc.
const HUD_KEY: egui::Key = egui::Key::F10;
/// Bật/tắt gửi chuột phím sang máy kia.
const CONTROL_KEY: egui::Key = egui::Key::F9;
/// Bật/tắt bảng chat và file.
const PANEL_KEY: egui::Key = egui::Key::F8;

/// Bung/thu toàn màn hình.
const FULLSCREEN_KEY: egui::Key = egui::Key::F11;

/// Ẩn/hiện thanh công cụ.
const CHROME_KEY: egui::Key = egui::Key::F12;

/// Chuột lên sát mép trên bao nhiêu pixel thì thanh công cụ hiện lại.
/// Chỗ phần bên trái thanh công cụ chiếm: thoát phiên, trạng thái, tên máy kia.
const BE_NGANG_THANH_TRAI: f32 = 380.0;

const REVEAL_EDGE: f32 = 6.0;

/// Chờ bao lâu trước lần nối lại đầu tiên.
const RECONNECT_FIRST: Duration = Duration::from_secs(2);

/// Trần thời gian chờ giữa hai lần thử. Máy kia mất điện có khi cả tiếng mới
/// bật lại, nhưng cứ 15 giây thử một lần là đủ nhanh mà không tốn gì.
const RECONNECT_MAX: Duration = Duration::from_secs(15);

/// Hình đứng lâu hơn chừng này thì báo cho người dùng biết.
const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(3);

// ───────────────────────────── chat ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Who {
    Me,
    Peer,
    /// Máy tự nói: "đã nhận xong tệp...", "viewer đã ngắt".
    System,
}

pub struct ChatEntry {
    pub who: Who,
    pub name: String,
    pub body: String,
    pub at_ms: u64,
}

#[derive(Default)]
pub struct ChatLog {
    entries: Vec<ChatEntry>,
}

impl ChatLog {
    pub fn receive(&mut self, message: ChatMessage) {
        self.entries.push(ChatEntry {
            who: Who::Peer,
            name: message.from,
            body: message.body,
            at_ms: message.sent_at_ms,
        });
    }

    pub fn mine(&mut self, name: &str, body: String) {
        self.entries.push(ChatEntry {
            who: Who::Me,
            name: name.to_string(),
            body,
            at_ms: now_ms(),
        });
    }

    pub fn system(&mut self, body: impl Into<String>) {
        self.entries.push(ChatEntry {
            who: Who::System,
            name: String::new(),
            body: body.into(),
            at_ms: now_ms(),
        });
    }

    pub fn entries(&self) -> &[ChatEntry] {
        &self.entries
    }
}

// ─────────────────────────── truyền file ───────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferState {
    /// Đã mời, đang chờ đầu kia bấm nhận.
    Offered,
    Active,
    Done(Option<PathBuf>),
    Rejected,
    /// Đổi ý giữa chừng. `by_peer` để nói đúng ai là người bỏ.
    Cancelled {
        by_peer: bool,
    },
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Out,
    In,
}

pub struct Transfer {
    pub id: u64,
    pub name: String,
    pub size: u64,
    pub done: u64,
    pub direction: Direction,
    pub state: TransferState,
    started: Instant,
}

impl Transfer {
    pub fn ratio(&self) -> f32 {
        if self.size == 0 {
            return 1.0;
        }
        (self.done as f32 / self.size as f32).clamp(0.0, 1.0)
    }

    /// Tốc độ trung bình từ lúc bắt đầu, byte mỗi giây.
    pub fn rate(&self) -> f64 {
        let secs = self.started.elapsed().as_secs_f64().max(1e-3);
        self.done as f64 / secs
    }
}

#[derive(Default)]
pub struct Transfers {
    list: Vec<Transfer>,
}

impl Transfers {
    fn push(&mut self, offer: FileOffer, direction: Direction, state: TransferState) {
        self.list.push(Transfer {
            id: offer.transfer_id,
            name: offer.name,
            size: offer.size,
            done: 0,
            direction,
            state,
            started: Instant::now(),
        });
    }

    pub fn offer_outgoing(&mut self, offer: FileOffer) {
        self.push(offer, Direction::Out, TransferState::Offered);
    }

    pub fn offer_incoming(&mut self, offer: FileOffer) {
        self.push(offer, Direction::In, TransferState::Offered);
    }

    fn find(&mut self, id: u64) -> Option<&mut Transfer> {
        self.list.iter_mut().find(|item| item.id == id)
    }

    /// Đầu kia đã bấm nhận: từ đây đồng hồ tốc độ mới có nghĩa.
    pub fn peer_accepted(&mut self, id: u64) {
        if let Some(item) = self.find(id) {
            item.state = TransferState::Active;
            item.started = Instant::now();
        }
    }

    pub fn accept_local(&mut self, id: u64) {
        if let Some(item) = self.find(id) {
            item.state = TransferState::Active;
            item.started = Instant::now();
        }
    }

    pub fn reject(&mut self, id: u64) {
        if let Some(item) = self.find(id) {
            item.state = TransferState::Rejected;
        }
    }

    pub fn progress(&mut self, id: u64, done: u64) {
        if let Some(item) = self.find(id) {
            item.done = done;
            if item.state == TransferState::Offered {
                item.state = TransferState::Active;
            }
        }
    }

    pub fn finish(&mut self, id: u64, path: Option<PathBuf>) {
        if let Some(item) = self.find(id) {
            item.done = item.size;
            item.state = TransferState::Done(path);
        }
    }

    /// Bỏ giữa chừng. Đè được cả trạng thái lỗi: khi đầu kia huỷ, luồng đang
    /// chạy ở máy này đứt trước nên nó kịp báo "lỗi" một nhịp — nhưng lý do
    /// thật là có người đổi ý, và đó mới là thứ đáng hiện.
    pub fn cancel(&mut self, id: u64, by_peer: bool) {
        if let Some(item) = self.find(id) {
            if !matches!(item.state, TransferState::Done(_)) {
                item.state = TransferState::Cancelled { by_peer };
            }
        }
    }

    pub fn fail(&mut self, id: u64, reason: String) {
        if let Some(item) = self.find(id) {
            // Đã huỷ rồi thì thôi: lỗi lúc này chỉ là tiếng vọng của cú cắt.
            if !matches!(item.state, TransferState::Cancelled { .. }) {
                item.state = TransferState::Failed(reason);
            }
        }
    }

    pub fn list(&self) -> &[Transfer] {
        &self.list
    }
}

// ─────────────────────────── phiên ───────────────────────────

/// Kết quả một lượt vẽ: giao diện có nên quay về màn hình đầu không.
pub enum Outcome {
    Continue,
    Leave,
}

pub struct SessionState {
    role: Role,
    net: NetHandle,
    my_name: String,
    password: String,
    bitrate_kbps: u32,

    info: Option<PipelineInfo>,
    current: Option<Arc<DecodedFrame>>,
    /// Lúc frame đang hiện được nhận. Dùng để biết hình còn sống hay đã đứng.
    current_at: Option<Instant>,
    metrics: Metrics,

    chat: ChatLog,
    transfers: Transfers,

    status: String,
    code: Option<PeerId>,
    /// Địa chỉ để đọc cho người kia — chỉ có ở vai host.
    addresses: Vec<Address>,
    peer: Option<String>,
    relayed: bool,
    link: Option<LinkStats>,
    latency_us: Option<u64>,
    /// Lý do phiên chết hẳn. Còn `None` là vẫn đang sống.
    ended: Option<String>,
    /// Cấu hình gốc, giữ lại để dựng lại phần mạng khi nối lại.
    config: NetConfig,
    /// Các màn hình của máy kia và màn hình đang xem.
    monitors: Vec<rd_protocol::control::MonitorInfo>,
    active_monitor: u8,
    /// Lúc sẽ thử nối lại. `None` là không có hẹn nào.
    retry_at: Option<Instant>,
    /// Đã thử nối lại mấy lần kể từ lần rớt gần nhất.
    attempts: u32,

    capture: InputCapture,
    control_on: bool,
    draft: String,
    /// Cầu nối clipboard. `None` là người dùng đã tắt đồng bộ — và lúc đó
    /// chương trình cũng thôi hẳn việc đọc clipboard, chứ không đọc rồi vứt.
    clipboard: Option<Clipboard>,
    /// Đường ghi clipboard mà phần mạng đang giữ. Sống suốt phiên, kể cả lúc
    /// người dùng tắt rồi bật lại đồng bộ.
    clip_sink: ClipboardSink,
    picker: FilePicker,
    /// Chặn hệ điều hành ngủ trong lúc chia sẻ màn hình. `None` ở vai viewer.
    _awake: Option<crate::os::KeepAwake>,
    show_hud: bool,
    show_panel: bool,
    /// Ẩn thanh trên và bảng bên để hình chiếm trọn cửa sổ.
    hide_chrome: bool,
    /// Số tệp đang lơ lửng trên cửa sổ chờ thả.
    hovering_files: usize,
    fullscreen: bool,
    /// Trạng thái toàn màn hình đã báo cho hệ điều hành. Lệch với `fullscreen`
    /// là còn một lệnh phải gửi — gửi mỗi khung thì cửa sổ giật liên tục.
    fullscreen_applied: bool,
    /// Thiết lập người dùng chọn ở trang cài đặt, chốt lúc vào phiên.
    prefs: Prefs,
    unread: u32,
    last_report: Instant,
}

/// Nhịp vẽ tối thiểu khi không có gì xảy ra, tính bằng mili giây.
const HEARTBEAT_MS: u64 = 250;

impl SessionState {
    /// `tailscale_ip` là địa chỉ tailnet của máy này, nếu Tailscale đang chạy.
    /// Lấy sẵn ở màn hình đầu rồi truyền vào, vì hỏi Tailscale là lời gọi chặn.
    pub fn start(
        mut config: NetConfig,
        tailscale_ip: Option<String>,
        prefs: Prefs,
    ) -> anyhow::Result<Self> {
        let role = config.role;
        let my_name = config.name.clone();
        let password = config.password.clone();
        let bitrate_kbps = config.bitrate_kbps;
        let addresses = match role {
            Role::Host => host_addresses(config.bind.port(), tailscale_ip),
            Role::Viewer => Vec::new(),
        };
        let clip_sink = ClipboardSink::default();
        config.clipboard = clip_sink.clone();
        let clipboard = prefs.clipboard_sync.then(|| Clipboard::start(&clip_sink));
        let net = NetHandle::spawn(config.clone())?;
        if let Some(clipboard) = &clipboard {
            clipboard.forward_to(Some(forwarder(&net, &clip_sink)));
        }
        Ok(Self {
            role,
            net,
            my_name,
            password,
            bitrate_kbps,
            info: None,
            current: None,
            current_at: None,
            metrics: Metrics::default(),
            chat: ChatLog::default(),
            transfers: Transfers::default(),
            status: "đang khởi động".into(),
            code: None,
            addresses,
            peer: None,
            relayed: false,
            link: None,
            latency_us: None,
            ended: None,
            config,
            monitors: Vec::new(),
            active_monitor: 0,
            retry_at: None,
            attempts: 0,
            capture: InputCapture::new(),
            // Mặc định tắt: vừa nối xong mà chuột đã điều khiển máy người khác
            // thì cú click để phóng to cửa sổ cũng rơi sang bên kia.
            control_on: false,
            draft: String::new(),
            clipboard,
            clip_sink,
            picker: FilePicker::default(),
            _awake: matches!(role, Role::Host).then(crate::os::KeepAwake::start),
            show_hud: prefs.show_hud,
            show_panel: true,
            hide_chrome: prefs.hide_chrome,
            hovering_files: 0,
            fullscreen: prefs.start_fullscreen,
            fullscreen_applied: false,
            prefs,
            unread: 0,
            last_report: Instant::now(),
        })
    }

    /// Tên máy kia tự khai lúc nối. `None` khi chưa nối được lần nào.
    pub fn peer_display_name(&self) -> Option<&str> {
        self.peer.as_deref()
    }

    pub fn update(&mut self, root: &mut egui::Ui) -> Outcome {
        let ctx = root.ctx().clone();
        // Phần mạng cần đường đánh thức luồng vẽ; chỉ lần gọi đầu có tác dụng.
        self.net.attach(&ctx);
        self.pump();
        self.hotkeys(&ctx);
        self.take_dropped_files(&ctx);

        let outcome = self.draw(root);

        self.metrics.repaints += 1;
        // Chỉ ghi khi thật sự có hình chạy qua. Host ngồi chờ cả buổi mà cứ 2
        // giây một dòng `fps=0` thì lúc cần đọc log lại phải bới qua hàng nghìn
        // dòng vô nghĩa.
        if self.last_report.elapsed() >= std::time::Duration::from_secs(2) && self.peer.is_some() {
            self.last_report = Instant::now();
            let summary = self.metrics.summary();
            tracing::info!(
                fps = summary.fps,
                p50_ms = summary.pipeline_p50_ms,
                p99_ms = summary.pipeline_p99_ms,
                mbps = summary.mbps,
                "thống kê phiên"
            );
        }

        // Frame mới và sự kiện mạng tự gõ chuông (`NetHandle::attach`), nên
        // không cần vẽ lại vô điều kiện — vẽ lại liên tục là đốt CPU cả lúc
        // màn hình bên kia đứng yên. Nhịp chậm này chỉ để HUD, đồng hồ và hẹn
        // giờ nối lại còn nhúc nhích.
        ctx.request_repaint_after(std::time::Duration::from_millis(HEARTBEAT_MS));
        outcome
    }

    fn pump(&mut self) {
        for event in self.net.poll_events() {
            match event {
                NetEvent::Status(text) => self.status = text,
                NetEvent::Code(id) => self.code = Some(id),
                NetEvent::Connected {
                    peer,
                    name,
                    relayed,
                } => {
                    self.peer = Some(name.clone());
                    self.relayed = relayed;
                    self.attempts = 0;
                    self.chat.system(format!(
                        "{name} ({peer}) đã nối{}",
                        if relayed { " qua relay" } else { "" }
                    ));
                }
                NetEvent::Info(info) => self.info = Some(*info),
                NetEvent::Monitors { list, active } => {
                    self.monitors = list;
                    self.active_monitor = active;
                }
                NetEvent::OutgoingOffer(offer) => {
                    self.chat.system(format!("đang gửi {}", offer.name));
                    self.transfers.offer_outgoing(offer);
                }
                NetEvent::IncomingOffer(offer) => {
                    self.chat
                        .system(format!("{} muốn gửi {}", self.peer_name(), offer.name));
                    let id = offer.transfer_id;
                    self.transfers.offer_incoming(offer);
                    if self.prefs.auto_accept_files {
                        self.transfers.accept_local(id);
                        self.net.send(UiCommand::AcceptFile(id));
                    } else {
                        // Có tệp chờ trả lời mà bảng đang giấu thì người gửi
                        // ngồi đợi mãi không hiểu vì sao. Mở bảng ra luôn.
                        self.show_panel = true;
                    }
                    self.unread += 1;
                }
                NetEvent::TransferAccepted(id) => self.transfers.peer_accepted(id),
                NetEvent::TransferRejected(id) => self.transfers.reject(id),
                NetEvent::TransferProgress { id, done } => self.transfers.progress(id, done),
                NetEvent::TransferDone { id, path } => {
                    self.chat.system(format!("xong: {}", path.display()));
                    self.transfers.finish(id, Some(path));
                }
                NetEvent::TransferCancelled { id, by_peer } => {
                    self.chat.system(if by_peer {
                        format!("{} đã huỷ một tệp", self.peer_name())
                    } else {
                        "đã huỷ một tệp".to_string()
                    });
                    self.transfers.cancel(id, by_peer);
                }
                NetEvent::TransferFailed { id, reason } => {
                    self.chat.system(format!("tệp lỗi: {reason}"));
                    self.transfers.fail(id, reason);
                }
                NetEvent::Chat(message) => {
                    self.chat.receive(message);
                    if !self.show_panel {
                        self.unread += 1;
                    }
                }
                NetEvent::Link(stats) => self.link = Some(stats),
                NetEvent::Latency(us) => self.latency_us = Some(us),
                NetEvent::PeerLeft => {
                    self.peer = None;
                    self.info = None;
                    self.current = None;
                    self.current_at = None;
                    self.latency_us = None;
                    self.chat.system("đầu kia đã ngắt");
                }
                NetEvent::Disconnected(reason) => {
                    self.chat.system(format!("phiên kết thúc: {reason}"));
                    self.ended = Some(reason);
                    if self.prefs.auto_reconnect {
                        self.schedule_retry();
                    }
                }
            }
        }

        // Hộp thoại chọn tệp nằm ở luồng khác nên kết quả về lúc nào không
        // biết trước — hỏi ở đây chứ không hỏi lúc vẽ, để người dùng có giấu
        // bảng tệp đi trong lúc chọn thì tệp vẫn được gửi.
        for path in self.picker.take() {
            self.net.send(UiCommand::SendFile(path));
        }

        if self.retry_at.is_some_and(|luc| Instant::now() >= luc) {
            self.reconnect();
        }

        if let Some(frame) = self.net.latest_frame() {
            self.metrics.push(
                frame.pipeline_us,
                frame.encode_us,
                frame.decode_us,
                frame.bytes,
                frame.keyframe,
            );
            // Đổi màn hình giữa phiên thì host không gửi lại `Welcome`, chỉ có
            // kích thước của frame nói lên độ phân giải mới. Không cập nhật thì
            // tỉ lệ khung hình và phép quy đổi toạ độ chuột đều lệch.
            if let Some(info) = self.info.as_mut()
                && (info.width != frame.frame.width || info.height != frame.frame.height)
            {
                info.width = frame.frame.width;
                info.height = frame.frame.height;
            }
            self.current = Some(frame.frame);
            self.current_at = Some(Instant::now());
        }
    }

    /// Hẹn giờ cho lần nối lại kế tiếp, giãn dần theo số lần đã hỏng.
    ///
    /// Giãn dần vì hai trường hợp hỏng khác hẳn nhau: rớt gói một nhịp thì thử
    /// lại sau 2 giây là vào ngay, còn máy kia mất điện thì có thử 100 lần
    /// trong hai phút cũng vô ích.
    fn schedule_retry(&mut self) {
        self.retry_at = Some(Instant::now() + backoff(self.attempts));
    }

    /// Dựng lại phần mạng, giữ nguyên chat và danh sách tệp của phiên cũ.
    fn reconnect(&mut self) {
        self.attempts += 1;
        self.retry_at = None;
        self.chat
            .system(format!("đang nối lại (lần {})", self.attempts));
        match NetHandle::spawn(self.config.clone()) {
            Ok(net) => {
                // `NetHandle` cũ rơi ở đây: luồng mạng của nó dừng, cổng được
                // trả lại trước khi cái mới xin cổng.
                if let Some(clipboard) = &self.clipboard {
                    clipboard.forward_to(Some(forwarder(&net, &self.clip_sink)));
                }
                self.net = net;
                self.ended = None;
                self.peer = None;
                self.info = None;
                self.current = None;
                self.current_at = None;
                self.status = "đang nối lại".into();
            }
            Err(err) => {
                self.status = format!("nối lại hỏng: {err}");
                if self.prefs.auto_reconnect {
                    self.schedule_retry();
                }
            }
        }
    }

    fn hotkeys(&mut self, ctx: &egui::Context) {
        // Bỏ qua khi con trỏ chữ đang nằm trong ô nhập: gõ "F8" vào tin nhắn
        // không được biến thành lệnh.
        if ctx.memory(|m| m.focused().is_some()) {
            return;
        }
        ctx.input_mut(|i| {
            if i.key_pressed(HUD_KEY) {
                self.show_hud = !self.show_hud;
            }
            if i.key_pressed(PANEL_KEY) {
                self.show_panel = !self.show_panel;
            }
            if i.key_pressed(CONTROL_KEY) && self.role == Role::Viewer {
                self.control_on = !self.control_on;
            }
            if i.key_pressed(FULLSCREEN_KEY) {
                self.fullscreen = !self.fullscreen;
            }
            if i.key_pressed(CHROME_KEY) {
                self.hide_chrome = !self.hide_chrome;
            }
            // Esc là phản xạ chung của mọi phần mềm xem toàn màn hình. Chỉ cho
            // nó thoát toàn màn hình, không cho nó thoát phiên — bấm nhầm mà
            // rụng cả kết nối thì khó chịu hơn nhiều.
            if i.key_pressed(egui::Key::Escape) && self.fullscreen {
                self.fullscreen = false;
            }
        });

        if self.fullscreen != self.fullscreen_applied {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
            self.fullscreen_applied = self.fullscreen;
        }
        if self.show_panel {
            self.unread = 0;
        }
    }

    /// Kéo tệp thả vào cửa sổ là cách gửi nhanh nhất, và không phải kéo thêm
    /// một thư viện hộp thoại chọn tệp về chỉ để làm việc này.
    fn take_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        for file in dropped {
            self.net
                .send(UiCommand::SendFile(file.path().to_path_buf()));
        }

        self.hovering_files = ctx.input(|i| i.raw.hovered_files.len());
    }

    /// Đang giữ tệp lơ lửng trên cửa sổ mà không có dấu hiệu gì thì người dùng
    /// không biết thả xuống có ăn thua không, và thường thả trượt ra ngoài.
    /// Phủ kín một lớp mờ cho chắc.
    fn draw_drop_overlay(&self, ui: &egui::Ui) {
        if self.hovering_files == 0 {
            return;
        }
        let area = ui.max_rect();
        let painter = ui.painter();
        painter.rect_filled(area, 0.0, egui::Color32::from_black_alpha(180));
        painter.text(
            area.center(),
            egui::Align2::CENTER_CENTER,
            format!(
                "Thả để gửi {} tệp sang {}",
                self.hovering_files,
                self.peer_name()
            ),
            egui::FontId::proportional(24.0),
            egui::Color32::WHITE,
        );
    }

    fn peer_name(&self) -> &str {
        self.peer.as_deref().unwrap_or("đầu kia")
    }

    fn connected(&self) -> bool {
        self.peer.is_some()
    }

    fn draw(&mut self, root: &mut egui::Ui) -> Outcome {
        let mut outcome = Outcome::Continue;

        // Giấu thanh đi mà không có đường lấy lại thì người dùng kẹt trong một
        // cửa sổ không nút nào — đưa chuột lên sát mép trên là nó hiện.
        let top = root.max_rect().top();
        let near_top = root
            .ctx()
            .pointer_latest_pos()
            .is_some_and(|pos| pos.y <= top + REVEAL_EDGE * 8.0 && pos.y >= top - REVEAL_EDGE);
        let show_chrome = !self.hide_chrome || near_top;

        if show_chrome {
            egui::Panel::top("thanh").show(root, |ui| {
                // Cửa sổ hẹp thì cho nhóm nút bên phải xuống hàng riêng. Layout
                // phải-sang-trái không biết co lại: hết chỗ là nó vẽ đè lên phần
                // bên trái, thành một mớ chữ chồng lên nhau.
                let mot_hang =
                    ui.available_width() >= self.be_ngang_thanh_phai() + BE_NGANG_THANH_TRAI;
                ui.horizontal(|ui| {
                    if ui.button("Thoát phiên").clicked() {
                        outcome = Outcome::Leave;
                    }
                    ui.separator();
                    ui.label(match self.role {
                        Role::Host => "Đang chia sẻ máy này",
                        Role::Viewer => "Đang xem máy khác",
                    });
                    ui.separator();
                    let cho_noi_lai = self
                        .retry_at
                        .map(|luc| luc.saturating_duration_since(Instant::now()));
                    let nhan_cho;
                    let (dot, color) = if let Some(cho) = cho_noi_lai {
                        nhan_cho = format!("nối lại sau {:.0}s", cho.as_secs_f32().ceil());
                        (nhan_cho.as_str(), ui::WARN)
                    } else if self.ended.is_some() {
                        ("đã dừng", ui::BAD)
                    } else if self.connected() {
                        ("đã nối", ui::GOOD)
                    } else {
                        ("đang chờ", ui::WARN)
                    };
                    ui.colored_label(color, dot);
                    if let Some(name) = &self.peer {
                        ui.label(name);
                    }
                    if self.retry_at.is_some() {
                        if ui.small_button("Thử ngay").clicked() {
                            self.retry_at = Some(Instant::now());
                        }
                        if ui.small_button("Thôi").clicked() {
                            self.retry_at = None;
                        }
                    } else if self.ended.is_some() && ui.small_button("Nối lại").clicked() {
                        self.attempts = 0;
                        self.retry_at = Some(Instant::now());
                    }

                    if mot_hang {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            self.thanh_phai(ui);
                        });
                    }
                });
                if !mot_hang {
                    ui.horizontal(|ui| self.thanh_phai(ui));
                }
            });
        }

        if self.show_panel && show_chrome {
            egui::Panel::right("bảng")
                .default_size(320.0)
                .show(root, |ui| self.draw_panel(ui));
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(root, |ui| match self.role {
                Role::Viewer => self.draw_video(ui),
                Role::Host => self.draw_host_card(ui),
            });

        egui::Area::new(egui::Id::new("lớp-thả-tệp"))
            .order(egui::Order::Foreground)
            .fixed_pos(root.max_rect().min)
            .show(root.ctx(), |ui| {
                ui.set_max_size(root.max_rect().size());
                self.draw_drop_overlay(ui);
            });

        outcome
    }

    /// Nhóm nút bên phải thanh công cụ: chat, chọn màn hình, và các nút của
    /// người đang xem.
    fn thanh_phai(&mut self, ui: &mut egui::Ui) {
        let label = if self.unread > 0 {
            format!("Chat/Tệp ({})", self.unread)
        } else {
            "Chat/Tệp".to_string()
        };
        if ui.selectable_label(self.show_panel, label).clicked() {
            self.show_panel = !self.show_panel;
            if self.show_panel {
                self.unread = 0;
            }
        }
        if self.role == Role::Viewer && self.monitors.len() > 1 {
            // Chỉ hiện khi máy kia có nhiều hơn một màn hình; một
            // màn hình mà vẫn bày ô chọn thì chỉ tổ chiếm chỗ.
            let hien_tai = self
                .monitors
                .iter()
                .find(|m| m.id == self.active_monitor)
                .map(|m| m.name.clone())
                .unwrap_or_else(|| "Màn hình".into());
            let mut chon = None;
            egui::ComboBox::from_id_salt("man-hinh")
                .selected_text(hien_tai)
                .show_ui(ui, |ui| {
                    for man in &self.monitors {
                        let nhan = format!(
                            "{} — {}x{}{}",
                            man.name,
                            man.width,
                            man.height,
                            if man.is_primary { " (chính)" } else { "" }
                        );
                        if ui
                            .selectable_label(man.id == self.active_monitor, nhan)
                            .clicked()
                        {
                            chon = Some(man.id);
                        }
                    }
                });
            if let Some(id) = chon.filter(|id| *id != self.active_monitor) {
                self.active_monitor = id;
                self.net.send(UiCommand::SelectMonitor(id));
                self.status = "đang đổi màn hình…".into();
            }
        }
        if self.role == Role::Viewer {
            let text = if self.control_on {
                "Điều khiển: bật (F9)"
            } else {
                "Điều khiển: tắt (F9)"
            };
            if ui.selectable_label(self.control_on, text).clicked() {
                self.control_on = !self.control_on;
            }
            if ui.button("Yêu cầu keyframe").clicked() {
                self.net.send(UiCommand::RequestKeyframe);
            }
            ui.label("kbps tối đa");
            // Là *trần*, không phải mức phát: máy kia tự dò xem
            // đường truyền tải nổi bao nhiêu rồi phát trong khoảng
            // đó. Hạ trần vẫn có ích khi muốn nhường băng thông cho
            // việc khác, hoặc khi đang dùng gói dữ liệu tính theo GB.
            if ui
                .add(
                    egui::DragValue::new(&mut self.bitrate_kbps)
                        .range(1_000..=200_000)
                        .speed(500),
                )
                .on_hover_text(
                    "Trần băng thông. Máy kia tự dò mức thật theo độ trễ và tỉ lệ mất gói.",
                )
                .changed()
            {
                self.net.send(UiCommand::SetBitrate(self.bitrate_kbps));
            }
            let full = if self.fullscreen {
                "Thu nhỏ (F11)"
            } else {
                "Toàn màn hình (F11)"
            };
            if ui.selectable_label(self.fullscreen, full).clicked() {
                self.fullscreen = !self.fullscreen;
            }
            if ui
                .selectable_label(self.hide_chrome, "Ẩn thanh (F12)")
                .on_hover_text("Đưa chuột lên sát mép trên là thanh hiện lại.")
                .clicked()
            {
                self.hide_chrome = !self.hide_chrome;
            }
        }
    }

    /// Ước lượng bề ngang nhóm ấy cần, để biết có nhét chung một hàng được không.
    fn be_ngang_thanh_phai(&self) -> f32 {
        let mut can = 130.0;
        if self.role == Role::Viewer {
            // Điều khiển, keyframe, trần kbps, toàn màn hình, ẩn thanh.
            can += 540.0;
            if self.monitors.len() > 1 {
                can += 170.0;
            }
        }
        can
    }

    fn draw_video(&mut self, ui: &mut egui::Ui) {
        let outer = ui.max_rect();
        ui::paint_backdrop(ui, outer);

        let Some(info) = self.info.clone() else {
            ui.centered_and_justified(|ui| {
                ui.label(egui::RichText::new(&self.status).size(16.0));
            });
            return;
        };

        let rect = ui::video_rect(outer, info.width, info.height);
        match &self.current {
            Some(frame) => {
                ui::draw_video(ui, rect, frame);
                // Frame tới nơi nhưng không nạp lên GPU được thì ô video trống
                // trơn — trông y hệt mất kết nối. Viết thẳng câu lỗi lên đó.
                // Hình đứng im lâu trông giống hệt hình đang chạy mà máy kia
                // không động đậy. Nói rõ ra, kèm số giây, để người dùng biết
                // mình đang nhìn ảnh cũ chứ không phải màn hình thật.
                let dung = self
                    .current_at
                    .map(|luc| luc.elapsed())
                    .filter(|cho| *cho > STALE_AFTER);
                if let Some(cho) = dung {
                    ui.scope_builder(egui::UiBuilder::new().max_rect(outer), |ui| {
                        ui.centered_and_justified(|ui| {
                            ui.label(
                                egui::RichText::new(format!(
                                    "Hình đã đứng {:.0} giây — đang chờ khung mới…",
                                    cho.as_secs_f32()
                                ))
                                .size(16.0)
                                .color(ui::WARN),
                            );
                        });
                    });
                }
                if let Some(err) = rd_viewer::video::import_error() {
                    ui.scope_builder(egui::UiBuilder::new().max_rect(outer), |ui| {
                        ui.centered_and_justified(|ui| {
                            ui.label(
                                egui::RichText::new(format!(
                                    "Nhận được hình nhưng card đồ hoạ không vẽ được:\n{err}"
                                ))
                                .size(16.0)
                                .color(ui::BAD),
                            );
                        });
                    });
                }
            }
            // Nối được nhưng chưa có hình thì phải nói ra. Để trống là màn hình
            // đen y hệt lúc hỏng, và người dùng không có cách nào phân biệt
            // "đang chờ" với "chuỗi chụp bên kia đã chết".
            None => {
                ui.scope_builder(egui::UiBuilder::new().max_rect(outer), |ui| {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            egui::RichText::new("Đã nối — đang chờ hình từ máy kia…")
                                .size(16.0)
                                .color(ui::WARN),
                        );
                    });
                });
            }
        }

        // Ô nhận input phải trùng đúng ô video: bấm vào viền đen mà vẫn tính
        // toạ độ là chuột nhảy về mép màn hình bên kia.
        let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());
        if self.control_on {
            if response.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::None);
            }
            let events = ui.input(|i| i.events.clone());
            let translated = self.capture.translate(&events, rect);
            if !translated.is_empty() {
                self.net.send(UiCommand::Input(translated));
            }
            // Mất focus mà còn phím đang giữ thì bên kia kẹt phím vĩnh viễn:
            // không sự kiện nhả nào tới nữa vì cửa sổ này hết nghe.
            if !ui.input(|i| i.focused) {
                self.net.send(UiCommand::Input(vec![self.capture.reset()]));
                self.control_on = false;
            }
            if self.prefs.local_cursor && response.hovered() {
                if let Some(pos) = ui.ctx().pointer_latest_pos() {
                    draw_cursor(ui, pos);
                }
            }
        }

        if self.show_hud {
            let mut lines = Vec::new();
            lines.push((
                format!(
                    "điều khiển: {} (F9)",
                    if self.control_on { "bật" } else { "tắt" }
                ),
                if self.control_on { ui::GOOD } else { ui::WARN },
            ));
            if let Some(stats) = &self.link {
                lines.push((
                    format!(
                        "rtt {:.1} ms   mất gói {:.2}%   mtu {}{}",
                        stats.rtt.as_secs_f32() * 1000.0,
                        stats.loss_ratio() * 100.0,
                        stats.current_mtu,
                        if self.relayed { "   qua relay" } else { "" }
                    ),
                    ui::WARN,
                ));
            }
            lines.push((
                "F10 bảng này   F8 chat   F11 toàn màn hình   F12 ẩn thanh".to_string(),
                ui::WARN,
            ));

            let data = ui::HudData {
                info: Some(&info),
                summary: self.metrics.summary(),
                lines,
                network_ms: self.latency_us.map(|us| us as f32 / 1000.0),
            };
            ui::hud_at(ui, outer, &data);
        }
    }

    fn draw_host_card(&mut self, ui: &mut egui::Ui) {
        let outer = ui.max_rect();
        ui::paint_backdrop(ui, outer);
        ui.vertical_centered(|ui| {
            ui.add_space(48.0);
            ui.heading("Máy này đang cho điều khiển từ xa");
            ui.add_space(16.0);

            egui::Frame::new()
                .fill(egui::Color32::from_black_alpha(160))
                .inner_margin(20.0)
                .corner_radius(8.0)
                .show(ui, |ui| {
                    ui.set_max_width(420.0);
                    match self.code {
                        Some(id) => {
                            ui.label("Mã máy — đọc cho người kia gõ vào:");
                            ui.label(
                                egui::RichText::new(id.to_string())
                                    .monospace()
                                    .size(34.0)
                                    .strong(),
                            );
                            if ui.button("Chép mã").clicked() {
                                ui.ctx().copy_text(id.get().to_string());
                            }
                        }
                        None => {
                            ui.label("Chưa có mã — chưa nối được rendezvous server.");
                            ui.label(
                                egui::RichText::new("Vẫn nối thẳng theo IP:cổng được.").small(),
                            );
                        }
                    }

                    // Địa chỉ ngay đây thì khỏi phải đi lục trong cài đặt mạng
                    // của hệ điều hành mới biết đọc gì cho người kia gõ.
                    for address in &self.addresses {
                        ui.add_space(12.0);
                        ui.separator();
                        ui.add_space(12.0);
                        ui.label(format!("Địa chỉ {}:", address.label));
                        ui.label(
                            egui::RichText::new(&address.value)
                                .monospace()
                                .size(22.0)
                                .strong(),
                        );
                        if ui.button("Chép địa chỉ").clicked() {
                            ui.ctx().copy_text(address.value.clone());
                        }
                    }

                    ui.add_space(12.0);
                    ui.separator();
                    ui.add_space(12.0);
                    ui.label("Mật khẩu phiên:");
                    ui.label(
                        egui::RichText::new(&self.password)
                            .monospace()
                            .size(26.0)
                            .strong(),
                    );
                    if ui.button("Chép mật khẩu").clicked() {
                        ui.ctx().copy_text(self.password.clone());
                    }
                });

            ui.add_space(20.0);
            ui.label(egui::RichText::new(&self.status).monospace().small());
            if let Some(stats) = &self.link {
                ui.label(
                    egui::RichText::new(format!(
                        "rtt {:.1} ms   đã gửi {}   đã nhận {}",
                        stats.rtt.as_secs_f32() * 1000.0,
                        ui::bytes(stats.bytes_sent),
                        ui::bytes(stats.bytes_received),
                    ))
                    .monospace()
                    .small(),
                );
            }
            if let Some(info) = &self.info {
                ui.label(
                    egui::RichText::new(format!(
                        "đang mã hoá {}x{} {:?} {:?} @{} fps",
                        info.width, info.height, info.codec, info.chroma, info.target_fps
                    ))
                    .monospace()
                    .small(),
                );
            }
        });
    }

    fn draw_panel(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        let mut sync_clipboard = self.clipboard.is_some();
        if ui
            .checkbox(&mut sync_clipboard, "Đồng bộ clipboard")
            .on_hover_text("Copy văn bản ở máy này thì dán được ở máy kia, và ngược lại")
            .changed()
        {
            self.clipboard = sync_clipboard.then(|| Clipboard::start(&self.clip_sink));
            if let Some(clipboard) = &self.clipboard {
                clipboard.forward_to(Some(forwarder(&self.net, &self.clip_sink)));
            }
        }

        ui.add_space(8.0);
        ui.separator();
        ui.heading("Tệp");
        self.draw_transfers(ui);

        ui.add_space(8.0);
        ui.separator();
        ui.heading("Chat");
        self.draw_chat(ui);
    }

    fn draw_transfers(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let button = egui::Button::new("Chọn tệp để gửi…");
            if ui.add_enabled(!self.picker.busy(), button).clicked() {
                self.picker.open();
            }
            ui.label(
                egui::RichText::new(if self.picker.busy() {
                    "đang mở hộp thoại…"
                } else {
                    "hoặc kéo thả vào cửa sổ"
                })
                .small(),
            );
        });

        let mut accept = None;
        let mut reject = None;
        let mut cancel = None;
        let mut reveal = None;
        let mut open = None;
        egui::ScrollArea::vertical()
            .id_salt("tệp")
            .max_height(200.0)
            .show(ui, |ui| {
                for item in self.transfers.list() {
                    ui.add_space(6.0);
                    // Chỉ dùng chữ: mũi tên Unicode không có trong font hệ
                    // thống của macOS lẫn Windows nên hiện ra ô vuông.
                    let arrow = match item.direction {
                        Direction::Out => "gửi",
                        Direction::In => "nhận",
                    };
                    ui.label(
                        egui::RichText::new(format!(
                            "{arrow} {}  ({})",
                            item.name,
                            ui::bytes(item.size)
                        ))
                        .small(),
                    );
                    match &item.state {
                        TransferState::Offered if item.direction == Direction::In => {
                            ui.horizontal(|ui| {
                                if ui.button("Nhận").clicked() {
                                    accept = Some(item.id);
                                }
                                if ui.button("Từ chối").clicked() {
                                    reject = Some(item.id);
                                }
                            });
                        }
                        TransferState::Offered => {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new("chờ đầu kia đồng ý").small());
                                if ui.small_button("Huỷ").clicked() {
                                    cancel = Some(item.id);
                                }
                            });
                        }
                        TransferState::Active => {
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::ProgressBar::new(item.ratio())
                                        .text(format!("{}/s", ui::bytes(item.rate() as u64)))
                                        .desired_width(ui.available_width() - 52.0),
                                );
                                if ui.small_button("Huỷ").clicked() {
                                    cancel = Some(item.id);
                                }
                            });
                        }
                        TransferState::Done(path) => match path {
                            Some(path) => {
                                ui.horizontal(|ui| {
                                    ui.colored_label(ui::GOOD, egui::RichText::new("xong").small());
                                    // Đường dẫn đầy đủ không nói lên gì mấy khi
                                    // nó dài hơn cả bảng; nút mở thẳng tới nơi
                                    // thì có ích hơn.
                                    if ui.small_button("Mở").clicked() {
                                        open = Some(path.clone());
                                    }
                                    if ui.small_button("Thư mục").clicked() {
                                        reveal = Some(path.clone());
                                    }
                                })
                                .response
                                .on_hover_text(path.display().to_string());
                            }
                            None => {
                                ui.colored_label(ui::GOOD, egui::RichText::new("xong").small());
                            }
                        },
                        TransferState::Rejected => {
                            ui.colored_label(ui::WARN, egui::RichText::new("bị từ chối").small());
                        }
                        TransferState::Cancelled { by_peer } => {
                            let text = if *by_peer {
                                "đầu kia đã huỷ"
                            } else {
                                "đã huỷ"
                            };
                            ui.colored_label(ui::WARN, egui::RichText::new(text).small());
                        }
                        TransferState::Failed(reason) => {
                            ui.colored_label(ui::BAD, egui::RichText::new(reason).small());
                        }
                    }
                }
            });

        if let Some(id) = accept {
            self.transfers.accept_local(id);
            self.net.send(UiCommand::AcceptFile(id));
        }
        if let Some(id) = reject {
            self.transfers.reject(id);
            self.net.send(UiCommand::RejectFile(id));
        }
        if let Some(id) = cancel {
            self.transfers.cancel(id, false);
            self.net.send(UiCommand::CancelFile(id));
        }
        if let Some(path) = reveal {
            crate::os::reveal(&path);
        }
        if let Some(path) = open {
            crate::os::open_file(&path);
        }
    }

    fn draw_chat(&mut self, ui: &mut egui::Ui) {
        let height = (ui.available_height() - 40.0).max(80.0);
        egui::ScrollArea::vertical()
            .id_salt("chat")
            .max_height(height)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for entry in self.chat.entries() {
                    let (color, prefix) = match entry.who {
                        Who::Me => (ui::GOOD, "bạn"),
                        Who::Peer => (ui::WARN, entry.name.as_str()),
                        Who::System => (egui::Color32::GRAY, "•"),
                    };
                    ui.horizontal_wrapped(|ui| {
                        ui.colored_label(color, egui::RichText::new(prefix).small().strong());
                        ui.label(egui::RichText::new(&entry.body).small());
                        ui.label(
                            egui::RichText::new(ago(entry.at_ms))
                                .small()
                                .color(egui::Color32::DARK_GRAY),
                        );
                    });
                }
            });

        ui.horizontal(|ui| {
            let field = ui.add(
                egui::TextEdit::singleline(&mut self.draft)
                    .hint_text("nhắn gì đó")
                    .desired_width(f32::INFINITY),
            );
            let submitted = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if submitted && !self.draft.trim().is_empty() {
                let body = self.draft.trim().to_string();
                self.draft.clear();
                self.chat.mine(&self.my_name, body.clone());
                self.net.send(UiCommand::Chat(ChatMessage {
                    from: self.my_name.clone(),
                    body,
                    sent_at_ms: now_ms(),
                }));
                field.request_focus();
            }
        });
    }
}

/// "3 phút trước". Đếm ngược từ hiện tại chứ không hiện giờ đồng hồ: dấu thời
/// gian đi trên dây là giờ của máy bên kia, mà hai máy có thể lệch múi giờ —
/// hiện thẳng ra là người đọc tưởng tin nhắn tới từ tương lai.
fn ago(at_ms: u64) -> String {
    let secs = now_ms().saturating_sub(at_ms) / 1000;
    match secs {
        0..=9 => "vừa xong".into(),
        10..=59 => format!("{secs}s"),
        60..=3599 => format!("{} phút", secs / 60),
        _ => format!("{} giờ", secs / 3600),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Đường đưa văn bản vừa copy từ luồng clipboard thẳng sang phần mạng.
///
/// Chỉ gửi khi đang có người ở đầu kia: dồn lại rồi bắn một loạt lúc vừa nối là
/// ghi đè clipboard của họ bằng những thứ ta copy từ mấy phút trước.
fn forwarder(net: &NetHandle, sink: &ClipboardSink) -> crate::os::ClipboardOut {
    let commands = net.commands();
    let sink = sink.clone();
    std::sync::Arc::new(move |text: String| {
        if sink.has_peer() {
            let _ = commands.send(UiCommand::Clipboard(text));
        }
    })
}

/// Chờ bao lâu trước lần thử nối lại thứ `attempts + 1`.
fn backoff(attempts: u32) -> Duration {
    (RECONNECT_FIRST * 2u32.saturating_pow(attempts.min(3))).min(RECONNECT_MAX)
}

/// Vẽ mũi tên con trỏ ngay tại chỗ chuột thật đang đứng.
///
/// Con trỏ nhìn thấy trong hình là con trỏ máy kia đã vẽ xong, mã hoá, truyền
/// qua mạng rồi giải mã — nó luôn tới muộn đúng bằng cả vòng đó, nên chuột cứ
/// như bị kéo lê dù chẳng khung hình nào rơi. Vẽ thêm một mũi tên ăn theo
/// chuột thật thì tay và mắt khớp lại ngay, mạng nhanh chậm không đổi. Đây là
/// cách Parsec và Moonlight vẫn làm.
fn draw_cursor(ui: &egui::Ui, pos: egui::Pos2) {
    let painter = ui.painter();
    // Hình mũi tên chuẩn: đỉnh ở đúng điểm chuột, đuôi chẻ đôi.
    let points = [
        egui::pos2(0.0, 0.0),
        egui::pos2(0.0, 16.0),
        egui::pos2(4.2, 12.2),
        egui::pos2(6.8, 17.6),
        egui::pos2(9.4, 16.4),
        egui::pos2(6.9, 11.2),
        egui::pos2(11.6, 11.0),
    ]
    .map(|p| pos + p.to_vec2());
    // Viền đen quanh thân trắng để mũi tên còn thấy được trên nền sáng.
    painter.add(egui::Shape::convex_polygon(
        points.to_vec(),
        egui::Color32::WHITE,
        egui::Stroke::new(1.2, egui::Color32::from_black_alpha(220)),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cho_giua_hai_lan_thu_gian_dan_roi_cham_tran() {
        assert_eq!(backoff(0), Duration::from_secs(2));
        assert_eq!(backoff(1), Duration::from_secs(4));
        assert_eq!(backoff(2), Duration::from_secs(8));
        // Từ đây trở đi đứng ở trần, kể cả khi máy kia tắt cả buổi.
        assert_eq!(backoff(3), RECONNECT_MAX);
        assert_eq!(backoff(1000), RECONNECT_MAX);
    }

    fn moi(id: u64) -> FileOffer {
        FileOffer {
            transfer_id: id,
            name: "a.bin".into(),
            size: 1024,
            hash: [0; 32],
        }
    }

    #[test]
    fn huy_de_len_loi_nhung_loi_khong_de_len_huy() {
        // Đầu kia huỷ: luồng ở máy này đứt nên bộ truyền báo lỗi ngay sau đó.
        // Người dùng phải thấy "đầu kia đã huỷ", không phải một dòng lỗi lạ.
        let mut a = Transfers::default();
        a.offer_incoming(moi(1));
        a.fail(1, "stream reset".into());
        a.cancel(1, true);
        assert_eq!(
            a.list()[0].state,
            TransferState::Cancelled { by_peer: true }
        );

        // Thứ tự ngược lại — tin huỷ tới trước, lỗi tới sau — vẫn thế.
        let mut b = Transfers::default();
        b.offer_incoming(moi(1));
        b.cancel(1, true);
        b.fail(1, "stream reset".into());
        assert_eq!(
            b.list()[0].state,
            TransferState::Cancelled { by_peer: true }
        );
    }

    #[test]
    fn huy_toi_muon_khong_xoa_mat_ket_qua_da_xong() {
        // Bấm Huỷ đúng lúc byte cuối vừa tới: tệp đã nằm trên đĩa rồi, đánh
        // dấu "đã huỷ" lúc này là nói dối.
        let mut t = Transfers::default();
        t.offer_incoming(moi(1));
        t.finish(1, Some(PathBuf::from("/tmp/a.bin")));
        t.cancel(1, false);
        assert!(matches!(t.list()[0].state, TransferState::Done(_)));
    }
}
