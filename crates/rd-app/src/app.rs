//! Màn hình đầu và bộ máy trạng thái của cả chương trình.

use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use eframe::egui;

#[cfg(target_os = "macos")]
use rd_codec::videotoolbox::DecodedFrame;
#[cfg(target_os = "windows")]
use rd_codec::mediafoundation::DecodedFrame;

use rd_signal::PeerId;
use rd_viewer::control::RemoteControl;
use rd_viewer::input_capture::InputCapture;
use rd_viewer::metrics::Metrics;
use rd_viewer::pipeline::Pipeline;
use rd_viewer::video;

use crate::net::{NetConfig, PeerAddress, Role};
use crate::session::{Outcome, SessionState};
use crate::ui;

/// Nạp một font hệ thống có đủ chữ Việt có dấu.
///
/// Font mặc định của egui dừng ở Latin Extended-A, mà phần lớn dấu tiếng Việt
/// (ề, ử, ọ, ậ...) nằm ở Latin Extended Additional — thiếu thì hiện ra ô vuông.
/// Mượn font sẵn có của hệ điều hành thay vì nhúng thêm một file font: không
/// làm chương trình nặng thêm, và chữ trông giống hệt các ứng dụng khác trên
/// cùng máy. Danh sách xếp theo thứ tự ưu tiên, lấy file đầu tiên đọc được.
fn install_vietnamese_font(ctx: &egui::Context) {
    #[cfg(target_os = "macos")]
    const CANDIDATES: &[&str] = &[
        "/System/Library/Fonts/Helvetica.ttc",
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
    ];
    #[cfg(target_os = "windows")]
    const CANDIDATES: &[&str] = &[
        r"C:\Windows\Fonts\segoeui.ttf",
        r"C:\Windows\Fonts\tahoma.ttf",
        r"C:\Windows\Fonts\arial.ttf",
    ];
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    const CANDIDATES: &[&str] = &[];

    const NAME: &str = "hệ thống";

    let Some((path, bytes)) = CANDIDATES
        .iter()
        .find_map(|path| Some((*path, std::fs::read(path).ok()?)))
    else {
        tracing::warn!("không đọc được font hệ thống nào, chữ có dấu sẽ hiện ra ô vuông");
        return;
    };

    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert(NAME.to_owned(), Arc::new(egui::FontData::from_owned(bytes)));
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, NAME.to_owned());
    // Ở chỗ chữ đều nhau thì để nó đứng cuối: chỉ dùng khi font kia không có
    // ký tự, nhờ vậy các con số vẫn thẳng cột như cũ.
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .push(NAME.to_owned());
    ctx.set_fonts(fonts);
    tracing::info!(path, "dùng font hệ thống cho chữ có dấu");
}

/// Cổng mặc định của host.
///
/// Cố định chứ không xin cổng ngẫu nhiên: người dùng nối thẳng theo IP phải gõ
/// được cổng mà không cần hỏi bên kia, và mở cổng trên router cũng cần một số
/// không đổi. Nằm trong dải cổng động nên hiếm khi đụng dịch vụ khác.
const DEFAULT_PORT: u16 = 47823;

pub const USAGE: &str = "\
Điều khiển máy tính từ xa

    remote-desktop [TUỲ CHỌN]

Không tham số nào là bắt buộc; chúng chỉ điền sẵn màn hình đầu.

    --host                 vào thẳng chế độ chia sẻ máy này
    --connect ĐỊA_CHỈ|MÃ   vào thẳng chế độ điều khiển máy khác
    --password MK          mật khẩu phiên (mặc định: sinh ngẫu nhiên)
    --rendezvous ĐỊA_CHỈ   server hẹn gặp, để nối qua internet
    --name TÊN             tên hiện cho máy kia
    --fps N                số hình mỗi giây (mặc định 60)
    --bitrate KBPS         băng thông video (mặc định 30000)
    --port CỔNG            cổng chờ khi chia sẻ (mặc định 47823)
    --downloads THƯ_MỤC    nơi lưu tệp nhận được
    --probe                in khả năng của máy này rồi thoát
    -h, --help             in bảng này
