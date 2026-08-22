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
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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
    /// "3 giờ trước" — chỉ có nghĩa với máy đang tắt. Rỗng nếu không rõ.
    pub last_seen: String,
}

#[derive(Debug, Clone, Default)]
pub struct Status {
    pub state: Option<State>,
    /// Địa chỉ của máy này trong tailnet — chính là thứ đọc cho người kia gõ.
    pub self_ip: Option<String>,
    pub self_name: String,
    /// Chủ tài khoản của máy này. Dùng để biết máy gọi tới có cùng tài khoản
    /// hay chỉ là khách được chia sẻ vào tailnet.
    pub self_user_id: Option<u64>,
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
    #[serde(rename = "UserID")]
    user_id: Option<u64>,
    /// Rỗng khi máy chưa từng nối; dùng để nói "tắt từ bao giờ".
    #[serde(rename = "LastSeen")]
    last_seen: Option<String>,
    /// Địa chỉ thật đang gửi thẳng tới máy kia. Rỗng nghĩa là đang đi vòng.
    #[serde(rename = "CurAddr")]
    cur_addr: Option<String>,
    /// Mã vùng DERP đang dùng để đi vòng, ví dụ `sin`.
    #[serde(rename = "Relay")]
    relay: Option<String>,
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
    nhu_dong_lenh(&mut command);

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

/// Nói cho Tailscale biết đây là một lời gọi dòng lệnh.
///
/// Bản macOS xem `TERM`, `SHLVL` và `TERM_PROGRAM` để đoán nó đang được gõ
/// trong cửa sổ dòng lệnh hay được giao diện gọi. Ứng dụng mở từ Finder không
/// có biến nào trong ba biến ấy, nên nó tưởng phải bật giao diện Tailscale lên,
/// rồi bỏ ra câu "The Tailscale GUI failed to start" — kèm mã thoát 0 — thay vì
/// trả lời. Đặt `TERM` là hết: `dumb` đúng nghĩa "có đọc chữ nhưng đừng tô màu".
#[cfg(target_os = "macos")]
fn nhu_dong_lenh(command: &mut Command) {
    let co_san = std::env::var_os("TERM").is_some_and(|term| !term.is_empty());
    if !co_san {
        command.env("TERM", "dumb");
    }
}

#[cfg(not(target_os = "macos"))]
fn nhu_dong_lenh(_command: &mut Command) {}

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
/// Hỏi Tailscale xem máy này đang ở đâu trong tailnet.
///
/// Hỏi hụt một lần thì hỏi lại: bản Tailscale cài từ App Store thỉnh thoảng
/// chưa kịp trả lời, lần ngay sau đó lại bình thường.
pub fn status() -> Status {
    match hoi_trang_thai() {
        Ok(status) => status,
        Err(loi) => {
            std::thread::sleep(Duration::from_millis(400));
            hoi_trang_thai().unwrap_or(Status {
                state: Some(State::Broken(loi)),
                ..Status::default()
            })
        }
    }
}

fn hoi_trang_thai() -> Result<Status, String> {
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
            return Ok(Status {
                state: Some(state),
                ..Status::default()
            });
        }
    };

    let raw = doc_trang_thai(&text)?;

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
                last_seen: node
                    .last_seen
                    .as_deref()
                    .and_then(|text| khoang_cach(text, now_unix()))
                    .unwrap_or_default(),
            })
        })
        .collect();
    sort_peers(&mut peers);

    Ok(Status {
        state: Some(state),
        self_ip: raw.self_node.as_ref().and_then(RawNode::ipv4),
        self_user_id: raw.self_node.as_ref().and_then(|node| node.user_id),
        self_name: raw
            .self_node
            .as_ref()
            .map(RawNode::short_name)
            .unwrap_or_default(),
        peers,
        auth_url: raw.auth_url.filter(|url| !url.is_empty()),
    })
}

