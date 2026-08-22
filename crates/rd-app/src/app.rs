//! Màn hình đầu và bộ máy trạng thái của cả chương trình.

use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use eframe::egui;

#[cfg(target_os = "windows")]
use rd_codec::mediafoundation::DecodedFrame;
#[cfg(target_os = "macos")]
use rd_codec::videotoolbox::DecodedFrame;

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
/// Cỡ chữ và khoảng cách chung.
///
/// Mặc định của egui hợp với công cụ lập trình: chữ 14 điểm, khoảng cách sát
/// nhau. Đây là phần mềm cho người dùng thường ngồi cách màn hình cả mét khi
/// điều khiển máy khác, nên nhích chữ và khoảng thở lên một chút.
fn apply_style(ctx: &egui::Context) {
    use egui::{FontFamily::Monospace, FontFamily::Proportional, FontId, TextStyle};

    // egui giữ style riêng cho từng chủ đề sáng/tối; đặt cả hai để đổi chủ đề
    // không làm chữ nhảy cỡ.
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        ctx.style_mut_of(theme, |style| {
            style.text_styles = [
                (TextStyle::Heading, FontId::new(21.0, Proportional)),
                (TextStyle::Body, FontId::new(15.0, Proportional)),
                (TextStyle::Button, FontId::new(15.0, Proportional)),
                (TextStyle::Small, FontId::new(12.5, Proportional)),
                (TextStyle::Monospace, FontId::new(14.0, Monospace)),
            ]
            .into();
            style.spacing.item_spacing = egui::vec2(8.0, 7.0);
            style.spacing.button_padding = egui::vec2(10.0, 5.0);
            style.visuals.window_corner_radius = 8.into();
        });
    }
}

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
    --background           chia sẻ ngay, cửa sổ thu nhỏ sẵn (dùng khi tự khởi động)
    --connect ĐỊA_CHỈ|MÃ   vào thẳng chế độ điều khiển máy khác
    --password MK          mật khẩu phiên (mặc định: sinh ngẫu nhiên)
    --rendezvous ĐỊA_CHỈ   server hẹn gặp, để nối qua internet
                           thêm #vân_tay để ghim: rd.vidu.com:7000#a1b2...
    --name TÊN             tên hiện cho máy kia
    --fps N                số hình mỗi giây (mặc định 60)
    --bitrate KBPS         băng thông video (mặc định 30000)
    --port CỔNG            cổng chờ khi chia sẻ (mặc định 47823)
    --downloads THƯ_MỤC    nơi lưu tệp nhận được
    --trust-tailnet        máy cùng tài khoản Tailscale vào thẳng, khỏi mật khẩu
    --probe                in khả năng của máy này rồi thoát
    --tu-kiem              tự kiểm clipboard và mục tự khởi động rồi thoát
    --screenshot TỆP       chụp cửa sổ ra tệp .ppm rồi thoát (dùng khi sửa giao diện)
                           thêm cai-dat: trước đường dẫn để chụp trang cài đặt
    -h, --help             in bảng này
";

/// Tham số dòng lệnh. Tất cả đều tuỳ chọn — chúng chỉ điền sẵn màn hình đầu.
#[derive(Debug, Clone)]
pub struct Args {
    /// Người dùng chỉ muốn xem cách dùng; đừng mở cửa sổ.
    pub help: bool,
    /// In khả năng của máy rồi thoát, không mở cửa sổ.
    /// Bật thẳng từ dòng lệnh để máy chạy nền không cần ai mở giao diện tick
    /// ô này. Không gõ cờ thì giữ nguyên thiết lập đã lưu.
    pub trust_tailnet: bool,
    pub probe: bool,
    /// Tự kiểm clipboard và mục tự khởi động rồi thoát. Máy dựng bản Windows ở
    /// đây là macOS nên hai thứ ấy chỉ máy Windows thật mới trả lời được — cờ
    /// này gói cả ba phép thử vào một lệnh để người dùng chạy hộ.
    pub selftest: bool,
    pub host: bool,
    /// Chạy ngầm: cửa sổ thu nhỏ sẵn, vào thẳng chế độ chia sẻ. Dùng cho lần tự
    /// khởi động sau khi máy có điện lại.
    pub background: bool,
    pub connect: Option<String>,
    pub password: Option<String>,
    pub rendezvous: Option<String>,
    pub name: Option<String>,
    pub fps: u32,
    pub bitrate: u32,
    pub downloads: Option<PathBuf>,
    pub port: u16,
    /// Chụp cửa sổ ra tệp .ppm rồi thoát. Cờ dành cho lúc sửa giao diện: máy
    /// dựng bản Windows ở đây là máy macOS, không có cách nào nhìn thấy giao
    /// diện thật ngoài việc bảo chương trình tự chụp lấy.
    pub screenshot: Option<PathBuf>,
    /// Ảnh chụp cần lấy trang cài đặt thay vì màn hình đầu.
    pub screenshot_settings: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            help: false,
            trust_tailnet: false,
            probe: false,
            selftest: false,
            host: false,
            background: false,
            connect: None,
            password: None,
            rendezvous: None,
            name: None,
            fps: Args::DEFAULT_FPS,
            bitrate: Args::DEFAULT_BITRATE,
            downloads: None,
            port: DEFAULT_PORT,
            screenshot: None,
            screenshot_settings: false,
        }
    }
}

impl Args {
    /// 60 fps là nhịp quét của gần hết màn hình đang dùng. Đặt cao hơn chỉ tốn
    /// băng thông cho những frame màn hình không hiện kịp.
    pub const DEFAULT_FPS: u32 = 60;
    pub const DEFAULT_BITRATE: u32 = 30_000;
    pub const DEFAULT_PORT: u16 = DEFAULT_PORT;

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
                "--trust-tailnet" => out.trust_tailnet = true,
                "--probe" => out.probe = true,
                "--tu-kiem" | "--selftest" => out.selftest = true,
                "--host" => out.host = true,
                "--background" => {
                    out.background = true;
                    out.host = true;
                }
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
                "--screenshot" => {
                    let raw = value()?;
                    // Tiền tố chọn màn hình cần chụp; không có thì chụp màn
                    // hình đầu.
                    match raw.strip_prefix("cai-dat:") {
                        Some(path) => {
                            out.screenshot = Some(PathBuf::from(path));
                            out.screenshot_settings = true;
                        }
                        None => out.screenshot = Some(PathBuf::from(raw)),
                    }
                }
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
    /// Cho máy cùng tài khoản Tailscale vào thẳng, khỏi mật khẩu.
    trust_tailnet: bool,
    prefs: Prefs,
    recents: Vec<RecentPeer>,
}