";

/// Tham số dòng lệnh. Tất cả đều tuỳ chọn — chúng chỉ điền sẵn màn hình đầu.
#[derive(Debug, Clone)]
pub struct Args {
    /// Người dùng chỉ muốn xem cách dùng; đừng mở cửa sổ.
    pub help: bool,
    /// In khả năng của máy rồi thoát, không mở cửa sổ.
    pub probe: bool,
    pub host: bool,
    pub connect: Option<String>,
    pub password: Option<String>,
    pub rendezvous: Option<String>,
    pub name: Option<String>,
    pub fps: u32,
    pub bitrate: u32,
    pub downloads: Option<PathBuf>,
    pub port: u16,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            help: false,
            probe: false,
            host: false,
            connect: None,
            password: None,
            rendezvous: None,
            name: None,
            // 60 fps là nhịp quét của gần hết màn hình đang dùng. Đặt cao hơn
            // chỉ tốn băng thông cho những frame màn hình không hiện kịp.
            fps: 60,
            bitrate: 30_000,
            downloads: None,
            port: DEFAULT_PORT,
        }
    }
}

impl Args {
    pub fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut out = Args::default();
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            // Mỗi cờ có giá trị đều phải lấy giá trị ngay tại đây; thiếu thì
            // báo tên cờ chứ không im lặng bỏ qua.
            let mut value = || {
                args.next()
                    .ok_or_else(|| format!("{arg} cần một giá trị đi kèm"))
            };
            match arg.as_str() {
                "-h" | "--help" => out.help = true,
                "--probe" => out.probe = true,
                "--host" => out.host = true,
                "--connect" => out.connect = Some(value()?),
                "--password" => out.password = Some(value()?),
                "--rendezvous" => out.rendezvous = Some(value()?),
                "--name" => out.name = Some(value()?),
                "--fps" => {
                    out.fps = value()?
                        .parse()
                        .map_err(|_| "--fps phải là số".to_string())?
                }
                "--bitrate" => {
                    out.bitrate = value()?
                        .parse()
                        .map_err(|_| "--bitrate phải là số kbps".to_string())?
                }
                "--port" => {
                    out.port = value()?
                        .parse()
                        .map_err(|_| "--port phải là số cổng".to_string())?
                }
                "--downloads" => out.downloads = Some(PathBuf::from(value()?)),
                other => return Err(format!("không hiểu tham số {other}")),
            }
        }
        Ok(out)
    }
}