/// Tách JSON ra khỏi những dòng chữ mà `tailscale` đôi khi in kèm.
///
/// Bản cài từ App Store có lúc in một câu than bằng tiếng Anh ra stdout rồi
/// vẫn thoát với mã 0 — chẳng hạn "The Tailscale GUI failed to start" khi ứng
/// dụng nền chưa sẵn sàng. Ném nguyên văn ấy cho serde thì người dùng nhận
/// được một lỗi cú pháp JSON khó hiểu, trong khi việc phải làm là mở ứng dụng
/// Tailscale lên.
fn doc_trang_thai(text: &str) -> Result<RawStatus, String> {
    match text.find('{') {
        Some(dau) => serde_json::from_str(&text[dau..])
            .map_err(|err| format!("không hiểu JSON của tailscale: {err}")),
        None => Err(loi_nguoi_doc(text)),
    }
}

/// Đổi câu than tiếng Anh của Tailscale thành một câu nói rõ phải làm gì.
fn loi_nguoi_doc(text: &str) -> String {
    let dong = text
        .lines()
        .find(|dong| !dong.trim().is_empty())
        .unwrap_or_default()
        .trim();
    let mo_app = "mở ứng dụng Tailscale rồi bấm Làm mới";
    if dong.is_empty() {
        format!("Tailscale không trả lời — {mo_app}")
    } else {
        format!("Tailscale chưa trả lời được ({dong}) — {mo_app}")
    }
}

/// Địa chỉ 100.x của máy này, đọc thẳng từ bảng định tuyến thay vì hỏi
/// `tailscale`.
///
/// Cùng mẹo với [`crate::session::lan_ip`]: nối một socket UDP tới máy chủ tên
/// miền của Tailscale — địa chỉ ấy chỉ đi được qua đường hầm, nên địa chỉ
/// nguồn hệ điều hành chọn chính là địa chỉ tailnet. Cần nó vì chương trình
/// dòng lệnh có thể lỡ nhịp, mà lúc ấy người dùng vẫn phải đọc được địa chỉ để
/// đọc cho máy bên kia.
pub fn tailnet_ip() -> Option<std::net::Ipv4Addr> {
    let ip = crate::session::dia_chi_nguon("100.100.100.100")?;
    is_tailnet_ip(ip.into()).then_some(ip)
}

/// Danh tính của một máy trong tailnet, tra từ địa chỉ `100.x.y.z`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Who {
    /// Tên ngắn của máy, ví dụ `may-cua-toi`.
    pub machine: String,
    /// Tài khoản sở hữu máy đó, ví dụ `ai-do@gmail.com`.
    pub login: String,
    pub user_id: u64,
    /// Nhãn ACL gắn cho máy (`tag:server`...). Máy gắn nhãn thuộc về nhãn chứ
    /// không thuộc về người, nên `login` của nó là người đã gắn nhãn.
    pub tags: Vec<String>,
}

#[derive(Deserialize)]
struct RawWhois {
    #[serde(rename = "Node")]
    node: Option<RawWhoisNode>,
    #[serde(rename = "UserProfile")]
    user: Option<RawUserProfile>,
}

#[derive(Deserialize)]
struct RawWhoisNode {
    #[serde(rename = "Name")]
    name: Option<String>,
    #[serde(rename = "Tags")]
    tags: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct RawUserProfile {
    #[serde(rename = "ID")]
    id: Option<u64>,
    #[serde(rename = "LoginName")]
    login_name: Option<String>,
}

/// Dải địa chỉ Tailscale cấp cho máy trong tailnet (CGNAT, `100.64.0.0/10`).
///
/// Kiểm tra dải trước khi hỏi Tailscale để khỏi gọi tiến trình con cho mọi kết
/// nối LAN — và để một địa chỉ ngoài dải không bao giờ đi được vào nhánh tin.
pub fn is_tailnet_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            a == 100 && (64..128).contains(&b)
        }
        // `fd7a:115c:a1e0::/48` là dải ULA riêng của Tailscale.
        std::net::IpAddr::V6(v6) => v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