impl Form {
    /// Thứ tự ưu tiên: cờ dòng lệnh, rồi thiết lập lần chạy trước, rồi mặc
    /// định. Nhờ vế giữa mà mật khẩu không đổi sau mỗi lần bật máy — máy host
    /// mất điện rồi tự bật lại vẫn dùng đúng mật khẩu người kia đang giữ.
    fn from_args(args: &Args) -> Self {
        let saved = Settings::load();
        let mac_dinh = Prefs::default();
        Self {
            target: args.connect.clone().or(saved.target).unwrap_or_default(),
            password: args
                .password
                .clone()
                .or(saved.password)
                .unwrap_or_else(random_password),
            rendezvous: args
                .rendezvous
                .clone()
                .or(saved.rendezvous)
                .unwrap_or_default(),
            name: args
                .name
                .clone()
                .or(saved.name)
                .unwrap_or_else(machine_name),
            // Hai số này luôn có giá trị mặc định từ clap nên không phân biệt
            // được "người dùng gõ" với "để yên"; lấy bản đã lưu khi cờ đang ở
            // đúng mức mặc định.
            fps: if args.fps == Args::DEFAULT_FPS {
                saved.fps.unwrap_or(args.fps)
            } else {
                args.fps
            },
            bitrate: if args.bitrate == Args::DEFAULT_BITRATE {
                saved.bitrate.unwrap_or(args.bitrate)
            } else {
                args.bitrate
            },
            downloads: args
                .downloads
                .clone()
                .map(|path| path.display().to_string())
                .or(saved.downloads)
                .unwrap_or_else(|| default_downloads().display().to_string()),
            port: if args.port == Args::DEFAULT_PORT {
                saved.port.unwrap_or(args.port)
            } else {
                args.port
            },
            // Cờ chỉ bật lên được, không tắt: người gõ `--trust-tailnet` là
            // đang muốn bật, còn không gõ thì họ chưa nói gì nên giữ nguyên
            // thiết lập cũ.
            trust_tailnet: args.trust_tailnet || saved.trust_tailnet.unwrap_or(false),
            recents: saved.recents.unwrap_or_default(),
            prefs: Prefs {
                local_cursor: saved.local_cursor.unwrap_or(mac_dinh.local_cursor),
                auto_accept_files: saved
                    .auto_accept_files
                    .unwrap_or(mac_dinh.auto_accept_files),
                clipboard_sync: saved.clipboard_sync.unwrap_or(mac_dinh.clipboard_sync),
                start_fullscreen: saved.start_fullscreen.unwrap_or(mac_dinh.start_fullscreen),
                hide_chrome: saved.hide_chrome.unwrap_or(mac_dinh.hide_chrome),
                show_hud: saved.show_hud.unwrap_or(mac_dinh.show_hud),
                auto_reconnect: saved.auto_reconnect.unwrap_or(mac_dinh.auto_reconnect),
            },
        }
    }

    /// Ghi lại những gì người dùng đang gõ để lần chạy sau khỏi gõ nữa.
    fn save(&self) {
        Settings {
            target: Some(self.target.clone()),
            password: Some(self.password.clone()),
            rendezvous: Some(self.rendezvous.clone()),
            name: Some(self.name.clone()),
            fps: Some(self.fps),
            bitrate: Some(self.bitrate),
            downloads: Some(self.downloads.clone()),
            port: Some(self.port),
            trust_tailnet: Some(self.trust_tailnet),
            local_cursor: Some(self.prefs.local_cursor),
            auto_accept_files: Some(self.prefs.auto_accept_files),
            clipboard_sync: Some(self.prefs.clipboard_sync),
            start_fullscreen: Some(self.prefs.start_fullscreen),
            hide_chrome: Some(self.prefs.hide_chrome),
            show_hud: Some(self.prefs.show_hud),
            auto_reconnect: Some(self.prefs.auto_reconnect),
            recents: Some(self.recents.clone()),
        }
        .save();
    }

    fn config(&self, role: Role, allow_10bit: bool) -> Result<NetConfig, String> {
        let (rendezvous, rendezvous_fingerprint) = match self.rendezvous.trim() {
            "" => (None, None),
            text => {
                let (addr, fingerprint) = split_pin(text)?;
                (Some(resolve(addr)?), fingerprint)
            }
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
            rendezvous_fingerprint,
            trust_tailnet: self.trust_tailnet,
            peer_fingerprint: None,
            peer,
            name: self.name.trim().to_string(),
            password: self.password.trim().to_string(),
            target_fps: self.fps.clamp(1, 240),
            bitrate_kbps: self.bitrate.clamp(500, 500_000),
            allow_10bit,
            download_dir: PathBuf::from(self.downloads.trim()),
            // Phiên tự gắn đường clipboard của nó vào đây.
            clipboard: Default::default(),
        })
    }
}

/// Một máy đã từng nối tới, để lần sau bấm một cái là vào.
///
/// Giữ cả mật khẩu vì mật khẩu phiên là thứ dài và vô nghĩa với người dùng;
/// bắt họ đọc lại 6 chữ số mỗi lần nối vào đúng cái máy tính của chính mình
/// thì chẳng ai chịu dùng. Tệp thiết lập nằm trong hồ sơ người dùng, cùng mức
/// bảo vệ như mọi thứ khác trong đó.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct RecentPeer {
    /// Mã 9 chữ số hoặc IP:cổng — đúng thứ gõ vào ô "Điều khiển máy khác".
    pub target: String,
    /// Tên máy kia tự khai lúc nối được. Rỗng khi chưa nối lần nào.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub password: String,
}

/// Giữ tối đa ngần này máy gần đây. Nhiều hơn thì danh sách dài hơn cả phần
/// còn lại của màn hình.
const MAX_RECENTS: usize = 5;