/// Bảng khai máy này làm được gì, in ra bởi `--probe`.
///
/// Có mục "chụp thử một frame" chứ không chỉ liệt kê màn hình, vì đúng cái lỗi
/// khó thấy nhất nằm ở khoảng giữa: hệ điều hành khai có màn hình, nhưng mở
/// luồng chụp lại hỏng, và lúc đó chương trình lặng lẽ phát hình tổng hợp thay
/// vì báo lỗi. Người ngồi ở máy chia sẻ không thấy gì bất thường cả.
pub fn probe() -> String {
    use rd_capture::ScreenCapturer as _;
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "remote-desktop {} trên {} {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    );

    let _ = writeln!(out, "\nMàn hình:");
    match rd_capture::PlatformCapturer::list_displays() {
        Ok(displays) if displays.is_empty() => {
            let _ = writeln!(out, "  (hệ điều hành không khai màn hình nào)");
        }
        Ok(displays) => {
            for display in displays {
                let _ = writeln!(
                    out,
                    "  [{}] {} — {}x{} @ {}Hz{}",
                    display.id,
                    display.name,
                    display.width,
                    display.height,
                    display.refresh_hz,
                    if display.is_primary { " (chính)" } else { "" }
                );
            }
        }
        Err(err) => {
            let _ = writeln!(out, "  không liệt kê được: {err}");
        }
    }

    let _ = writeln!(out, "\nChụp thử màn hình chính:");
    match rd_capture::PlatformCapturer::start(rd_capture::CaptureConfig::default()) {
        Ok(mut capturer) => {
            match capturer.next_frame(std::time::Duration::from_secs(5)) {
                Ok(frame) => {
                    let _ = writeln!(out, "  được — frame {}x{}", frame.width, frame.height);
                }
                Err(err) => {
                    let _ = writeln!(out, "  mở được luồng nhưng không có frame nào: {err}");
                }
            }
            capturer.stop();
        }
        Err(err) => {
            let _ = writeln!(out, "  KHÔNG được: {err}");
            let _ = writeln!(
                out,
                "  → khi chia sẻ, người kia sẽ thấy hình tổng hợp chứ không phải màn hình này"
            );
        }
    }

    let _ = writeln!(out, "\nVideo:");
    let _ = writeln!(out, "  mã hoá được:  {:?}", rd_codec::encodable());
    let decodable = rd_codec::decodable();
    let _ = writeln!(out, "  giải mã được: {decodable:?}");
    if !decodable.contains(&rd_codec::Codec::Hevc) {
        let _ = writeln!(
            out,
            "  → thiếu bộ giải mã HEVC; phiên sẽ tự lùi về H.264 (tốn băng thông hơn ~30%).\n    \
             Cài \"HEVC Video Extensions\" trong Microsoft Store để dùng HEVC."
        );
    }

    let _ = writeln!(out, "\nMạng:");
    match crate::session::lan_ip() {
        Some(ip) => {
            let _ = writeln!(out, "  địa chỉ trong mạng nhà: {ip}:{DEFAULT_PORT}");
        }
        None => {
            let _ = writeln!(out, "  không xác định được địa chỉ trong mạng nhà");
        }
    }

    let _ = writeln!(out, "\nTailscale:");
    for line in crate::tailscale::describe().lines() {
        let _ = writeln!(out, "  {line}");
    }

    out
}

/// Nội dung người dùng đang gõ ở màn hình đầu.
struct Form {
    target: String,
    password: String,
    rendezvous: String,
    name: String,
    fps: u32,
    bitrate: u32,
    downloads: String,
    port: u16,
}

impl Form {
    fn from_args(args: &Args) -> Self {
        Self {
            target: args.connect.clone().unwrap_or_default(),
            password: args.password.clone().unwrap_or_else(random_password),
            rendezvous: args.rendezvous.clone().unwrap_or_default(),
            name: args.name.clone().unwrap_or_else(machine_name),
            fps: args.fps,
            bitrate: args.bitrate,
            downloads: args
                .downloads
                .clone()
                .unwrap_or_else(default_downloads)
                .display()
                .to_string(),
            port: args.port,
        }
    }

    fn config(&self, role: Role, allow_10bit: bool) -> Result<NetConfig, String> {
        let rendezvous = match self.rendezvous.trim() {
            "" => None,
            text => Some(resolve(text)?),
        };
        let peer = match role {
            Role::Host => None,
            Role::Viewer => Some(parse_peer(self.target.trim())?),
        };
        // Host phải nghe ở cổng đã hẹn; viewer thì cổng nào cũng được, xin cổng
        // cố định chỉ tổ đụng nhau khi chạy hai bản trên cùng máy để thử.
        let bind: SocketAddr = match role {
            Role::Host => ([0, 0, 0, 0], self.port).into(),
            Role::Viewer => ([0, 0, 0, 0], 0).into(),
        };
        Ok(NetConfig {
            role,
            bind,
            rendezvous,
            rendezvous_fingerprint: None,
            peer_fingerprint: None,
            peer,
            name: self.name.trim().to_string(),
            password: self.password.trim().to_string(),
            target_fps: self.fps.clamp(1, 240),
            bitrate_kbps: self.bitrate.clamp(500, 500_000),
            allow_10bit,
            download_dir: PathBuf::from(self.downloads.trim()),
        })
    }
}

enum Screen {
    Start,
    Session(Box<SessionState>),
    Local(Box<LocalState>),
}

