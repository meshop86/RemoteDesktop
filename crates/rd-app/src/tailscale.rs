//! Dùng Tailscale đang cài trên máy làm đường đi giữa hai bên.
//!
//! Tailscale dựng một mạng riêng ảo trên nền WireGuard: mỗi máy đăng nhập vào
//! cùng một tài khoản sẽ có một địa chỉ `100.x.y.z` **cố định**, thấy nhau ở mọi
//! mạng, kể cả sau NAT nhiều lớp hay sau CGNAT của nhà mạng di động. Với ta,
//! điều đó thay đúng vai trò của rendezvous server: không cần VPS, không cần mở
//! cổng trên router, chỉ cần gõ địa chỉ `100.x.y.z:47823`.
//!
//! Đổi lại, cả hai máy phải cùng một tailnet — nên đây là đường cho máy của
//! chính mình, còn muốn đọc mã 9 số cho người lạ thì vẫn phải có rendezvous.
//!
//! Cách nói chuyện: gọi chương trình `tailscale` có sẵn rồi đọc JSON nó in ra.
//! Không nhúng thư viện Tailscale vào (nó viết bằng Go), cũng không tự mở
//! socket điều khiển của `tailscaled` — socket đó không có giao kèo ổn định,
//! còn `tailscale status --json` thì có.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;

/// Tình trạng của Tailscale trên máy này.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Không tìm thấy chương trình `tailscale` ở đâu cả.
    NotInstalled,
    /// Có cài nhưng dịch vụ nền không chạy, hoặc người dùng đã bấm tắt.
    Stopped,
    /// Chạy rồi nhưng chưa đăng nhập tài khoản nào.
    NeedsLogin,
    /// Đã đăng nhập và đang trong tailnet.
    Running,
    /// Gọi được chương trình nhưng nó trả về thứ ta không hiểu.
    Broken(String),
}

/// Một máy khác trong cùng tailnet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub name: String,
    pub ip: String,
    pub os: String,
    pub online: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Status {
    pub state: Option<State>,
    /// Địa chỉ của máy này trong tailnet — chính là thứ đọc cho người kia gõ.
    pub self_ip: Option<String>,
    pub self_name: String,
    pub peers: Vec<Peer>,
    /// Đường dẫn đăng nhập, khi Tailscale đang chờ người dùng mở trình duyệt.
    pub auth_url: Option<String>,
}

impl Status {
    pub fn running(&self) -> bool {
        self.state == Some(State::Running)
    }
}

// ───────────────────────── đọc từ chương trình ─────────────────────────

/// Hình dạng của `tailscale status --json`, chỉ lấy những trường ta cần.
#[derive(Deserialize)]
struct RawStatus {
    #[serde(rename = "BackendState")]
    backend_state: Option<String>,
    #[serde(rename = "AuthURL")]
    auth_url: Option<String>,
    #[serde(rename = "Self")]
    self_node: Option<RawNode>,
    #[serde(rename = "Peer")]
    peer: Option<std::collections::BTreeMap<String, RawNode>>,
}

#[derive(Deserialize)]
struct RawNode {
    #[serde(rename = "HostName")]
    host_name: Option<String>,
    #[serde(rename = "DNSName")]
    dns_name: Option<String>,
    #[serde(rename = "TailscaleIPs")]
    ips: Option<Vec<String>>,
    #[serde(rename = "OS")]
    os: Option<String>,
    #[serde(rename = "Online")]
    online: Option<bool>,
}

impl RawNode {
    /// Tên ngắn để hiện lên giao diện.
    ///
    /// `DNSName` là tên đầy đủ kiểu `may-cua-toi.tail1234.ts.net.` — cắt lấy
    /// nhãn đầu, vì phần đuôi giống hệt nhau ở mọi máy nên chỉ tổ chật chỗ.
    fn short_name(&self) -> String {
        if let Some(dns) = self.dns_name.as_deref()
            && let Some(first) = dns.split('.').next()
            && !first.is_empty()
        {
            return first.to_owned();
        }
        self.host_name.clone().unwrap_or_else(|| "?".into())
    }

    /// Địa chỉ IPv4 trong tailnet. Bỏ qua IPv6: ô địa chỉ của ta nhận
    /// `host:cổng`, mà IPv6 ở dạng đó phải bọc ngoặc vuông — thêm một cách gõ
    /// sai cho người dùng, đổi lại chẳng được gì vì máy nào cũng có IPv4.
    fn ipv4(&self) -> Option<String> {
        self.ips
            .as_ref()?
            .iter()
            .find(|ip| ip.parse::<std::net::Ipv4Addr>().is_ok())
            .cloned()
    }
}

