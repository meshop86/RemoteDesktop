//! Một phiên đang chạy: giữ trạng thái và vẽ nó ra.
//!
//! Cùng một kiểu dùng cho cả hai vai. Khác nhau đúng hai chỗ: host không có
//! video để vẽ (nó là bên *bị* xem), và chỉ viewer mới bắt chuột phím gửi đi.
//! Phần chat với truyền file thì đối xứng hoàn toàn — gộp làm một chỗ để sửa
//! một lần là cả hai bên cùng đúng.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use eframe::egui;

#[cfg(target_os = "macos")]
use rd_codec::videotoolbox::DecodedFrame;
#[cfg(target_os = "windows")]
use rd_codec::mediafoundation::DecodedFrame;

use rd_protocol::control::{ChatMessage, FileOffer};
use rd_signal::PeerId;
use rd_transport::LinkStats;
use rd_viewer::input_capture::InputCapture;
use rd_viewer::metrics::Metrics;
use rd_viewer::pipeline::PipelineInfo;

use crate::net::{NetConfig, NetEvent, NetHandle, Role, UiCommand};
use crate::ui;

/// Bật/tắt HUD. Trùng với phím tắt của chế độ thử tại chỗ để khỏi phải nhớ hai
/// phím cho cùng một việc.
const HUD_KEY: egui::Key = egui::Key::F10;
/// Bật/tắt gửi chuột phím sang máy kia.
const CONTROL_KEY: egui::Key = egui::Key::F9;
/// Bật/tắt bảng chat và file.
const PANEL_KEY: egui::Key = egui::Key::F8;

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

    pub fn fail(&mut self, id: u64, reason: String) {
        if let Some(item) = self.find(id) {
            item.state = TransferState::Failed(reason);
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
    metrics: Metrics,

    chat: ChatLog,
    transfers: Transfers,

    status: String,
    code: Option<PeerId>,
    peer: Option<String>,
    relayed: bool,
    link: Option<LinkStats>,
    latency_us: Option<u64>,
    /// Lý do phiên chết hẳn. Còn `None` là vẫn đang sống.
    ended: Option<String>,

    capture: InputCapture,
    control_on: bool,
    draft: String,
    file_draft: String,
    show_hud: bool,
    show_panel: bool,
    unread: u32,
    last_report: Instant,
}

impl SessionState {
    pub fn start(config: NetConfig) -> anyhow::Result<Self> {
        let role = config.role;
        let my_name = config.name.clone();
        let password = config.password.clone();
        let bitrate_kbps = config.bitrate_kbps;
        let net = NetHandle::spawn(config)?;
        Ok(Self {
            role,
            net,
            my_name,
            password,
            bitrate_kbps,
            info: None,
            current: None,
            metrics: Metrics::default(),
            chat: ChatLog::default(),
            transfers: Transfers::default(),
            status: "đang khởi động".into(),
            code: None,
            peer: None,
            relayed: false,
            link: None,
            latency_us: None,
            ended: None,
            capture: InputCapture::new(),
            // Mặc định tắt: vừa nối xong mà chuột đã điều khiển máy người khác
            // thì cú click để phóng to cửa sổ cũng rơi sang bên kia.
            control_on: false,
            draft: String::new(),
            file_draft: String::new(),
            show_hud: true,
            show_panel: true,
            unread: 0,
            last_report: Instant::now(),
        })
    }

    pub fn update(&mut self, root: &mut egui::Ui) -> Outcome {
        let ctx = root.ctx().clone();
        self.pump();
        self.hotkeys(&ctx);
        self.take_dropped_files(&ctx);

        let outcome = self.draw(root);

        self.metrics.repaints += 1;
        if self.last_report.elapsed() >= std::time::Duration::from_secs(2) {
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

        // Không chờ sự kiện chuột: frame tới bất cứ lúc nào, và HUD phải nhúc
        // nhích ngay cả khi hình đứng yên.
        ctx.request_repaint();
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
                    self.chat.system(format!(
                        "{name} ({peer}) đã nối{}",
                        if relayed { " qua relay" } else { "" }
                    ));
                }
                NetEvent::Info(info) => self.info = Some(*info),
                NetEvent::OutgoingOffer(offer) => {
                    self.chat.system(format!("đang gửi {}", offer.name));
                    self.transfers.offer_outgoing(offer);
                }
                NetEvent::IncomingOffer(offer) => {
                    self.chat
                        .system(format!("{} muốn gửi {}", self.peer_name(), offer.name));
                    self.transfers.offer_incoming(offer);
                    self.unread += 1;
                }
                NetEvent::TransferAccepted(id) => self.transfers.peer_accepted(id),
                NetEvent::TransferRejected(id) => self.transfers.reject(id),
                NetEvent::TransferProgress { id, done } => self.transfers.progress(id, done),
                NetEvent::TransferDone { id, path } => {
                    self.chat
                        .system(format!("xong: {}", path.display()));
                    self.transfers.finish(id, Some(path));
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
                    self.latency_us = None;
                    self.chat.system("đầu kia đã ngắt");
                }
                NetEvent::Disconnected(reason) => {
                    self.chat.system(format!("phiên kết thúc: {reason}"));
                    self.ended = Some(reason);
                }
            }
        }

        if let Some(frame) = self.net.latest_frame() {
            self.metrics.push(
                frame.pipeline_us,
                frame.encode_us,
                frame.decode_us,
                frame.bytes,
                frame.keyframe,
            );
            self.current = Some(frame.frame);
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
        });
        if self.show_panel {
            self.unread = 0;
        }
    }

    /// Kéo tệp thả vào cửa sổ là cách gửi nhanh nhất, và không phải kéo thêm
    /// một thư viện hộp thoại chọn tệp về chỉ để làm việc này.
    fn take_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        for file in dropped {
            self.net.send(UiCommand::SendFile(file.path().to_path_buf()));
        }
    }

    fn peer_name(&self) -> &str {
        self.peer.as_deref().unwrap_or("đầu kia")
    }

    fn connected(&self) -> bool {
        self.peer.is_some()
    }

    fn draw(&mut self, root: &mut egui::Ui) -> Outcome {
        let mut outcome = Outcome::Continue;

        egui::Panel::top("thanh").show(root, |ui| {
            ui.horizontal(|ui| {
                if ui.button("← Thoát phiên").clicked() {
                    outcome = Outcome::Leave;
                }
                ui.separator();
                ui.label(match self.role {
                    Role::Host => "Đang chia sẻ máy này",
                    Role::Viewer => "Đang xem máy khác",
                });
                ui.separator();
                let (dot, color) = if self.ended.is_some() {
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

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
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
                        ui.label("kbps");
                        // Chỉnh được giữa phiên vì mạng thay đổi trong lúc
                        // dùng: chuyển từ wifi sang 4G là phải hạ ngay, không
                        // thì hình đứng cả chục giây trước khi bộ điều khiển
                        // tắc nghẽn tự nhận ra.
                        if ui
                            .add(
                                egui::DragValue::new(&mut self.bitrate_kbps)
                                    .range(1_000..=200_000)
                                    .speed(500),
                            )
                            .changed()
                        {
                            self.net.send(UiCommand::SetBitrate(self.bitrate_kbps));
                        }
                    }
                });
            });
        });

        if self.show_panel {
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

        outcome
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
        if let Some(frame) = &self.current {
            ui::draw_video(ui, rect, frame);
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
                self.net
                    .send(UiCommand::Input(vec![self.capture.reset()]));
                self.control_on = false;
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
            lines.push(("F10 ẩn/hiện bảng này   F8 chat".to_string(), ui::WARN));

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
        ui.heading("Tệp");
        self.draw_transfers(ui);

        ui.add_space(8.0);
        ui.separator();
        ui.heading("Chat");
        self.draw_chat(ui);
    }

    fn draw_transfers(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let field = ui.add(
                egui::TextEdit::singleline(&mut self.file_draft)
                    .hint_text("đường dẫn tệp, hoặc kéo thả vào cửa sổ")
                    .desired_width(f32::INFINITY),
            );
            let submitted =
                field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if submitted && !self.file_draft.trim().is_empty() {
                let path = PathBuf::from(self.file_draft.trim());
                self.file_draft.clear();
                self.net.send(UiCommand::SendFile(path));
            }
        });
        if ui.button("Gửi tệp").clicked() && !self.file_draft.trim().is_empty() {
            let path = PathBuf::from(self.file_draft.trim());
            self.file_draft.clear();
            self.net.send(UiCommand::SendFile(path));
        }

        let mut accept = None;
        let mut reject = None;
        egui::ScrollArea::vertical()
            .id_salt("tệp")
            .max_height(200.0)
            .show(ui, |ui| {
                for item in self.transfers.list() {
                    ui.add_space(6.0);
                    let arrow = match item.direction {
                        Direction::Out => "↑",
                        Direction::In => "↓",
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
                            ui.label(egui::RichText::new("chờ đầu kia đồng ý").small());
                        }
                        TransferState::Active => {
                            ui.add(egui::ProgressBar::new(item.ratio()).text(format!(
                                "{}/s",
                                ui::bytes(item.rate() as u64)
                            )));
                        }
                        TransferState::Done(path) => {
                            let text = match path {
                                Some(path) => format!("xong → {}", path.display()),
                                None => "xong".to_string(),
                            };
                            ui.colored_label(ui::GOOD, egui::RichText::new(text).small());
                        }
                        TransferState::Rejected => {
                            ui.colored_label(ui::WARN, egui::RichText::new("bị từ chối").small());
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
            let submitted =
                field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
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