pub struct RdApp {
    screen: Screen,
    form: Form,
    /// Card đồ hoạ dựng được texture 16-bit hay không — quyết định có xin video
    /// 10-bit của máy kia không.
    can_10bit: bool,
    /// Trạng thái Tailscale, hỏi trên luồng nền (xem `crate::tailscale`).
    tailscale: crate::tailscale::Watcher,
    error: Option<String>,
}

impl RdApp {
    pub fn new(cc: &eframe::CreationContext<'_>, args: Args) -> anyhow::Result<Self> {
        let can_10bit = cc
            .wgpu_render_state
            .as_ref()
            .map(|state| video::supports_10bit(&state.device))
            .unwrap_or(false);
        tracing::info!(can_10bit, "khởi động giao diện");
        install_vietnamese_font(&cc.egui_ctx);

        let form = Form::from_args(&args);
        let mut app = Self {
            screen: Screen::Start,
            form,
            can_10bit,
            tailscale: crate::tailscale::Watcher::new(),
            error: None,
        };

        // Có cờ dòng lệnh thì vào thẳng, khỏi phải bấm — chạy đi chạy lại lúc
        // thử nghiệm mà mỗi lần còn phải bấm hai nút là mất thì giờ.
        if args.host {
            app.start(Role::Host);
        } else if args.connect.is_some() {
            app.start(Role::Viewer);
        }
        Ok(app)
    }

    fn start(&mut self, role: Role) {
        let config = match self.form.config(role, self.can_10bit) {
            Ok(config) => config,
            Err(err) => {
                self.error = Some(err);
                return;
            }
        };
        if let Err(err) = std::fs::create_dir_all(&config.download_dir) {
            self.error = Some(format!("không tạo được thư mục nhận tệp: {err}"));
            return;
        }
        // Địa chỉ tailnet chỉ có nghĩa khi Tailscale đang chạy; đưa sẵn cho
        // phiên để thẻ host đọc luôn được cả hai đường.
        let snapshot = self.tailscale.snapshot();
        let tailscale_ip = snapshot.running().then_some(snapshot.self_ip).flatten();
        match SessionState::start(config, tailscale_ip) {
            Ok(state) => {
                self.error = None;
                self.screen = Screen::Session(Box::new(state));
            }
            Err(err) => self.error = Some(err.to_string()),
        }
    }

    fn start_local(&mut self) {
        match LocalState::start(self.form.fps, self.form.bitrate, self.can_10bit) {
            Ok(state) => {
                self.error = None;
                self.screen = Screen::Local(Box::new(state));
            }
            Err(err) => self.error = Some(err.to_string()),
        }
    }