/// Hỏi Tailscale xem địa chỉ này là máy nào, của ai. Lời gọi này **chặn**.
pub fn whois(ip: std::net::IpAddr) -> Result<Who, String> {
    let text = run(&["whois", "--json", &ip.to_string()])?;
    let raw: RawWhois =
        serde_json::from_str(&text).map_err(|err| format!("không hiểu JSON của whois: {err}"))?;
    let node = raw
        .node
        .ok_or_else(|| "whois không trả về máy nào".to_string())?;
    let user = raw
        .user
        .ok_or_else(|| "whois không trả về tài khoản".to_string())?;
    Ok(Who {
        machine: node
            .name
            .as_deref()
            .and_then(|name| name.split('.').next())
            .unwrap_or("?")
            .to_owned(),
        login: user.login_name.unwrap_or_default(),
        user_id: user
            .id
            .ok_or_else(|| "whois thiếu mã tài khoản".to_string())?,
        tags: node.tags.unwrap_or_default(),
    })
}

/// Máy ở địa chỉ này có phải máy *của chính mình* trong tailnet không.
///
/// "Của chính mình" nghĩa là cùng tài khoản sở hữu, chứ không chỉ là cùng
/// tailnet: tailnet có thể có khách được chia sẻ vào, và khách thì vẫn phải gõ
/// mật khẩu như người lạ.
///
/// Lời gọi này **chặn** — chạy nó trên luồng nền.
pub fn same_account(ip: std::net::IpAddr) -> Result<Who, String> {
    if !is_tailnet_ip(ip) {
        return Err(format!("{ip} không nằm trong dải Tailscale"));
    }
    let me = status();
    if !me.running() {
        return Err("Tailscale không chạy trên máy này".into());
    }
    let my_user = me
        .self_user_id
        .ok_or_else(|| "không biết máy này thuộc tài khoản nào".to_string())?;
    let who = whois(ip)?;
    // Máy gắn tag ACL không đứng tên người nào — Tailscale coi nó là thiết bị
    // của tailnet chứ không của tài khoản, nên `UserID` không bao giờ khớp.
    // Nói thẳng ra, không thì người dùng cứ tưởng mình cài sai.
    if !who.tags.is_empty() {
        return Err(format!(
            "máy {} chạy dưới tag ACL ({}) nên không tính là cùng tài khoản; \
             máy ấy phải gõ mật khẩu",
            who.machine,
            who.tags.join(", ")
        ));
    }
    if who.user_id != my_user {
        return Err(format!(
            "máy {} thuộc tài khoản khác ({})",
            who.machine, who.login
        ));
    }
    Ok(who)
}

/// Đường mà Tailscale đang thực sự chuyển gói tới một máy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Path {
    /// Gói đi thẳng máy nọ sang máy kia — nhanh nhất, đúng thứ ta cần cho video.
    Direct(String),
    /// Gói đi vòng qua máy chủ DERP của Tailscale. Chạy được ở mọi nơi nhưng
    /// độ trễ cao hơn hẳn và băng thông bị chia sẻ, video sẽ giật.
    Derp(String),
}

/// Hỏi Tailscale đang gửi tới `ip` bằng đường nào. Lời gọi này **chặn**.
///
/// Chỉ đúng khi hai máy đang thực sự trao đổi gói: lúc im lặng, Tailscale chưa
/// chọn đường nào cả.
pub fn path_to(ip: std::net::IpAddr) -> Option<Path> {
    doc_duong_di(&run(&["status", "--json"]).ok()?, ip)
}

/// Phần đọc JSON của [`path_to`], tách ra để test khỏi cần Tailscale thật.
fn doc_duong_di(text: &str, ip: std::net::IpAddr) -> Option<Path> {
    let raw: RawStatus = serde_json::from_str(text).ok()?;
    let dich = ip.to_string();
    let node = raw
        .peer?
        .into_values()
        .find(|node| node.ips.iter().flatten().any(|có| *có == dich))?;
    match node.cur_addr.as_deref().unwrap_or("") {
        "" => Some(Path::Derp(node.relay.unwrap_or_default())),
        addr => Some(Path::Direct(addr.to_owned())),
    }
}