/// Tuỳ chọn trải nghiệm.
///
/// Tách khỏi [`NetConfig`] vì chúng chỉ đổi cách giao diện cư xử: không đi trên
/// dây, không cần khớp với máy kia, và đổi lúc nào cũng được.
#[derive(Clone, Copy)]
pub struct Prefs {
    /// Vẽ con trỏ ngay tại chỗ chuột đang ở thay vì chờ thấy nó trong hình.
    pub local_cursor: bool,
    /// Nhận tệp mà không phải bấm đồng ý từng cái.
    pub auto_accept_files: bool,
    pub clipboard_sync: bool,
    /// Vào phiên là bung toàn màn hình luôn.
    pub start_fullscreen: bool,
    /// Vào phiên là ẩn thanh trên và bảng bên, chỉ còn hình.
    pub hide_chrome: bool,
    pub show_hud: bool,
    /// Mất kết nối thì tự nối lại thay vì bỏ phiên.
    pub auto_reconnect: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            // Bật sẵn: đây là thứ quyết định cảm giác "mượt" khi mạng có độ
            // trễ, mà lại không tốn gì cả.
            local_cursor: true,
            auto_accept_files: false,
            clipboard_sync: true,
            start_fullscreen: false,
            hide_chrome: false,
            show_hud: true,
            // Bật sẵn: máy kia mất điện, rớt wifi hay ngủ màn hình là chuyện
            // thường ngày, và phiên phải sống qua được những lần đó.
            auto_reconnect: true,
        }
    }
}

/// Thiết lập nhớ giữa hai lần chạy.
///
/// Mọi trường đều `Option` để bản ghi cũ thiếu trường vẫn đọc được: thêm mục
/// mới vào đây không được phép làm hỏng thiết lập người dùng đã có.
#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Settings {
    target: Option<String>,
    password: Option<String>,
    rendezvous: Option<String>,
    name: Option<String>,
    fps: Option<u32>,
    bitrate: Option<u32>,
    downloads: Option<String>,
    port: Option<u16>,
    trust_tailnet: Option<bool>,
    local_cursor: Option<bool>,
    auto_accept_files: Option<bool>,
    clipboard_sync: Option<bool>,
    start_fullscreen: Option<bool>,
    hide_chrome: Option<bool>,
    show_hud: Option<bool>,
    auto_reconnect: Option<bool>,
    recents: Option<Vec<RecentPeer>>,
}

impl Settings {
    fn load() -> Self {
        let Some(path) = settings_path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match serde_json::from_str(&text) {
            Ok(settings) => settings,
            // Tệp hỏng thì bắt đầu lại từ mặc định chứ không chặn khởi động.
            Err(err) => {
                tracing::warn!(%err, path = %path.display(), "thiết lập đã lưu không đọc được");
                Self::default()
            }
        }
    }

    fn save(&self) {
        let Some(path) = settings_path() else {
            return;
        };
        if let Some(dir) = path.parent()
            && let Err(err) = std::fs::create_dir_all(dir)
        {
            tracing::warn!(%err, "không tạo được thư mục thiết lập");
            return;
        }
        match serde_json::to_string_pretty(self) {
            Ok(text) => {
                if let Err(err) = std::fs::write(&path, text) {
                    tracing::warn!(%err, path = %path.display(), "không ghi được thiết lập");
                }
            }
            Err(err) => tracing::warn!(%err, "không dựng được JSON thiết lập"),
        }
    }
}

/// `%LOCALAPPDATA%\RemoteDesktop` trên Windows, `~/Library/Application Support`
/// trên macOS — đúng chỗ mỗi hệ quy định, nên bản cài gỡ đi là dọn theo được.
pub fn app_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
    #[cfg(not(target_os = "windows"))]
    let base = PathBuf::from(std::env::var_os("HOME")?).join("Library/Application Support");
    Some(base.join("RemoteDesktop"))
}

fn settings_path() -> Option<PathBuf> {
    Some(app_dir()?.join("settings.json"))
}

enum Screen {
    Start,
    Settings,
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
    /// Hộp thoại chọn thư mục nhận tệp, khi người dùng bấm nút cạnh ô đó.
    picker: crate::os::FilePicker,
    /// Phần mềm có nằm trong mục tự khởi động của hệ điều hành không. Hỏi hệ
    /// một lần lúc mở, vì mỗi lần hỏi là một tiến trình con.
    autostart: bool,
    error: Option<String>,
    /// Còn phải thu nhỏ cửa sổ không (chế độ `--background`).
    minimize: bool,
    /// Nơi lưu ảnh chụp và số khung đã vẽ, khi chạy với `--screenshot`.
    screenshot: Option<(PathBuf, u32)>,
    /// Dò xem máy trong tailnet có đang mở phần mềm này không.
    prober: crate::probe::Prober,
    /// Đã tự mở khung Tailscale ra lần nào chưa. Chỉ mở đúng một lượt, sau đó
    /// đóng hay mở là quyền người dùng.
    da_mo_tailnet: bool,
    /// Chữ đang gõ ở ô lọc danh sách máy trong tailnet. Một tài khoản
    /// Tailscale quản được hàng trăm máy, cuộn tay để tìm là không xong.
    loc_may: String,
    /// Chiều cao khối nội dung màn hình đầu ở lượt vẽ trước, để căn giữa dọc.
    start_height: f32,
    /// Chiều cao thẻ cao nhất ở lượt vẽ trước, để hai thẻ bằng nhau.
    card_height: f32,
}

/// Cổng đã dùng lần trước khi nối tới đúng địa chỉ này.
fn cong_da_dung(recents: &[RecentPeer], ip: &str) -> Option<u16> {
    recents.iter().find_map(|item| {
        let (dia_chi, cong) = item.target.rsplit_once(':')?;
        (dia_chi == ip).then(|| cong.parse().ok())?
    })
}

/// Từ bao nhiêu máy trở lên thì hiện ô lọc danh sách tailnet.
const NGUONG_LOC: usize = 6;

/// Bề ngang tối đa của nội dung màn hình đầu và trang cài đặt.
///
/// Cửa sổ rộng 1280 điểm mà chữ trải hết bề ngang thì mắt phải quét cả gang
/// tay cho một dòng bốn chữ. Giữ trong khoảng này rồi căn giữa.
const CONTENT_WIDTH: f32 = 860.0;

/// Chiều cao tối thiểu của hai thẻ hành động, để chúng bằng nhau.
const CARD_HEIGHT: f32 = 118.0;

/// Chừa hai bên bao nhiêu khi cửa sổ hẹp hơn `CONTENT_WIDTH`.
const SIDE_MARGIN: f32 = 24.0;