    /// Khung Tailscale: cho biết máy này đang ở đâu trong tailnet và cho bấm
    /// thẳng vào một máy khác thay vì gõ địa chỉ.
    ///
    /// Có Tailscale rồi thì không cần rendezvous server nữa: địa chỉ `100.x.y.z`
    /// đứng yên ở mọi mạng, hai máy tự tìm nhau kể cả sau NAT.
    fn draw_tailscale(&mut self, ui: &mut egui::Ui) {
        use crate::tailscale::State;

        // Người dùng đăng nhập xong ở trình duyệt, hay máy kia vừa bật: hỏi lại
        // đều đặn thì khung này tự đúng, khỏi phải bấm Làm mới.
        self.tailscale.tick(std::time::Duration::from_secs(5));
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs(1));

        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.heading("Tailscale");
                ui.add_space(6.0);
                if self.tailscale.busy() {
                    ui.spinner();
                } else if ui.small_button("Làm mới").clicked() {
                    self.tailscale.refresh();
                }
            });

            let status = self.tailscale.snapshot();
            let Some(state) = status.state.clone() else {
                ui.label(egui::RichText::new("đang kiểm tra…").small());
                return;
            };

            match state {
                State::NotInstalled => {
                    ui.label(
                        egui::RichText::new(
                            "Chưa cài. Cài Tailscale trên cả hai máy rồi đăng nhập cùng một \
                             tài khoản là nối được qua internet mà không cần rendezvous server \
                             và không phải mở cổng trên router.",
                        )
                        .small(),
                    );
                    ui.hyperlink_to("Tải Tailscale", "https://tailscale.com/download");
                }
                State::Stopped => {
                    ui.colored_label(ui::WARN, "Đã cài nhưng chưa bật.");
                    if ui.button("Bật kết nối").clicked() {
                        self.tailscale.up();
                    }
                }
                State::NeedsLogin => {
                    ui.colored_label(ui::WARN, "Chưa đăng nhập.");
                    if ui.button("Đăng nhập Tailscale").clicked() {
                        self.tailscale.login();
                    }
                }
                State::Broken(reason) => {
                    ui.colored_label(ui::BAD, reason);
                }
                State::Running => self.draw_tailnet(ui, &status),
            }

            if let Some(message) = self.tailscale.message() {
                ui.add_space(4.0);
                ui.label(egui::RichText::new(message).small());
            }
        });
    }

    /// Phần chỉ hiện khi Tailscale đã đăng nhập: địa chỉ máy này và danh sách máy khác.
    fn draw_tailnet(&mut self, ui: &mut egui::Ui, status: &crate::tailscale::Status) {
        let port = self.form.port;

        ui.horizontal(|ui| {
            ui.colored_label(ui::GOOD, "Đã kết nối tailnet");
            ui.label(egui::RichText::new(format!("máy này: {}", status.self_name)).small());
        });

        if let Some(ip) = &status.self_ip {
            let address = format!("{ip}:{port}");
            ui.horizontal(|ui| {
                ui.label("Địa chỉ để máy kia gõ:");
                ui.monospace(&address);
                if ui.small_button("Chép").clicked() {
                    ui.ctx().copy_text(address.clone());
                }
            });
        }

        ui.add_space(6.0);
        if status.peers.is_empty() {
            ui.label(
                egui::RichText::new(
                    "Chưa thấy máy nào khác. Đăng nhập cùng tài khoản Tailscale ở máy kia.",
                )
                .small(),
            );
            return;
        }

        ui.label(egui::RichText::new("Bấm một máy để điền sẵn địa chỉ:").small());
        // Nhiều máy thì đừng để danh sách đẩy hai nút chính xuống khỏi cửa sổ.
        egui::ScrollArea::vertical()
            .max_height(120.0)
            .show(ui, |ui| {
                for peer in &status.peers {
                    ui.horizontal(|ui| {
                        let label = if peer.os.is_empty() {
                            peer.name.clone()
                        } else {
                            format!("{} ({})", peer.name, peer.os)
                        };
                        // Máy đang tắt vẫn hiện ra — biết nó tồn tại mà đang tắt
                        // thì đỡ hơn là không thấy đâu và tưởng mình nhìn nhầm.
                        if ui
                            .add_enabled(peer.online, egui::Button::new(label))
                            .clicked()
                        {
                            self.form.target = format!("{}:{port}", peer.ip);
                            self.error = None;
                        }
                        ui.label(
                            egui::RichText::new(if peer.online { "đang bật" } else { "đang tắt" })
                                .small()
                                .color(if peer.online { ui::GOOD } else { ui::WARN }),
                        );
                    });
                }
            });
    }

    fn draw_start(&mut self, root: &mut egui::Ui) {
        egui::CentralPanel::default().show(root, |ui| {
            ui.add_space(16.0);
            ui.vertical_centered(|ui| {
                ui.heading("Điều khiển máy tính từ xa");
                ui.label(
                    egui::RichText::new(if self.can_10bit {
                        "card đồ hoạ hiển thị được 10-bit"
                    } else {
                        "card đồ hoạ chỉ hiển thị 8-bit"
                    })
                    .small(),
                );
            });
            ui.add_space(16.0);

            egui::Grid::new("cấu hình")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Tên hiện cho máy kia");
                    ui.text_edit_singleline(&mut self.form.name);
                    ui.end_row();

                    ui.label("Mật khẩu phiên");
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut self.form.password);
                        if ui.button("Đổi").clicked() {
                            self.form.password = random_password();
                        }
                    });
                    ui.end_row();

                    ui.label("Rendezvous server");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.form.rendezvous)
                            .hint_text("bỏ trống nếu nối thẳng trong mạng nhà hoặc qua Tailscale"),
                    );
                    ui.end_row();

                    ui.label("Thư mục nhận tệp");
                    ui.text_edit_singleline(&mut self.form.downloads);
                    ui.end_row();

                    ui.label("Chất lượng");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut self.form.fps).range(15..=240));
                        ui.label("fps");
                        ui.add(
                            egui::DragValue::new(&mut self.form.bitrate)
                                .range(1_000..=200_000)
                                .speed(500),
                        );
                        ui.label("kbps");
                    });
                    ui.end_row();

                    ui.label("Cổng chờ (khi chia sẻ)");
                    ui.add(egui::DragValue::new(&mut self.form.port).range(1024..=65535));
                    ui.end_row();
                });

            ui.add_space(14.0);
            self.draw_tailscale(ui);

            ui.add_space(16.0);
            ui.separator();
            ui.add_space(12.0);

            ui.columns(2, |columns| {
                columns[0].group(|ui| {
                    ui.set_min_height(150.0);
                    ui.heading("Cho điều khiển máy này");
                    ui.label(
                        egui::RichText::new(
                            "Máy này hiện màn hình cho người kia xem và nhận chuột phím của họ. \
                             Sau khi bấm sẽ có mã và mật khẩu để đọc cho người kia.",
                        )
                        .small(),
                    );
                    ui.add_space(8.0);
                    if ui
                        .add(egui::Button::new("Bắt đầu chia sẻ").min_size(egui::vec2(180.0, 32.0)))
                        .clicked()
                    {
                        self.start(Role::Host);
                    }
                });

                columns[1].group(|ui| {
                    ui.set_min_height(150.0);
                    ui.heading("Điều khiển máy khác");
                    ui.label(
                        egui::RichText::new(
                            "Gõ mã 9 chữ số của máy kia, hoặc địa chỉ IP:cổng nếu hai máy \
                             cùng mạng. Mật khẩu phải khớp với mật khẩu máy kia đang hiện.",
                        )
                        .small(),
                    );
                    ui.add_space(8.0);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.form.target)
                            .hint_text("123 456 789 hoặc 192.168.1.10:47823")
                            .desired_width(f32::INFINITY),
                    );
                    ui.add_space(8.0);
                    if ui
                        .add(egui::Button::new("Kết nối").min_size(egui::vec2(180.0, 32.0)))
                        .clicked()
                    {
                        self.start(Role::Viewer);
                    }
                });
            });

            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button("Thử tại chỗ (đo độ trễ không qua mạng)").clicked() {
                    self.start_local();
                }
                ui.label(
                    egui::RichText::new(
                        "chụp rồi giải mã ngay trong máy — con số nó cho ra là sàn \
                         mà chạy hai máy không thể vượt qua",
                    )
                    .small(),
                );
            });

            if let Some(error) = &self.error {
                ui.add_space(10.0);
                ui.colored_label(ui::BAD, error);
            }
        });
    }
}