/// Giây kể từ mốc Unix. Trả 0 nếu đồng hồ máy lùi trước 1970 — lúc đó mọi
/// khoảng cách thời gian đều vô nghĩa, thà không hiện gì.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Đọc mốc thời gian RFC 3339 của Tailscale (`2026-08-22T00:10:00.1Z`) thành
/// giây Unix. Chỉ nhận đúng dạng UTC mà Tailscale in ra, không cần tổng quát.
fn doc_moc(text: &str) -> Option<i64> {
    let (ngay, gio) = text.split_once('T')?;
    let mut phan_ngay = ngay.split('-');
    let nam: i64 = phan_ngay.next()?.parse().ok()?;
    let thang: u32 = phan_ngay.next()?.parse().ok()?;
    let ngay_trong_thang: i64 = phan_ngay.next()?.parse().ok()?;

    let gio = gio.trim_end_matches('Z');
    let gio = gio.split('.').next()?;
    let mut phan_gio = gio.split(':');
    let h: i64 = phan_gio.next()?.parse().ok()?;
    let m: i64 = phan_gio.next()?.parse().ok()?;
    let sec: i64 = phan_gio.next().unwrap_or("0").parse().ok()?;

    Some(so_ngay(nam, thang, ngay_trong_thang) * 86_400 + h * 3_600 + m * 60 + sec)
}

/// Số ngày từ 1970-01-01 tới ngày cho trước (thuật toán lịch dân dụng của
/// Howard Hinnant). Tự tính vì kéo cả một thư viện ngày tháng vào chỉ để hiện
/// "3 giờ trước" thì không đáng.
fn so_ngay(nam: i64, thang: u32, ngay: i64) -> i64 {
    let nam = if thang <= 2 { nam - 1 } else { nam };
    let ky = if nam >= 0 { nam } else { nam - 399 } / 400;
    let nam_trong_ky = nam - ky * 400;
    let thang = thang as i64;
    let ngay_trong_nam = (153 * (if thang > 2 { thang - 3 } else { thang + 9 }) + 2) / 5 + ngay - 1;
    let ngay_trong_ky = nam_trong_ky * 365 + nam_trong_ky / 4 - nam_trong_ky / 100 + ngay_trong_nam;
    ky * 146_097 + ngay_trong_ky - 719_468
}

/// "5 phút trước", "3 giờ trước", "2 ngày trước". `None` nếu mốc không đọc được.
fn khoang_cach(moc: &str, bay_gio: i64) -> Option<String> {
    let truoc = bay_gio.checked_sub(doc_moc(moc)?)?;
    if truoc < 0 {
        return None;
    }
    Some(match truoc {
        0..=89 => "vừa xong".to_string(),
        90..=5399 => format!("{} phút trước", truoc / 60),
        5400..=86_399 => format!("{} giờ trước", truoc / 3600),
        _ => format!("{} ngày trước", truoc / 86_400),
    })
}

/// Đang chạy ngon mà hỏng bao nhiêu lần liên tiếp thì mới chịu tin là hỏng.
const HONG_TOI_DA: u32 = 3;