/// Đặt một cột nội dung rộng `width` vào giữa bề ngang, chữ vẫn canh trái.
///
/// `vertical_centered` của egui canh giữa từng dòng con một, nên tiêu đề và ô
/// đánh dấu mỗi cái lệch một kiểu — đọc rất mệt. Ở đây chỉ đẩy nguyên cột vào
/// giữa, bên trong vẫn là cột canh trái bình thường.
fn centered_column(ui: &mut egui::Ui, width: f32, add: impl FnOnce(&mut egui::Ui)) {
    // Cửa sổ hẹp hơn cột thì cột co lại chứ không tràn ra ngoài.
    let width = width.min(ui.available_width() - 2.0 * SIDE_MARGIN);
    let margin = ((ui.available_width() - width) / 2.0).max(0.0);
    ui.horizontal(|ui| {
        ui.add_space(margin);
        ui.vertical(|ui| {
            ui.set_max_width(width);
            add(ui);
        });
    });
}

impl RdApp {
    pub fn new(cc: &eframe::CreationContext<'_>, args: Args) -> anyhow::Result<Self> {
        let can_10bit = cc
            .wgpu_render_state
            .as_ref()
            .map(|state| video::supports_10bit(&state.device))
            .unwrap_or(false);
        tracing::info!(can_10bit, "khởi động giao diện");

        // Bộ vẽ video phải nằm trong kho tài nguyên của egui: hàm vẽ lại chỉ
        // được đưa cho đúng cái kho này, ngoài ra không thấy gì khác. Quên đặt
        // vào là `paint` không tìm thấy bộ vẽ rồi lặng lẽ bỏ qua — ô video đen
        // thui trong khi mạng, giải mã và fps đều báo bình thường.
        match &cc.wgpu_render_state {
            Some(state) => video::install(state),
            None => tracing::error!("không có wgpu, sẽ không vẽ được video"),
        }
        install_vietnamese_font(&cc.egui_ctx);
        apply_style(&cc.egui_ctx);

        let form = Form::from_args(&args);
        let mut app = Self {
            screen: if args.screenshot_settings {
                Screen::Settings
            } else {
                Screen::Start
            },
            form,
            can_10bit,
            tailscale: crate::tailscale::Watcher::new(),
            picker: crate::os::FilePicker::default(),
            autostart: crate::os::autostart_enabled(),
            error: None,
            screenshot: args.screenshot.clone().map(|path| (path, 0)),
            prober: Default::default(),
            da_mo_tailnet: false,
            loc_may: String::new(),
            start_height: 0.0,
            card_height: 0.0,
            // Thu nhỏ ở khung vẽ đầu tiên chứ không phải ngay đây: cửa sổ chưa
            // tồn tại thì lệnh gửi đi rơi vào hư không.
            minimize: args.background,
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
        if role == Role::Viewer {
            self.remember_target();
        }
        // Lưu trước khi nối: nối hỏng vì gõ sai địa chỉ thì lần sau vẫn còn
        // nguyên những gì đã gõ để sửa, khỏi điền lại từ đầu.
        self.form.save();
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
        match SessionState::start(config, tailscale_ip, self.form.prefs) {
            Ok(state) => {
                self.error = None;
                self.screen = Screen::Session(Box::new(state));
            }
            Err(err) => self.error = Some(err.to_string()),
        }
    }

    /// Đưa máy vừa gõ lên đầu danh sách gần đây.
    fn remember_target(&mut self) {
        let target = self.form.target.trim().to_string();
        if target.is_empty() {
            return;
        }
        let password = self.form.password.trim().to_string();
        // Nối lại cùng một máy không được đẩy nó thành hai dòng; bỏ bản cũ rồi
        // chèn bản mới lên đầu.
        let name = self
            .form
            .recents
            .iter()
            .find(|item| item.target == target)
            .map(|item| item.name.clone())
            .unwrap_or_default();
        self.form.recents.retain(|item| item.target != target);
        self.form.recents.insert(
            0,
            RecentPeer {
                target,
                name,
                password,
            },
        );
        self.form.recents.truncate(MAX_RECENTS);
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
            ui.set_width(ui.available_width());
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
        // Chép ra trước: vòng vẽ danh sách bên dưới mượn `self` kiểu mutable.
        let recents = self.form.recents.clone();

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

        let mut tin = self.form.trust_tailnet;
        let o = ui.checkbox(
            &mut tin,
            "Máy cùng tài khoản Tailscale vào thẳng, khỏi mật khẩu",
        );
        if o.changed() {
            self.form.trust_tailnet = tin;
            self.form.save();
        }
        o.on_hover_text(
            "Khi chia sẻ, host hỏi Tailscale xem máy đang gọi là máy nào. Cùng tài khoản \
             thì cho vào luôn — tiện khi bạn có nhiều máy và không muốn nhớ mật khẩu của \
             từng cái.\n\nĐổi lại, ai vào được tài khoản Tailscale của bạn là vào được máy \
             này. Máy của khách được chia sẻ vào tailnet vẫn phải gõ mật khẩu.",
        );

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
        // Danh sách ngắn thì ô lọc chỉ tổ chiếm chỗ; dài thì không có nó là
        // phải cuộn mò từng dòng.
        if status.peers.len() > NGUONG_LOC {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.loc_may)
                        .hint_text("lọc theo tên máy")
                        .desired_width(200.0),
                );
                if !self.loc_may.is_empty() && ui.small_button("Xoá").clicked() {
                    self.loc_may.clear();
                }
            });
        }
        let loc = self.loc_may.trim().to_lowercase();
        let hien: Vec<_> = status
            .peers
            .iter()
            .filter(|peer| loc.is_empty() || peer.name.to_lowercase().contains(&loc))
            .collect();
        if hien.is_empty() {
            ui.label(egui::RichText::new("Không máy nào khớp.").small());
            return;
        }

        // Nhiều máy thì đừng để danh sách đẩy hai nút chính xuống khỏi cửa sổ.
        egui::ScrollArea::vertical()
            .max_height(120.0)
            .show(ui, |ui| {
                for peer in hien {
                    // Cổng của lần nối trước tới đúng máy này, nếu có. Mỗi máy
                    // đặt cổng riêng được, mà danh sách thì chỉ biết địa chỉ —
                    // lấy cổng của máy đang ngồi gán cho máy kia là đoán mò.
                    let cong = cong_da_dung(&recents, &peer.ip).unwrap_or(port);
                    let address = format!("{}:{cong}", peer.ip);
                    let tinh = match (peer.online, address.parse()) {
                        (true, Ok(addr)) => Some(self.prober.hoi(addr)),
                        _ => None,
                    };
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
                            self.form.target = address.clone();
                            self.error = None;
                        }
                        // Máy đang tắt thì "tắt từ bao giờ" mới là thứ cần
                        // biết: tắt 5 phút trước là đang khởi động lại, tắt 3
                        // ngày trước là quên chưa bật.
                        let tinh_trang = if peer.online {
                            match tinh {
                                Some(crate::probe::Tinh::Co) => "sẵn sàng".to_string(),
                                Some(crate::probe::Tinh::Khong) => {
                                    format!("bật, chưa mở phần mềm (cổng {cong})")
                                }
                                _ => "đang bật, đang dò…".to_string(),
                            }
                        } else if peer.last_seen.is_empty() {
                            "đang tắt".to_string()
                        } else {
                            format!("tắt, thấy lần cuối {}", peer.last_seen)
                        };
                        let mau = match (peer.online, tinh) {
                            (true, Some(crate::probe::Tinh::Co)) => ui::GOOD,
                            (true, Some(crate::probe::Tinh::Khong)) => ui::WARN,
                            (true, _) => egui::Color32::GRAY,
                            (false, _) => ui::WARN,
                        };
                        ui.label(egui::RichText::new(tinh_trang).small().color(mau));
                    });
                }
            });
    }

    fn draw_start(&mut self, root: &mut egui::Ui) {
        egui::Panel::top("thanh-dau").show(root, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Điều khiển máy tính từ xa");
                ui.label(
                    egui::RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                        .small()
                        .color(egui::Color32::GRAY),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⚙ Cài đặt").clicked() {
                        self.screen = Screen::Settings;
                    }
                });
            });
        });

        egui::Panel::bottom("thanh-cuoi").show(root, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .small_button("Thử tại chỗ")
                    .on_hover_text(
                        "Chụp rồi giải mã ngay trong máy, không qua mạng — con số nó cho ra \
                         là sàn mà chạy hai máy không thể vượt qua.",
                    )
                    .clicked()
                {
                    self.start_local();
                }
                ui.separator();
                ui.label(
                    egui::RichText::new(if self.can_10bit {
                        "card đồ hoạ hiển thị được 10-bit"
                    } else {
                        "card đồ hoạ chỉ hiển thị 8-bit"
                    })
                    .small()
                    .color(egui::Color32::GRAY),
                );
            });
        });
        egui::CentralPanel::default().show(root, |ui| {
            // Nội dung màn hình đầu chỉ có vài dòng; kéo giãn nó ra hết bề
            // ngang 1280 điểm thì chữ nằm rải rác còn khoảng giữa trống hoác.
            // Giới hạn bề ngang, căn giữa cả hai chiều.
            egui::ScrollArea::vertical().show(ui, |ui| {
                // Chiều cao đo được ở lượt vẽ trước. egui không biết trước nội
                // dung cao bao nhiêu, mà bố cục này không đổi giữa hai lượt nên
                // dùng số cũ là khớp; lượt đầu tiên lệch một khung, không thấy.
                // Đặt hơi cao hơn chính giữa: căn giữa hẳn thì trên màn hình
                // lớn khối nội dung trôi lửng lơ giữa hai mảng trống.
                let du = (ui.available_height() - self.start_height) / 2.0;
                ui.add_space(du.clamp(8.0, 120.0));
                let truoc = ui.cursor().top();
                centered_column(ui, CONTENT_WIDTH, |ui| self.draw_start_body(ui));
                self.start_height = ui.cursor().top() - truoc;
            });
        });
    }

    fn draw_start_body(&mut self, ui: &mut egui::Ui) {
        // Hai ô này đi cùng nhau ở mọi phiên nên để ngay đầu; những thứ chỉnh
        // một lần rồi thôi đã dọn sang trang cài đặt.
        ui.horizontal(|ui| {
            ui.label("Tên máy này");
            ui.add(
                egui::TextEdit::singleline(&mut self.form.name)
                    .desired_width(180.0)
                    .hint_text("tên hiện cho máy kia"),
            );
            ui.add_space(12.0);
            ui.label("Mật khẩu");
            ui.add(
                egui::TextEdit::singleline(&mut self.form.password)
                    .desired_width(130.0)
                    .font(egui::TextStyle::Monospace),
            );
            if ui.small_button("Chép").clicked() {
                ui.ctx().copy_text(self.form.password.clone());
            }
            if ui.small_button("Đổi").clicked() {
                self.form.password = random_password();
                self.form.save();
            }
        });

        ui.add_space(10.0);

        // Bấm nối ở trong `columns` thì `self` đang bị mượn; gom quyết định lại
        // rồi làm sau khi vẽ xong.
        let mut noi = false;
        let mut chia_se = false;
        let mut chon = None;
        // Thẻ trái có thêm mấy dòng địa chỉ nên tự nhiên cao hơn; lấy chiều cao
        // lớn nhất của lượt trước làm sàn cho cả hai thì mép dưới thẳng hàng.
        let cao_the = self.card_height.max(CARD_HEIGHT);
        let mut do_cao: f32 = 0.0;
        ui.columns(2, |columns| {
            columns[0].group(|ui| {
                // Không ép bề ngang thì thẻ co lại vừa nội dung, và hai thẻ
                // cạnh nhau rộng hẹp khác nhau trông như lỗi.
                ui.set_width(ui.available_width());
                ui.set_min_height(cao_the);
                let noi_dung = ui.vertical(|ui| {
                    ui.strong("Cho điều khiển máy này");
                    ui.label(
                        egui::RichText::new(
                            "Hiện màn hình máy này cho người kia và nhận chuột phím của họ.",
                        )
                        .small(),
                    );
                    ui.add_space(8.0);
                    if ui
                        .add(egui::Button::new("Bắt đầu chia sẻ").min_size(egui::vec2(160.0, 30.0)))
                        .clicked()
                    {
                        chia_se = true;
                    }
                    // Địa chỉ hiện luôn ở đây, chưa cần bấm gì: người kia hỏi
                    // "gõ gì bây giờ" là đọc được ngay, không phải vào phiên
                    // rồi mới thấy.
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new("Máy kia sẽ gõ:").small());
                    if let Some(ip) = crate::session::lan_ip() {
                        ui.label(
                            egui::RichText::new(format!("{ip}:{}  (cùng mạng)", self.form.port))
                                .small()
                                .monospace(),
                        );
                    }
                    if let Some(ip) = self.tailscale.snapshot().self_ip.clone() {
                        ui.label(
                            egui::RichText::new(format!("{ip}:{}  (Tailscale)", self.form.port))
                                .small()
                                .monospace(),
                        );
                    }
                });
                do_cao = do_cao.max(noi_dung.response.rect.height());
            });

            columns[1].group(|ui| {
                ui.set_width(ui.available_width());
                ui.set_min_height(cao_the);
                let noi_dung = ui.vertical(|ui| {
                    ui.strong("Điều khiển máy khác");
                    ui.label(
                        egui::RichText::new(
                            "Địa chỉ Tailscale 100.x của máy kia, hoặc IP:cổng nếu cùng mạng.",
                        )
                        .small(),
                    );
                    ui.add_space(6.0);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.form.target)
                            .hint_text("100.68.1.20:47823 hoặc 192.168.1.10:47823")
                            .desired_width(f32::INFINITY),
                    );
                    ui.add_space(6.0);
                    if ui
                        .add(egui::Button::new("Kết nối").min_size(egui::vec2(160.0, 30.0)))
                        .clicked()
                    {
                        noi = true;
                    }
                    if !self.form.recents.is_empty() {
                        ui.add_space(8.0);
                        ui.label(egui::RichText::new("Gần đây").small());
                    }
                    for item in &self.form.recents {
                        let nhan = if item.name.is_empty() {
                            item.target.clone()
                        } else {
                            format!("{} — {}", item.name, item.target)
                        };
                        if ui
                            .add(egui::Button::new(egui::RichText::new(nhan).small()).frame(false))
                            .on_hover_text("Bấm để điền và nối lại ngay")
                            .clicked()
                        {
                            chon = Some(item.clone());
                        }
                    }
                });
                do_cao = do_cao.max(noi_dung.response.rect.height());
            });
        });
        self.card_height = do_cao;

        if let Some(item) = chon {
            self.form.target = item.target;
            // Mật khẩu đi kèm máy: nối lại máy cũ mà vẫn phải hỏi lại mật khẩu
            // thì danh sách này chẳng tiết kiệm được gì.
            if !item.password.is_empty() {
                self.form.password = item.password;
            }
            noi = true;
        }
        if noi {
            self.start(Role::Viewer);
        }
        if chia_se {
            self.start(Role::Host);
        }

        ui.add_space(8.0);

        // Tailscale gấp lại được, nhưng mở sẵn khi đã thấy máy khác: lúc đó
        // danh sách máy chính là cách nối nhanh nhất, giấu đi thì người dùng
        // quay lại gõ tay địa chỉ.
        self.tailscale.tick(std::time::Duration::from_secs(5));
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs(1));
        let tom_tat = self.tailscale_summary();
        let dang_xem = self.tailscale_needs_action() || self.tailscale_co_may_khac();
        // egui chỉ đọc `default_open` đúng lượt vẽ đầu tiên thấy header, mà
        // lượt ấy luồng nền còn chưa hỏi xong Tailscale nên danh sách luôn
        // rỗng và khung nằm đóng mãi. Ép mở đúng một lượt khi đã có thứ đáng
        // xem, rồi trả quyền đóng/mở lại cho người dùng.
        let ep_mo = (dang_xem && !self.da_mo_tailnet).then(|| {
            self.da_mo_tailnet = true;
            true
        });
        egui::CollapsingHeader::new(format!("Tailscale — {tom_tat}"))
            .id_salt("tailscale")
            .default_open(dang_xem)
            .open(ep_mo)
            .show(ui, |ui| self.draw_tailscale(ui));

        if let Some(error) = &self.error {
            ui.add_space(8.0);
            ui.colored_label(ui::BAD, error);
        }
    }

    /// Một dòng đủ để biết Tailscale đang ra sao mà không phải mở khung ra.
    fn tailscale_summary(&self) -> String {
        use crate::tailscale::State;
        let status = self.tailscale.snapshot();
        match &status.state {
            None => "đang kiểm tra…".to_string(),
            Some(State::NotInstalled) => "chưa cài".to_string(),
            Some(State::Stopped) => "đã cài, chưa bật".to_string(),
            Some(State::NeedsLogin) => "chưa đăng nhập".to_string(),
            Some(State::Broken(_)) => "có lỗi".to_string(),
            Some(State::Running) => match &status.self_ip {
                Some(ip) => format!("đang chạy, máy này {ip}"),
                None => "đang chạy".to_string(),
            },
        }
    }

    /// Còn việc phải bấm thì mở sẵn khung ra; xong xuôi rồi thì gấp lại.
    /// Tailnet có máy nào khác ngoài máy này không.
    fn tailscale_co_may_khac(&self) -> bool {
        !self.tailscale.snapshot().peers.is_empty()
    }

    fn tailscale_needs_action(&self) -> bool {
        use crate::tailscale::State;
        matches!(
            self.tailscale.snapshot().state,
            Some(State::Stopped) | Some(State::NeedsLogin) | Some(State::Broken(_))
        )
    }

    fn draw_settings(&mut self, root: &mut egui::Ui) {
        egui::Panel::top("thanh-cai-dat").show(root, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Xong").clicked() {
                    self.form.save();
                    self.screen = Screen::Start;
                }
                ui.separator();
                ui.heading("Cài đặt");
                ui.label(
                    egui::RichText::new("tự lưu, áp dụng cho phiên sau")
                        .small()
                        .color(egui::Color32::GRAY),
                );
            });
        });

        egui::CentralPanel::default().show(root, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                centered_column(ui, CONTENT_WIDTH, |ui| {
                    ui.add_space(6.0);

                    ui.group(|ui| {
                        // Không ép bề ngang thì mỗi khung co theo nội dung
                        // của nó, năm khung xếp dọc thành hình răng cưa.
                        ui.set_width(ui.available_width());
                        ui.heading("Hình ảnh");
                        egui::Grid::new("cd-hinh")
                            .num_columns(2)
                            .spacing([12.0, 8.0])
                            .show(ui, |ui| {
                                ui.label("Số hình mỗi giây");
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::DragValue::new(&mut self.form.fps).range(15..=240),
                                    );
                                    ui.label(
                                        egui::RichText::new(
                                            "cao hơn thì mượt hơn nhưng tốn CPU máy kia",
                                        )
                                        .small(),
                                    );
                                });
                                ui.end_row();

                                ui.label("Trần băng thông");
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::DragValue::new(&mut self.form.bitrate)
                                            .range(1_000..=200_000)
                                            .speed(500),
                                    );
                                    ui.label(
                                        egui::RichText::new(
                                            "kbps — máy kia tự dò mức thật trong khoảng này",
                                        )
                                        .small(),
                                    );
                                });
                                ui.end_row();
                            });
                    });

                    ui.add_space(10.0);
                    ui.group(|ui| {
                        // Không ép bề ngang thì mỗi khung co theo nội dung
                        // của nó, năm khung xếp dọc thành hình răng cưa.
                        ui.set_width(ui.available_width());
                        ui.heading("Khi đang xem máy khác");
                        ui.checkbox(
                            &mut self.form.prefs.local_cursor,
                            "Vẽ con trỏ ngay tại chỗ chuột đang ở",
                        )
                        .on_hover_text(
                            "Con trỏ trong hình luôn tới muộn đúng bằng độ trễ mạng. Vẽ thêm một \
                         con trỏ ngay tại chỗ chuột thật thì thao tác hết cảm giác giật, dù \
                         mạng không nhanh thêm chút nào.",
                        );
                        ui.checkbox(
                            &mut self.form.prefs.start_fullscreen,
                            "Vào phiên là bung toàn màn hình (F11)",
                        );
                        ui.checkbox(
                            &mut self.form.prefs.hide_chrome,
                            "Vào phiên là ẩn thanh công cụ (F12)",
                        )
                        .on_hover_text("Đưa chuột lên sát mép trên là thanh hiện lại.");
                        ui.checkbox(&mut self.form.prefs.show_hud, "Hiện bảng thông số (F10)");
                        ui.checkbox(
                            &mut self.form.prefs.auto_reconnect,
                            "Mất kết nối thì tự nối lại",
                        )
                        .on_hover_text(
                            "Máy kia rớt mạng, ngủ màn hình hay mất điện: phiên chờ và tự nối \
                         lại khi máy kia quay lại, thay vì bắt bấm lại từ đầu.",
                        );
                    });

                    ui.add_space(10.0);
                    ui.group(|ui| {
                        // Không ép bề ngang thì mỗi khung co theo nội dung
                        // của nó, năm khung xếp dọc thành hình răng cưa.
                        ui.set_width(ui.available_width());
                        ui.heading("Tệp và clipboard");
                        ui.horizontal(|ui| {
                            ui.label("Thư mục nhận tệp");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.form.downloads)
                                    .desired_width(300.0),
                            );
                            let button = egui::Button::new("Chọn…");
                            if ui.add_enabled(!self.picker.busy(), button).clicked() {
                                self.picker.open_folder();
                            }
                        });
                        ui.checkbox(
                            &mut self.form.prefs.auto_accept_files,
                            "Tự nhận tệp, không hỏi",
                        )
                        .on_hover_text(
                            "Chỉ bật khi tin người ở đầu kia — tệp sẽ tự ghi vào thư mục trên.",
                        );
                        ui.checkbox(
                            &mut self.form.prefs.clipboard_sync,
                            "Đồng bộ clipboard hai chiều",
                        );
                    });

                    ui.add_space(10.0);
                    ui.group(|ui| {
                        // Không ép bề ngang thì mỗi khung co theo nội dung
                        // của nó, năm khung xếp dọc thành hình răng cưa.
                        ui.set_width(ui.available_width());
                        ui.heading("Mạng");
                        egui::Grid::new("cd-mang")
                            .num_columns(2)
                            .spacing([12.0, 8.0])
                            .show(ui, |ui| {
                                ui.label("Cổng chờ khi chia sẻ");
                                ui.add(
                                    egui::DragValue::new(&mut self.form.port).range(1024..=65535),
                                );
                                ui.end_row();

                                ui.label("Rendezvous server");
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.form.rendezvous)
                                        .desired_width(300.0)
                                        .hint_text(
                                            "bỏ trống nếu nối thẳng — hoặc máy_chủ:7000#vân_tay",
                                        ),
                                );
                                ui.end_row();
                            });
                    });

                    ui.add_space(10.0);
                    ui.group(|ui| {
                        // Không ép bề ngang thì mỗi khung co theo nội dung
                        // của nó, năm khung xếp dọc thành hình răng cưa.
                        ui.set_width(ui.available_width());
                        ui.heading("Hệ thống");
                        let mut on = self.autostart;
                        if ui
                            .checkbox(&mut on, "Tự chạy ngầm ở chế độ chia sẻ khi đăng nhập")
                            .on_hover_text(
                                "Máy mất điện rồi bật lại: đăng nhập xong là phần mềm tự lên, \
                             cửa sổ thu nhỏ sẵn, tự mở chia sẻ với đúng mật khẩu đang lưu. \
                             Mất mạng rồi có lại thì tự chờ tiếp, không cần ai bấm gì.",
                            )
                            .changed()
                        {
                            match crate::os::set_autostart(on) {
                                Ok(()) => {
                                    self.autostart = on;
                                    // Lần tự khởi động sau sẽ dùng mật khẩu và
                                    // cổng đang có trên màn hình; ghi ngay
                                    // xuống đĩa chứ đừng đợi bấm "Xong".
                                    self.form.save();
                                }
                                Err(err) => self.error = Some(err),
                            }
                        }
                        if let Some(path) = settings_path() {
                            ui.label(
                                egui::RichText::new(format!("thiết lập lưu ở {}", path.display()))
                                    .small()
                                    .color(egui::Color32::GRAY),
                            );
                        }
                    });

                    if let Some(error) = &self.error {
                        ui.add_space(10.0);
                        ui.colored_label(ui::BAD, error);
                    }
                    ui.add_space(10.0);
                });
            });
        });
    }
}