impl eframe::App for RdApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let leave = match &mut self.screen {
            Screen::Start => {
                self.draw_start(ui);
                false
            }
            Screen::Session(state) => matches!(state.update(ui), Outcome::Leave),
            Screen::Local(state) => state.update(ui),
        };
        if leave {
            // Thả `SessionState` ở đây là dừng luồng mạng và đóng kết nối; màn
            // hình đầu phải sạch để bấm nối lại được ngay.
            self.screen = Screen::Start;
        }
    }
}

// ───────────────────────── chế độ thử tại chỗ ─────────────────────────

/// Chụp → mã hoá → giải mã ngay trong máy, không qua mạng.
pub struct LocalState {
    pipeline: Pipeline,
    current: Option<Arc<DecodedFrame>>,
    metrics: Metrics,
    capture: InputCapture,
    control: RemoteControl,
    show_hud: bool,
    last_report: Instant,
}

impl LocalState {
    fn start(fps: u32, bitrate: u32, allow_10bit: bool) -> anyhow::Result<Self> {
        Ok(Self {
            pipeline: Pipeline::start(fps, bitrate, allow_10bit)?,
            current: None,
            metrics: Metrics::default(),
            capture: InputCapture::new(),
            control: RemoteControl::new(),
            show_hud: true,
            last_report: Instant::now(),
        })
    }