/// Những chỗ Tailscale hay nằm, xếp theo thứ tự thử.
fn candidates() -> Vec<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let mut paths = vec![
            PathBuf::from(r"C:\Program Files\Tailscale\tailscale.exe"),
            PathBuf::from(r"C:\Program Files (x86)\Tailscale\tailscale.exe"),
        ];
        // Bản cài cho một người dùng nằm trong thư mục riêng của họ.
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            paths.push(PathBuf::from(local).join(r"Tailscale\tailscale.exe"));
        }
        paths
    }
    #[cfg(not(target_os = "windows"))]
    {
        vec![
            // Bản Mac App Store gói chương trình dòng lệnh trong bụng app.
            PathBuf::from("/Applications/Tailscale.app/Contents/MacOS/Tailscale"),
            PathBuf::from("/usr/local/bin/tailscale"),
            PathBuf::from("/opt/homebrew/bin/tailscale"),
            PathBuf::from("/usr/bin/tailscale"),
        ]
    }
}

/// Tên file của chương trình, để dò trong PATH.
const EXE: &str = if cfg!(target_os = "windows") {
    "tailscale.exe"
} else {
    "tailscale"
};

/// Tìm chương trình `tailscale`, hoặc `None` nếu máy chưa cài.
///
/// Phải trả về đường dẫn *có thật*, không được đưa ra cái tên trần rồi phó mặc
/// cho hệ điều hành tra: có phân biệt được "chưa cài" với "cài rồi mà chưa bật"
/// thì mới chỉ đúng việc cho người dùng làm tiếp.
pub fn binary() -> Option<PathBuf> {
    if let Some(path) = candidates().into_iter().find(|path| path.exists()) {
        return Some(path);
    }
    // Cài kiểu khác (gói của bên thứ ba, hay tự bỏ vào chỗ riêng) thì vẫn còn
    // cửa: người ta hay thêm nó vào PATH.
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(EXE))
        .find(|path| path.exists())
}