/// Số khung chờ trước khi chụp.
///
/// Khung đầu chưa có font hệ thống và chưa đo xong bề rộng chữ, chụp ngay thì
/// ra một cửa sổ trắng.
const SCREENSHOT_DELAY: u32 = 12;

impl RdApp {
    /// Đếm khung, chụp, ghi tệp rồi đóng cửa sổ — chỉ chạy khi có `--screenshot`.
    fn tick_screenshot(&mut self, ctx: &egui::Context) {
        let Some((path, frames)) = &mut self.screenshot else {
            return;
        };
        *frames += 1;
        ctx.request_repaint();
        if *frames == SCREENSHOT_DELAY {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
            return;
        }
        if *frames <= SCREENSHOT_DELAY {
            return;
        }
        let image = ctx.input(|i| {
            i.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        let Some(image) = image else {
            // Quá lâu không thấy ảnh về thì thôi, đừng treo mãi.
            if *frames > SCREENSHOT_DELAY * 4 {
                tracing::error!("không chụp được cửa sổ");
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            return;
        };
        // PPM vì nó không cần thư viện nén nào: 15 dòng ở đây đổi lấy việc
        // không kéo thêm một crate ảnh vào bản phát hành.
        let mut out = format!("P6\n{} {}\n255\n", image.width(), image.height()).into_bytes();
        for pixel in image.pixels.iter() {
            out.extend_from_slice(&[pixel.r(), pixel.g(), pixel.b()]);
        }
        match std::fs::write(&*path, out) {
            Ok(()) => {
                tracing::info!(path = %path.display(), w = image.width(), h = image.height(), "đã chụp cửa sổ")
            }
            Err(err) => tracing::error!(%err, "không ghi được ảnh chụp"),
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

impl eframe::App for RdApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.minimize {
            self.minimize = false;
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        let leave = match &mut self.screen {
            Screen::Start => {
                // Trạng thái Tailscale đổi sau lưng ta (người dùng đăng nhập
                // xong ở trình duyệt, máy bên kia vừa bật). Luồng nền gõ chuông
                // khi hỏi xong; nhịp chậm này chỉ để `tick` còn tới lượt đi hỏi.
                self.tailscale.attach(ui.ctx());
                self.prober.attach(ui.ctx());
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_secs(1));
                self.draw_start(ui);
                false
            }
            Screen::Settings => {
                // Hộp thoại chọn thư mục nằm ở luồng khác nên kết quả về lúc
                // nào không biết trước; hỏi mỗi lượt vẽ.
                if let Some(dir) = self.picker.take().into_iter().next() {
                    self.form.downloads = dir.display().to_string();
                }
                self.draw_settings(ui);
                false
            }
            Screen::Session(state) => matches!(state.update(ui), Outcome::Leave),
            Screen::Local(state) => state.update(ui),
        };
        self.tick_screenshot(ui.ctx());
        if leave {
            // Tên máy kia chỉ biết được sau khi nối; ghi lại lúc rời phiên để
            // lần sau danh sách gần đây hiện tên chứ không phải dãy số.
            if let Screen::Session(state) = &self.screen {
                if let Some(name) = state.peer_display_name() {
                    let name = name.to_string();
                    let target = self.form.target.trim().to_string();
                    if let Some(item) = self
                        .form
                        .recents
                        .iter_mut()
                        .find(|item| item.target == target)
                    {
                        item.name = name;
                        self.form.save();
                    }
                }
            }
            // Thả `SessionState` ở đây là dừng luồng mạng và đóng kết nối; màn
            // hình đầu phải sạch để bấm nối lại được ngay.
            self.screen = Screen::Start;
            // Phiên có thể đã bung toàn màn hình; về màn hình đầu mà cửa sổ
            // vẫn phủ kín thì không còn nút nào để thu nó lại.
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
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
                if ui.button("Thoát").clicked() {
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
/// Tách `địa_chỉ#vân_tay` thành hai phần. Ghim vân tay là cách duy nhất biết
/// mình đang nói chuyện với đúng rendezvous server chứ không phải bản giả —
/// không ghim thì vẫn chạy, chỉ là tin vào bất cứ server nào trả lời.
fn split_pin(text: &str) -> Result<(&str, Option<[u8; 32]>), String> {
    let Some((addr, pin)) = text.split_once('#') else {
        return Ok((text, None));
    };
    let fingerprint = rd_transport::parse_fingerprint(pin)
        .ok_or_else(|| format!("vân tay sau dấu # phải là 64 ký tự hex, nhận được: {pin}"))?;
    Ok((addr.trim(), Some(fingerprint)))
}

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

    /// `--background` phải kéo theo cả chế độ chia sẻ.
    ///
    /// Thiếu vế đó thì lần tự khởi động sau khi mất điện chỉ mở ra một cửa sổ
    /// thu nhỏ nằm im, không ai ở xa vào được.
    #[test]
    fn chay_ngam_thi_chia_se_luon() {
        let args = Args::parse(["--background".to_string()].into_iter()).expect("đọc tham số");
        assert!(args.background);
        assert!(args.host);
    }

    /// Mỗi máy đặt cổng riêng được, nên bấm vào một máy trong danh sách phải
    /// lấy lại cổng của lần nối trước tới đúng máy ấy.
    #[test]
    fn lay_lai_cong_da_dung_voi_tung_may() {
        let recents = vec![
            RecentPeer {
                target: "100.64.1.9:47900".into(),
                name: String::new(),
                password: String::new(),
            },
            RecentPeer {
                target: "100.64.1.10:47823".into(),
                name: String::new(),
                password: String::new(),
            },
        ];
        assert_eq!(cong_da_dung(&recents, "100.64.1.10"), Some(47823));
        // Máy chưa từng nối thì chưa biết gì, để phía gọi tự chọn mặc định.
        assert_eq!(cong_da_dung(&recents, "100.64.1.11"), None);
        // Mã rendezvous cũ nằm lẫn trong danh sách không được làm hỏng việc đọc.
        let ma = vec![RecentPeer {
            target: "622236572".into(),
            name: String::new(),
            password: String::new(),
        }];
        assert_eq!(cong_da_dung(&ma, "622236572"), None);
    }

    /// Cờ `--trust-tailnet` là đường duy nhất bật được tính năng này trên máy
    /// chạy nền, nơi không ai mở giao diện ra tick.
    #[test]
    fn co_dong_lenh_bat_duoc_tin_tailnet() {
        let args = Args::parse(["--trust-tailnet".to_string()].into_iter()).expect("đọc tham số");
        assert!(args.trust_tailnet);
        assert!(!Args::default().trust_tailnet);
    }

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

        assert!(
            !du,
            "egui đã có sẵn chữ Việt, không cần mượn font hệ thống nữa"
        );
    }
}