    /// Trả về `true` khi người dùng muốn về màn hình đầu.
    fn update(&mut self, root: &mut egui::Ui) -> bool {
        let ctx = root.ctx().clone();
        if let Some(frame) = self.pipeline.latest() {
            self.metrics.push(
                frame.pipeline_us,
                frame.encode_us,
                frame.decode_us,
                frame.bytes,
                frame.keyframe,
            );
            self.current = Some(frame.frame);
        }

        ctx.input_mut(|i| {
            if i.key_pressed(egui::Key::F10) {
                self.show_hud = !self.show_hud;
            }
            if i.key_pressed(egui::Key::F9) {
                let on = !self.control.enabled();
                self.control.set_enabled(on);
            }
        });

        let mut leave = false;
        egui::Panel::top("thanh-local").show(root, |ui| {
            ui.horizontal(|ui| {
                if ui.button("← Thoát").clicked() {
                    leave = true;
                }
                ui.separator();
                ui.label("Thử tại chỗ");
                ui.separator();
                let counters = self.pipeline.counters();
                ui.label(
                    egui::RichText::new(format!(
                        "chụp {} · mã hoá {} · giải mã {} · bỏ {} · lỗi {}",
                        counters.captured,
                        counters.encoded,
                        counters.decoded,
                        counters.dropped_late,
                        counters.errors
                    ))
                    .monospace()
                    .small(),
                );
            });
        });

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(root, |ui| {
                let outer = ui.max_rect();
                ui::paint_backdrop(ui, outer);
                let info = self.pipeline.info().clone();
                let rect = ui::video_rect(outer, info.width, info.height);
                if let Some(frame) = &self.current {
                    ui::draw_video(ui, rect, frame);
                }

                if self.control.enabled() {
                    let events = ui.input(|i| i.events.clone());
                    let translated = self.capture.translate(&events, rect);
                    self.control.send(&translated);
                }

                if self.show_hud {
                    let lines = vec![
                        (
                            format!("điều khiển: {} (F9)", self.control.note()),
                            if self.control.enabled() {
                                ui::GOOD
                            } else {
                                ui::WARN
                            },
                        ),
                        ("F10 ẩn/hiện bảng này".to_string(), ui::WARN),
                    ];
                    let data = ui::HudData {
                        info: Some(&info),
                        summary: self.metrics.summary(),
                        lines,
                        network_ms: None,
                    };
                    ui::hud_at(ui, outer, &data);
                }
            });

        self.metrics.repaints += 1;
        if self.last_report.elapsed() >= std::time::Duration::from_secs(2) {
            self.last_report = Instant::now();
            let summary = self.metrics.summary();
            tracing::info!(
                fps = summary.fps,
                p50_ms = summary.pipeline_p50_ms,
                p99_ms = summary.pipeline_p99_ms,
                mbps = summary.mbps,
                "thống kê thử tại chỗ"
            );
        }
        ctx.request_repaint();
        leave
    }
}

// ───────────────────────────── tiện ích ─────────────────────────────

/// Hiểu cả hai cách người dùng chỉ tới máy kia: mã 9 chữ số, hay địa chỉ mạng.
fn parse_peer(text: &str) -> Result<PeerAddress, String> {
    if text.is_empty() {
        return Err("chưa nhập mã hoặc địa chỉ của máy kia".into());
    }
    // Thử mã trước: "123456789" cũng là tên miền hợp lệ về mặt cú pháp, nên để
    // phân giải tên chạy trước là mã sẽ không bao giờ tới lượt.
    if let Ok(id) = PeerId::from_str(text) {
        return Ok(PeerAddress::Code(id));
    }
    resolve(text).map(PeerAddress::Direct)
}