/// Chạy `tailscale` với tham số cho trước, trả về stdout.
fn run(args: &[&str]) -> Result<String, String> {
    let binary = binary().ok_or_else(|| "chưa cài Tailscale".to_string())?;
    let mut command = Command::new(&binary);
    command.args(args);
    hide_console(&mut command);

    let output = command
        .output()
        .map_err(|err| format!("không gọi được {}: {err}", binary.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if stderr.is_empty() {
            format!("tailscale {} thoát với mã lỗi", args.join(" "))
        } else {
            stderr
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Đừng để cửa sổ console đen nhấp nháy mỗi lần hỏi trạng thái.
#[cfg(target_os = "windows")]
fn hide_console(command: &mut Command) {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
fn hide_console(_command: &mut Command) {}

/// Hỏi Tailscale xem tình hình thế nào. Lời gọi này **chặn** — gọi trên luồng
/// nền, đừng gọi trong lúc vẽ giao diện.
pub fn status() -> Status {
    let text = match run(&["status", "--json"]) {
        Ok(text) => text,
        Err(err) => {
            let state = if binary().is_none() {
                State::NotInstalled
            } else {
                // `tailscale status` cũng thoát với mã lỗi khi dịch vụ nền chưa
                // chạy — với người dùng thì hai chuyện đó là một: chưa dùng được.
                State::Stopped
            };
            tracing::debug!(%err, ?state, "không đọc được trạng thái Tailscale");
            return Status {
                state: Some(state),
                ..Status::default()
            };
        }
    };

    let raw: RawStatus = match serde_json::from_str(&text) {
        Ok(raw) => raw,
        Err(err) => {
            return Status {
                state: Some(State::Broken(format!("không hiểu JSON của tailscale: {err}"))),
                ..Status::default()
            };
        }
    };

    let state = match raw.backend_state.as_deref() {
        Some("Running") => State::Running,
        Some("NeedsLogin") | Some("NeedsMachineAuth") => State::NeedsLogin,
        Some("Stopped") | Some("NoState") | Some("Starting") => State::Stopped,
        Some(other) => State::Broken(format!("trạng thái lạ: {other}")),
        None => State::Broken("tailscale không cho biết trạng thái".into()),
    };

    let mut peers: Vec<Peer> = raw
        .peer
        .unwrap_or_default()
        .into_values()
        .filter_map(|node| {
            Some(Peer {
                name: node.short_name(),
                ip: node.ipv4()?,
                os: node.os.clone().unwrap_or_default(),
                online: node.online.unwrap_or(false),
            })
        })
        .collect();
    sort_peers(&mut peers);

    Status {
        state: Some(state),
        self_ip: raw.self_node.as_ref().and_then(RawNode::ipv4),
        self_name: raw
            .self_node
            .as_ref()
            .map(RawNode::short_name)
            .unwrap_or_default(),
        peers,
        auth_url: raw.auth_url.filter(|url| !url.is_empty()),
    }
}

/// Máy đang bật lên trước, rồi tới thứ tự tên: danh sách này để bấm chọn, mà
/// máy đang tắt thì bấm cũng không nối được.
fn sort_peers(peers: &mut [Peer]) {
    peers.sort_by(|a, b| b.online.cmp(&a.online).then_with(|| a.name.cmp(&b.name)));
}

/// Bắt đầu đăng nhập. Trả về địa chỉ web người dùng phải mở.
///
/// `tailscale login` in ra đường dẫn rồi **ngồi chờ** tới khi người dùng xong
/// việc trên trình duyệt, nên không đọc được stdout tới cùng mà vẫn kịp hiện
/// đường dẫn lên. Cách đi vòng: hỏi trạng thái trước — khi đang chờ đăng nhập,
/// chính `status --json` đã mang sẵn `AuthURL`.
pub fn login() -> Result<String, String> {
    if let Some(url) = status().auth_url {
        open_url(&url);
        return Ok(url);
    }

    // Chưa có sẵn thì phải đánh thức: `tailscale login` tự mở trình duyệt.
    let binary = binary().ok_or_else(|| "chưa cài Tailscale".to_string())?;
    let mut command = Command::new(&binary);
    command.arg("login");
    hide_console(&mut command);
    command
        .spawn()
        .map_err(|err| format!("không chạy được `tailscale login`: {err}"))?;

    // Cho nó vài giây để đăng ký đường dẫn rồi hỏi lại.
    for _ in 0..10 {
        std::thread::sleep(Duration::from_millis(500));
        let status = status();
        if let Some(url) = status.auth_url {
            open_url(&url);
            return Ok(url);
        }
        if status.running() {
            return Ok(String::new());
        }
    }
    Err("Tailscale chưa đưa ra đường dẫn đăng nhập; mở ứng dụng Tailscale để đăng nhập".into())
}

/// Mở một địa chỉ web bằng trình duyệt mặc định.
fn open_url(url: &str) {
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("cmd");
        // Tham số rỗng sau `start` là *tiêu đề cửa sổ*: thiếu nó thì `start` coi
        // địa chỉ web là tiêu đề và không mở gì cả.
        command.args(["/C", "start", "", url]);
        command
    };
    #[cfg(not(target_os = "windows"))]
    let mut command = {
        let mut command = Command::new("open");
        command.arg(url);
        command
    };
    hide_console(&mut command);
    if let Err(err) = command.spawn() {
        tracing::warn!(%err, url, "không mở được trình duyệt");
    }
}

/// Vài dòng mô tả cho `--probe`.
pub fn describe() -> String {
    let Some(binary) = binary() else {
        return "chưa cài (có thì dùng được địa chỉ 100.x.y.z thay cho rendezvous server)".into();
    };
    let status = status();
    let mut out = format!("chương trình: {}\n", binary.display());
    match status.state.clone().unwrap_or(State::Stopped) {
        State::Running => {
            out.push_str(&format!(
                "đang chạy — máy này là {} ({})\n",
                status.self_name,
                status.self_ip.clone().unwrap_or_else(|| "?".into())
            ));
            out.push_str(&format!("thấy {} máy khác trong tailnet", status.peers.len()));
        }
        State::NeedsLogin => out.push_str("chưa đăng nhập"),
        State::Stopped => out.push_str("dịch vụ nền chưa chạy"),
        State::NotInstalled => out.push_str("chưa cài"),
        State::Broken(reason) => out.push_str(&reason),
    }
    out
}

// ─────────────────────────── phần cho giao diện ───────────────────────────

/// Trạng thái Tailscale được làm mới trên luồng nền.
///
/// Hỏi Tailscale mất cỡ trăm mili giây — làm việc đó trong lúc vẽ là giao diện
/// tụt xuống dưới 10 fps. Nên: luồng nền đi hỏi, giao diện chỉ đọc kết quả gần
/// nhất và vẽ ngay.
pub struct Watcher {
    status: Arc<Mutex<Status>>,
    busy: Arc<AtomicBool>,
    message: Arc<Mutex<Option<String>>>,
    /// Lần cuối bắt đầu đi hỏi, để `tick` biết đã tới lượt chưa.
    last_poll: Mutex<Instant>,
}

impl Default for Watcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Watcher {
    pub fn new() -> Self {
        let watcher = Self {
            status: Arc::new(Mutex::new(Status::default())),
            busy: Arc::new(AtomicBool::new(false)),
            message: Arc::new(Mutex::new(None)),
            last_poll: Mutex::new(Instant::now()),
        };
        watcher.refresh();
        watcher
    }

    /// Hỏi lại nếu lần hỏi trước đã quá `every`.
    ///
    /// Gọi mỗi khung hình. Cần nó vì trạng thái đổi sau lưng ta: người dùng đăng
    /// nhập xong trên trình duyệt, hay máy bên kia vừa bật lên.
    pub fn tick(&self, every: Duration) {
        let mut last = self.last_poll.lock().expect("khoá nhịp hỏi");
        if last.elapsed() < every {
            return;
        }
        *last = Instant::now();
        drop(last);
        self.refresh();
    }

    /// Bản chụp gần nhất. `state` là `None` khi lần hỏi đầu tiên chưa xong.
    pub fn snapshot(&self) -> Status {
        self.status.lock().expect("khoá trạng thái tailscale").clone()
    }

    pub fn busy(&self) -> bool {
        self.busy.load(Ordering::Relaxed)
    }

    /// Lời nhắn cuối cùng dành cho người dùng (đường dẫn đăng nhập, hoặc lỗi).
    pub fn message(&self) -> Option<String> {
        self.message.lock().expect("khoá lời nhắn").clone()
    }

    /// Chạy `job` trên luồng nền, rồi hỏi lại trạng thái. `job` trả về lời nhắn
    /// cho người dùng, hoặc `None` để xoá lời nhắn cũ.
    ///
    /// Mọi việc với Tailscale đều đi qua đây, nên chỉ có một luồng chạy tại một
    /// thời điểm: bấm liên tục không đẻ ra một đàn tiến trình con.
    fn spawn<F>(&self, name: &str, job: F)
    where
        F: FnOnce() -> Option<String> + Send + 'static,
    {
        if self.busy.swap(true, Ordering::Relaxed) {
            return;
        }
        let slot = self.status.clone();
        let busy = self.busy.clone();
        let message = self.message.clone();
        let spawned = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                let note = job();
                *message.lock().expect("khoá lời nhắn") = note;
                *slot.lock().expect("khoá trạng thái tailscale") = status();
                busy.store(false, Ordering::Relaxed);
            });
        // Không tạo được luồng thì đừng kẹt cờ bận vĩnh viễn.
        if let Err(err) = spawned {
            tracing::warn!(%err, name, "không tạo được luồng cho tailscale");
            self.busy.store(false, Ordering::Relaxed);
        }
    }

    pub fn refresh(&self) {
        // Không đụng tới lời nhắn: lần hỏi định kỳ mà xoá mất đường dẫn đăng
        // nhập đang hiện thì người dùng chưa kịp đọc đã mất.
        let keep = self.message();
        self.spawn("rd-tailscale", move || keep);
    }

    /// Mở luồng đăng nhập rồi tự làm mới khi xong.
    pub fn login(&self) {
        self.spawn("rd-tailscale-login", || {
            Some(match login() {
                Ok(url) if url.is_empty() => "đã đăng nhập".to_string(),
                Ok(url) => format!("mở trang này để đăng nhập: {url}"),
                Err(err) => err,
            })
        });
    }

    /// Bật kết nối (`tailscale up`) cho máy đã đăng nhập nhưng đang tắt.
    pub fn up(&self) {
        self.spawn("rd-tailscale-up", || {
            run(&["up"]).err().map(|err| format!("không bật được: {err}"))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(json: &str) -> RawNode {
        serde_json::from_str(json).expect("đọc được node")
    }

    #[test]
    fn ten_ngan_cat_duoi_magicdns() {
        let node = node(r#"{"HostName":"admin-pc","DNSName":"may-ban.tail1234.ts.net."}"#);
        assert_eq!(node.short_name(), "may-ban");
    }

    #[test]
    fn khong_co_dnsname_thi_lay_hostname() {
        let node = node(r#"{"HostName":"admin-pc"}"#);
        assert_eq!(node.short_name(), "admin-pc");
    }

    #[test]
    fn chi_lay_dia_chi_ipv4() {
        let node = node(r#"{"TailscaleIPs":["fd7a:115c:a1e0::1","100.64.1.9"]}"#);
        assert_eq!(node.ipv4().as_deref(), Some("100.64.1.9"));
    }

    fn peer(name: &str, online: bool) -> Peer {
        Peer {
            name: name.into(),
            ip: "100.0.0.1".into(),
            os: String::new(),
            online,
        }
    }

    #[test]
    fn may_dang_bat_dung_truoc_trong_danh_sach() {
        let mut peers = [peer("b", false), peer("c", true), peer("a", true)];
        sort_peers(&mut peers);
        let names: Vec<&str> = peers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["a", "c", "b"]);
    }
}