/// Có nên giữ bản chụp cũ thay vì nhận bản mới không.
///
/// Một lần hỏi hụt không có nghĩa là Tailscale đã tắt — trên macOS nó hay lỡ
/// nhịp. Nhảy ngay sang màu đỏ thì người dùng mất luôn địa chỉ 100.x đang đọc
/// dở và danh sách máy, rồi vài giây sau nó lại hiện về: khung nhấp nháy mà
/// chẳng có gì hỏng thật.
fn giu_ban_cu(cu: &Status, moi: &Status, hong: &AtomicU32) -> bool {
    let dang_chay = matches!(cu.state, Some(State::Running));
    let van_chay = matches!(moi.state, Some(State::Running));
    if van_chay || !dang_chay {
        hong.store(0, Ordering::Relaxed);
        return false;
    }
    hong.fetch_add(1, Ordering::Relaxed) + 1 < HONG_TOI_DA
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
    nhu_dong_lenh(&mut command);
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
            out.push_str(&format!(
                "thấy {} máy khác trong tailnet",
                status.peers.len()
            ));
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
    /// Số lần hỏi hụt liên tiếp, đếm từ lúc trạng thái còn đang tốt.
    hong: Arc<AtomicU32>,
    /// Đánh thức giao diện khi luồng nền hỏi xong. Thiếu nó thì kết quả nằm im
    /// tới lần người dùng động vào cửa sổ — đăng nhập xong mà màn hình vẫn ghi
    /// "chưa đăng nhập" cho tới khi rê chuột.
    repaint: Arc<std::sync::OnceLock<egui::Context>>,
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
            hong: Arc::new(AtomicU32::new(0)),
            repaint: Arc::new(std::sync::OnceLock::new()),
        };
        watcher.refresh();
        watcher
    }

    /// Cho watcher đường đánh thức giao diện. Gọi mỗi khung hình cho gọn.
    pub fn attach(&self, ctx: &egui::Context) {
        if self.repaint.get().is_none() {
            let _ = self.repaint.set(ctx.clone());
        }
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
        self.status
            .lock()
            .expect("khoá trạng thái tailscale")
            .clone()
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
        let hong = self.hong.clone();
        let busy = self.busy.clone();
        let message = self.message.clone();
        let repaint = self.repaint.clone();
        let spawned = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                let note = job();
                *message.lock().expect("khoá lời nhắn") = note;
                let moi = status();
                let mut cho = slot.lock().expect("khoá trạng thái tailscale");
                if !giu_ban_cu(&cho, &moi, &hong) {
                    *cho = moi;
                }
                drop(cho);
                busy.store(false, Ordering::Relaxed);
                if let Some(ctx) = repaint.get() {
                    ctx.request_repaint();
                }
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
            run(&["up"])
                .err()
                .map(|err| format!("không bật được: {err}"))
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
    fn cau_than_tieng_anh_khong_thanh_loi_json() {
        // Nguyên văn bản Tailscale cài từ App Store in ra stdout, mã thoát 0.
        let text = "The Tailscale GUI failed to start: The operation couldn\u{2019}t be \
                    completed. (Tailscale.CLIError error 3.)\n\n";
        let Err(loi) = doc_trang_thai(text) else {
            panic!("câu than tiếng Anh không phải JSON");
        };
        assert!(loi.contains("mở ứng dụng Tailscale"), "{loi}");
        assert!(!loi.contains("JSON"), "{loi}");
    }

    #[test]
    fn bo_qua_chu_thua_dung_truoc_json() {
        let raw = doc_trang_thai("Warning: something\n{\"BackendState\":\"Running\"}")
            .expect("vẫn đọc được JSON");
        assert_eq!(raw.backend_state.as_deref(), Some("Running"));
    }

    #[test]
    fn hong_mot_hai_lan_thi_van_giu_ban_cu() {
        let chay = Status {
            state: Some(State::Running),
            ..Status::default()
        };
        let hong_roi = Status {
            state: Some(State::Broken("lỡ nhịp".into())),
            ..Status::default()
        };
        let dem = AtomicU32::new(0);
        assert!(giu_ban_cu(&chay, &hong_roi, &dem));
        assert!(giu_ban_cu(&chay, &hong_roi, &dem));
        // Lần thứ ba thì chịu: hỏng thật rồi.
        assert!(!giu_ban_cu(&chay, &hong_roi, &dem));
        // Hỏi lại được thì quên hết, lần hụt sau lại được tha.
        assert!(!giu_ban_cu(&chay, &chay, &dem));
        assert!(giu_ban_cu(&chay, &hong_roi, &dem));
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
            last_seen: String::new(),
        }
    }

    #[test]
    fn phan_biet_noi_thang_va_di_vong_derp() {
        let json = r#"{"Peer":{"k1":{"TailscaleIPs":["100.64.1.9"],"CurAddr":"203.0.113.7:41641","Relay":"sin"},
                               "k2":{"TailscaleIPs":["100.64.1.10"],"CurAddr":"","Relay":"sin"}}}"#;
        let thang = "100.64.1.9".parse().expect("ip hợp lệ");
        let vong = "100.64.1.10".parse().expect("ip hợp lệ");
        let la = "100.64.1.11".parse().expect("ip hợp lệ");
        assert!(
            matches!(doc_duong_di(json, thang), Some(Path::Direct(addr)) if addr == "203.0.113.7:41641")
        );
        assert!(matches!(doc_duong_di(json, vong), Some(Path::Derp(vung)) if vung == "sin"));
        // Máy lạ thì im lặng, không đoán bừa là đang đi vòng.
        assert!(doc_duong_di(json, la).is_none());
    }

    #[test]
    fn doc_duoc_moc_thoi_gian_cua_tailscale() {
        assert_eq!(doc_moc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(doc_moc("2026-08-22T00:10:00.1Z"), Some(1_787_357_400));
        // Năm nhuận và ngày ngay sau tháng 2 là chỗ thuật toán lịch hay sai.
        assert_eq!(doc_moc("2000-02-29T12:00:00Z"), Some(951_825_600));
        assert_eq!(doc_moc("1972-03-01T00:00:00Z"), Some(68_256_000));
        assert_eq!(doc_moc("rác"), None);
    }

    #[test]
    fn khoang_cach_noi_tieng_viet() {
        let moc = "2026-08-22T00:00:00Z";
        let goc = doc_moc(moc).unwrap();
        assert_eq!(khoang_cach(moc, goc + 30).as_deref(), Some("vừa xong"));
        assert_eq!(
            khoang_cach(moc, goc + 600).as_deref(),
            Some("10 phút trước")
        );
        assert_eq!(
            khoang_cach(moc, goc + 10_800).as_deref(),
            Some("3 giờ trước")
        );
        assert_eq!(
            khoang_cach(moc, goc + 172_800).as_deref(),
            Some("2 ngày trước")
        );
        // Đồng hồ hai máy lệch nhau thì thà không hiện còn hơn hiện "âm".
        assert_eq!(khoang_cach(moc, goc - 60), None);
    }

    #[test]
    fn dai_dia_chi_tailscale() {
        assert!(is_tailnet_ip("100.68.186.31".parse().unwrap()));
        assert!(is_tailnet_ip("100.64.0.1".parse().unwrap()));
        assert!(is_tailnet_ip("fd7a:115c:a1e0::1".parse().unwrap()));
        assert!(!is_tailnet_ip("100.63.255.255".parse().unwrap()));
        assert!(!is_tailnet_ip("100.128.0.1".parse().unwrap()));
        assert!(!is_tailnet_ip("192.168.1.5".parse().unwrap()));
        assert!(!is_tailnet_ip("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn doc_duoc_whois() {
        let raw: RawWhois = serde_json::from_str(
            r#"{"Node":{"Name":"may-a.tail1.ts.net.","Tags":["tag:server"]},
                 "UserProfile":{"ID":42,"LoginName":"toi@vidu.com"}}"#,
        )
        .expect("đọc được whois");
        let node = raw.node.unwrap();
        assert_eq!(node.name.unwrap().split('.').next(), Some("may-a"));
        assert_eq!(raw.user.unwrap().id, Some(42));
    }

    #[test]
    fn may_dang_bat_dung_truoc_trong_danh_sach() {
        let mut peers = [peer("b", false), peer("c", true), peer("a", true)];
        sort_peers(&mut peers);
        let names: Vec<&str> = peers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["a", "c", "b"]);
    }
}