/// Đổi "máy:cổng" thành địa chỉ. Thiếu cổng thì hiểu là cổng mặc định.
fn resolve(text: &str) -> Result<SocketAddr, String> {
    let with_port = if text.contains(':') {
        text.to_string()
    } else {
        format!("{text}:{DEFAULT_PORT}")
    };
    with_port
        .to_socket_addrs()
        .map_err(|err| format!("không hiểu địa chỉ {text}: {err}"))?
        .next()
        .ok_or_else(|| format!("{text} không phân giải ra địa chỉ nào"))
}

/// Sáu chữ số ngẫu nhiên. Đủ ngắn để đọc qua điện thoại, và mỗi lần chạy một
/// khác nên không có mật khẩu mặc định để ai đó thử.
fn random_password() -> String {
    use rand::Rng as _;
    format!("{:06}", rand::rng().random_range(0..1_000_000u32))
}

fn machine_name() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "máy không tên".into())
}

fn default_downloads() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    home.join("Downloads")
}

#[cfg(all(test, any(target_os = "macos", target_os = "windows")))]
mod tests {
    use super::*;

    /// Mọi chữ Việt có dấu dùng trong giao diện phải có glyph thật.
    ///
    /// Thiếu thì egui vẽ ra ô vuông — đúng lỗi mà font mặc định của nó gây ra,
    /// vì font ấy dừng ở Latin Extended-A.
    const CHU_KHO: &str = "ềửọậợỹăđâêôơưựếịốồùáíãõẻẩ";

    /// Kết thúc lượt vẽ và vứt bỏ texture vừa dựng. Không dọn thì `FullOutput`
    /// nổ lúc `Drop` vì tưởng người gọi quên đẩy texture lên GPU.
    fn end_pass(ctx: &egui::Context) {
        let mut out = ctx.end_pass();
        out.textures_delta.clear();
    }

    /// Chỉ kiểm họ chữ thường (`Proportional`) — gần như toàn bộ chữ trong giao
    /// diện nằm ở đó. Họ chữ đều nhau không kiểm được bằng cách này: epaint coi
    /// một ký tự là "thiếu" khi nó rơi vào đúng font đang giữ ký tự thay thế,
    /// mà font ấy lại là Hack đứng đầu họ, nên mọi chữ Hack có đều bị báo thiếu
    /// oan. Ở đó font hệ thống đứng cuối làm lớp đỡ, Hack thiếu chữ nào thì nó
    /// nhận chữ đó.
    #[test]
    fn font_giao_dien_du_chu_viet() {
        let ctx = egui::Context::default();
        install_vietnamese_font(&ctx);
        // egui chỉ dựng bộ font khi bắt đầu vẽ, trước đó chưa hỏi được.
        ctx.begin_pass(Default::default());
        let thieu: Vec<char> = ctx.fonts_mut(|f| {
            CHU_KHO
                .chars()
                .filter(|c| !f.has_glyph(&egui::FontId::proportional(14.0), *c))
                .collect()
        });
        end_pass(&ctx);

        assert!(thieu.is_empty(), "font giao diện thiếu chữ {thieu:?}");
    }

    /// Chốt lại rằng test trên có ý nghĩa: font mặc định của egui đúng là
    /// thiếu. Ngày nào egui đổi font mặc định thành font đủ chữ thì test này
    /// đỏ, và đó là lúc bỏ hẳn được `install_vietnamese_font`.
    #[test]
    fn font_mac_dinh_cua_egui_van_thieu_chu_viet() {
        let ctx = egui::Context::default();
        ctx.begin_pass(Default::default());
        let du = ctx.fonts_mut(|f| f.has_glyphs(&egui::FontId::proportional(14.0), CHU_KHO));
        end_pass(&ctx);

        assert!(!du, "egui đã có sẵn chữ Việt, không cần mượn font hệ thống nữa");
    }
}
