//! Ba việc phải nhờ hệ điều hành làm: đọc/ghi clipboard, mở hộp thoại chọn
//! tệp, và mở thư mục chứa tệp vừa nhận.
//!
//! Gom chung vì cùng một kiểu rắc rối: cái nào cũng có thể chặn luồng gọi vô
//! thời hạn, mà luồng gọi ở đây là luồng vẽ giao diện — chặn nó một giây là
//! cửa sổ đứng hình một giây. Nên tất cả đều chạy ở luồng khác và nói chuyện
//! với giao diện qua kênh.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rd_protocol::MAX_CLIPBOARD_TEXT;

/// Bao lâu ngó clipboard một lần.
///
/// Không hệ nào cho đăng ký nhận thông báo khi clipboard đổi theo cách dùng
/// được từ Rust, nên đành hỏi vòng vòng. 200 ms là quãng người dùng copy xong,
/// chuyển cửa sổ rồi mới dán — tới lúc đó nội dung đã sang tới máy kia.
///
/// Trên Windows nhịp này rẻ vì mỗi vòng chỉ hỏi *số thứ tự* clipboard (xem
/// [`clipboard_serial`]); các hệ khác mới thực sự đọc nội dung mỗi vòng.
const POLL: Duration = Duration::from_millis(200);

/// Thử ghi clipboard mấy lần trước khi bỏ cuộc.
///
/// Windows cho *một* chương trình giữ clipboard tại một thời điểm; Chrome,
/// Office hay chính Explorer đều có lúc giữ nó vài chục mili giây. Hỏng một
/// nhịp mà bỏ luôn thì đoạn văn bản đó mất hẳn — máy kia copy rồi mà dán ra
/// vẫn là nội dung cũ.
const SET_TRIES: u32 = 5;

/// Nghỉ giữa hai lần thử ghi.
const SET_RETRY_DELAY: Duration = Duration::from_millis(40);

/// Đầu ghi clipboard, dùng chung được cho luồng mạng.
///
/// Hai lớp gián tiếp, mỗi lớp một lý do. Ô `to_set` rỗng được vì người dùng
/// bật/tắt đồng bộ giữa chừng — cầu nối cũ bị thả, cầu nối mới dựng lên, còn
/// phần mạng vẫn giữ nguyên một `ClipboardSink` từ đầu phiên tới cuối. Cờ
/// `peer` để đường *gửi đi* biết có ai ở đầu kia không: dồn những thứ copy từ
/// mấy phút trước rồi bắn một loạt lúc vừa nối là ghi đè clipboard của họ.
///
/// Quan trọng hơn cả: cả hai chiều đều chạy ngoài luồng vẽ giao diện. Cửa sổ
/// thu nhỏ xuống thanh tác vụ là hệ điều hành ngừng gửi lệnh vẽ, luồng ấy đứng
/// im — mà máy host thì gần như lúc nào cũng thu nhỏ.
#[derive(Clone, Default)]
pub struct ClipboardSink {
    to_set: Arc<Mutex<Option<mpsc::Sender<String>>>>,
    peer: Arc<AtomicBool>,
}

impl std::fmt::Debug for ClipboardSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipboardSink")
            .field("peer", &self.peer.load(Ordering::Relaxed))
            .finish()
    }
}

impl ClipboardSink {
    /// Máy kia vừa copy: ghi đè clipboard máy này. Không có cầu nối (người dùng
    /// tắt đồng bộ) thì lặng lẽ bỏ qua.
    pub fn set(&self, text: String) {
        if let Ok(slot) = self.to_set.lock()
            && let Some(tx) = slot.as_ref()
        {
            let _ = tx.send(text);
        }
    }

    /// Có phiên đang chạy không.
    pub fn has_peer(&self) -> bool {
        self.peer.load(Ordering::Relaxed)
    }

    /// Đánh dấu "đang có người xem" cho tới khi giá trị trả về bị thả.
    pub fn session_guard(&self) -> SessionGuard {
        self.peer.store(true, Ordering::Relaxed);
        SessionGuard(Arc::clone(&self.peer))
    }

    fn attach(&self, tx: Option<mpsc::Sender<String>>) {
        if let Ok(mut slot) = self.to_set.lock() {
            *slot = tx;
        }
    }
}

/// Hạ cờ "đang có người xem" khi phiên kết thúc, kể cả khi kết thúc vì lỗi.
pub struct SessionGuard(Arc<AtomicBool>);

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

/// Hàm nhận văn bản vừa copy ở máy này. Gọi từ luồng nền của clipboard.
pub type ClipboardOut = Arc<dyn Fn(String) + Send + Sync>;

/// Cầu nối clipboard giữa hai máy.
///
/// Một luồng nền giữ `arboard::Clipboard` và làm cả hai chiều. Phải là luồng
/// nền vì trên Windows việc đọc clipboard là mở một khoá toàn máy — ứng dụng
/// khác đang giữ khoá đó thì lời gọi nằm chờ.
pub struct Clipboard {
    /// Nơi gửi văn bản vừa copy đi. Luồng nền đọc ô này mỗi lần thấy clipboard
    /// đổi, nên đổi người nhận giữa chừng (nối lại) chỉ là thay giá trị ở đây.
    out: Arc<Mutex<Option<ClipboardOut>>>,
    /// Giữ để lúc thả thì gỡ đầu ghi khỏi `sink`.
    sink: ClipboardSink,
}

impl Clipboard {
    /// Dựng cầu nối và luồng nền của nó.
    ///
    /// Thả giá trị này là luồng nền tự dừng — nhờ vậy tắt đồng bộ clipboard
    /// cũng là ngừng đọc clipboard thật, chứ không phải vẫn đọc rồi vứt đi.
    pub fn start(sink: &ClipboardSink) -> Self {
        let (to_set, set_rx) = mpsc::channel();
        let out: Arc<Mutex<Option<ClipboardOut>>> = Arc::new(Mutex::new(None));
        // Dựng `arboard::Clipboard` *bên trong* luồng chứ không dựng ở đây rồi
        // chuyển sang: nó gắn với trạng thái của luồng tạo ra nó.
        let spawned = std::thread::Builder::new()
            .name("rd-clipboard".into())
            .spawn({
                let out = Arc::clone(&out);
                move || run(&out, set_rx)
            });
        if let Err(err) = spawned {
            tracing::warn!(%err, "không tạo được luồng clipboard");
        }
        sink.attach(Some(to_set));
        Self {
            out,
            sink: sink.clone(),
        }
    }

    /// Đặt nơi nhận văn bản vừa copy ở máy này. `None` là tạm ngừng gửi đi.
    pub fn forward_to(&self, out: Option<ClipboardOut>) {
        if let Ok(mut slot) = self.out.lock() {
            *slot = out;
        }
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        // Gỡ đầu ghi ra: luồng nền thấy kênh đứt thì tự dừng, và phần mạng
        // nhận clipboard từ máy kia cũng thôi ghi vào máy này.
        self.sink.attach(None);
    }
}

/// Số thứ tự clipboard của Windows — tăng lên mỗi lần có ai đó ghi vào.
///
/// Hỏi con số này *không* mở khoá clipboard, nên hỏi 5 lần mỗi giây cũng không
/// cản ai. Đọc thẳng nội dung thì ngược lại: mỗi lần đọc là một lần giành khoá
/// toàn máy, mà đúng lúc người dùng bấm Ctrl+C ở phần mềm khác thì chính lời
/// gọi copy của họ hỏng — copy xong dán ra vẫn ra nội dung cũ.
#[cfg(target_os = "windows")]
fn clipboard_serial() -> u32 {
    use windows::Win32::System::DataExchange::GetClipboardSequenceNumber;
    // SAFETY: hàm chỉ đọc một bộ đếm của phiên đăng nhập, không nhận con trỏ.
    unsafe { GetClipboardSequenceNumber() }
}

/// Ghi clipboard, thử lại vài nhịp nếu máy khác đang giữ khoá.
///
/// Nhận thẳng lời gọi ghi thay vì `arboard::Clipboard`: máy dựng bản Windows
/// không có clipboard của Windows để chạy thử, nên phần thử-lại phải kiểm
/// chứng được bằng một hàm giả.
fn retry_set<E>(mut set: impl FnMut() -> Result<(), E>, nghi: Duration) -> Result<(), E> {
    let mut last_err = None;
    for lan in 0..SET_TRIES {
        match set() {
            Ok(()) => return Ok(()),
            Err(err) => {
                last_err = Some(err);
                if lan + 1 < SET_TRIES {
                    std::thread::sleep(nghi);
                }
            }
        }
    }
    Err(last_err.expect("SET_TRIES phải lớn hơn 0"))
}

/// Nội dung mới nhất ta biết trên clipboard máy này, dù do người dùng copy hay
/// do chính ta vừa ghi vào.
///
/// Đây là chốt chặn vòng lặp vô tận: không có nó thì mỗi lần nhận từ máy kia
/// ta lại thấy clipboard "vừa đổi" và gửi ngược lại. Tách khỏi vòng lặp để
/// kiểm chứng được — vòng lặp thật cần clipboard thật, chốt chặn thì không.
#[derive(Default)]
struct DaBiet(String);

/// Nội dung vừa đọc được ở máy này thuộc loại nào.
#[derive(Debug, PartialEq)]
enum Doc {
    /// Đúng cái ta đã biết — không phải người dùng vừa copy.
    Cu,
    /// Vượt giới hạn giao thức, chỉ ghi nhận chứ không gửi.
    QuaDai(usize),
    /// Cần đẩy sang máy kia.
    Moi(String),
}

impl DaBiet {
    /// Máy kia gửi sang: có cần ghi vào clipboard máy này không.
    fn can_ghi(&self, text: &str) -> bool {
        text != self.0
    }

    /// Ghi nhận cái ta vừa tự tay đặt vào clipboard.
    fn ghi_nhan(&mut self, text: String) {
        self.0 = text;
    }

    /// Phân loại nội dung vừa đọc được, đồng thời ghi nhận nó.
    ///
    /// Ghi nhận cả khi quá dài: không thì cứ 200 ms một lần ta lại thấy nó
    /// "vừa đổi" và lại đo lại độ dài.
    fn doc(&mut self, text: String) -> Doc {
        if text == self.0 {
            return Doc::Cu;
        }
        self.0 = text.clone();
        if text.len() > MAX_CLIPBOARD_TEXT {
            Doc::QuaDai(text.len())
        } else {
            Doc::Moi(text)
        }
    }
}

fn run(out: &Mutex<Option<ClipboardOut>>, to_set: mpsc::Receiver<String>) {
    let mut clipboard = match arboard::Clipboard::new() {
        Ok(clipboard) => clipboard,
        Err(err) => {
            tracing::warn!(%err, "không mở được clipboard hệ thống");
            return;
        }
    };

    let mut last = DaBiet(clipboard.get_text().unwrap_or_default());
    #[cfg(target_os = "windows")]
    let mut serial = clipboard_serial();

    loop {
        match to_set.recv_timeout(POLL) {
            Ok(text) => {
                // Dồn lấy cái mới nhất: dán lần lượt từng cái cũ chỉ làm
                // clipboard nhấp nháy qua mấy giá trị không ai cần.
                let mut text = text;
                while let Ok(newer) = to_set.try_recv() {
                    text = newer;
                }
                if last.can_ghi(&text) {
                    match retry_set(|| clipboard.set_text(text.clone()), SET_RETRY_DELAY) {
                        Ok(()) => {
                            tracing::info!(
                                len = text.len(),
                                "đã dán clipboard của máy kia vào máy này"
                            );
                            last.ghi_nhan(text);
                        }
                        Err(err) => tracing::warn!(%err, "không ghi được clipboard"),
                    }
                    // Chính ta vừa ghi nên số thứ tự đã nhảy; không ghi nhận
                    // thì vòng sau tưởng người dùng vừa copy và gửi ngược lại.
                    #[cfg(target_os = "windows")]
                    {
                        serial = clipboard_serial();
                    }
                }
                continue;
            }
            Err(RecvTimeoutError::Timeout) => {}
            // Giao diện đã thả cầu nối: hết việc.
            Err(RecvTimeoutError::Disconnected) => break,
        }

        // Không ai động vào clipboard từ vòng trước thì khỏi mở nó ra làm gì.
        #[cfg(target_os = "windows")]
        {
            let moi = clipboard_serial();
            if moi == serial {
                continue;
            }
            serial = moi;
        }

        match clipboard.get_text() {
            Ok(text) => match last.doc(text) {
                Doc::Cu => {}
                Doc::QuaDai(len) => tracing::debug!(len, "clipboard quá dài, không đồng bộ"),
                Doc::Moi(text) => {
                    let gui = out.lock().ok().and_then(|slot| slot.clone());
                    match gui {
                        Some(gui) => {
                            tracing::info!(len = text.len(), "máy này vừa copy, đẩy sang máy kia");
                            gui(text);
                        }
                        // Chưa nối thì thôi, nhưng `doc` đã ghi nhận rồi: không
                        // thì vòng sau lại thấy nó "vừa đổi".
                        None => tracing::debug!(len = text.len(), "chưa có ai để gửi clipboard"),
                    }
                }
            },
            // Clipboard đang giữ ảnh hoặc danh sách tệp. Không phải lỗi, chỉ là
            // không có gì cho ta làm.
            Err(arboard::Error::ContentNotAvailable) => {}
            Err(err) => tracing::debug!(%err, "không đọc được clipboard"),
        }
    }
}

/// Hộp thoại chọn tệp của chính hệ điều hành.
///
/// Hộp thoại chặn cho tới lúc người dùng bấm xong, nên nó phải nằm ở luồng
/// khác. Trên macOS thì bản thân hộp thoại lại bắt buộc chạy ở luồng chính —
/// rfd tự chuyển lời gọi về đó, ta chỉ cần đừng chặn luồng vẽ trong lúc chờ.
#[derive(Default)]
pub struct FilePicker {
    picked: Option<mpsc::Receiver<Vec<PathBuf>>>,
}

impl FilePicker {
    /// Có hộp thoại nào đang mở không.
    pub fn busy(&self) -> bool {
        self.picked.is_some()
    }

    /// Chọn tệp để gửi — chọn được nhiều cái một lúc.
    pub fn open(&mut self) {
        self.spawn(|| {
            rfd::FileDialog::new()
                .set_title("Chọn tệp để gửi sang máy kia")
                .pick_files()
                .unwrap_or_default()
        });
    }

    /// Chọn thư mục — dùng cho ô "thư mục nhận tệp".
    pub fn open_folder(&mut self) {
        self.spawn(|| {
            rfd::FileDialog::new()
                .set_title("Chọn thư mục lưu tệp nhận được")
                .pick_folder()
                .into_iter()
                .collect()
        });
    }

    /// Không làm gì nếu đang có một hộp thoại mở rồi: hai cái chồng nhau thì
    /// cái sau che cái trước, và người dùng không hiểu vì sao bấm xong lại hiện
    /// tiếp một cái nữa.
    fn spawn(&mut self, pick: impl FnOnce() -> Vec<PathBuf> + Send + 'static) {
        if self.busy() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("rd-picker".into())
            .spawn(move || {
                let _ = tx.send(pick());
            });
        match spawned {
            Ok(_) => self.picked = Some(rx),
            Err(err) => tracing::warn!(%err, "không mở được hộp thoại chọn"),
        }
    }

    /// Những tệp vừa được chọn. Rỗng khi hộp thoại còn đang mở, hoặc khi người
    /// dùng bấm huỷ.
    pub fn take(&mut self) -> Vec<PathBuf> {
        let Some(rx) = &self.picked else {
            return Vec::new();
        };
        match rx.try_recv() {
            Ok(paths) => {
                self.picked = None;
                paths
            }
            Err(mpsc::TryRecvError::Empty) => Vec::new(),
            // Luồng chết mà chưa gửi gì: coi như huỷ, và đừng chờ nữa.
            Err(mpsc::TryRecvError::Disconnected) => {
                self.picked = None;
                Vec::new()
            }
        }
    }
}

/// Mở trình quản lý tệp, con trỏ đặt sẵn vào tệp vừa nhận.
///
/// Chỉ trỏ tới chứ không mở tệp: tệp từ máy khác gửi sang mà tự mở ra thì một
/// cú bấm nhầm cũng đủ chạy thứ người dùng chưa kịp nhìn tên.
/// Mở tệp bằng phần mềm mặc định của hệ điều hành.
///
/// Nhận xong mà còn phải đi tìm trong thư mục thì thao tác thừa; phần lớn
/// trường hợp người ta chỉ muốn xem ngay cái vừa nhận.
pub fn open_file(path: &Path) {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg(path);
        command
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        // `explorer <đường dẫn>` mở tệp bằng phần mềm gắn với đuôi của nó.
        let mut command = std::process::Command::new("explorer");
        command.arg(path);
        command
    };
    if let Err(err) = command.spawn() {
        tracing::warn!(%err, path = %path.display(), "không mở được tệp");
    }
}

pub fn reveal(path: &Path) {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg("-R").arg(path);
        command
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("explorer");
        // Explorer đòi đúng dạng này, dấu phẩy dính liền đường dẫn.
        command.arg(format!("/select,{}", path.display()));
        command
    };
    if let Err(err) = command.spawn() {
        tracing::warn!(%err, path = %path.display(), "không mở được thư mục");
    }
}

/// Giữ máy không ngủ trong lúc đang chia sẻ màn hình.
///
/// Máy host ngủ hoặc màn hình tắt là mất luôn đường chụp: DXGI Desktop
/// Duplication không có màn hình để sao chép, còn máy đã ngủ thì gói UDP tới
/// cũng chẳng ai nghe. Vì thế chỗ này phải chặn cả hai mức — máy và màn hình.
///
/// Thả giá trị này là trả máy về nếp ngủ bình thường của nó.
pub struct KeepAwake {
    #[cfg(target_os = "windows")]
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    #[cfg(target_os = "windows")]
    thread: Option<std::thread::JoinHandle<()>>,
    #[cfg(not(target_os = "windows"))]
    child: Option<std::process::Child>,
}

impl KeepAwake {
    #[cfg(target_os = "windows")]
    pub fn start() -> Self {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use windows::Win32::System::Power::{
            ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
        };

        let stop = Arc::new(AtomicBool::new(false));
        // Trạng thái này gắn với *luồng* gọi nó, nên phải có một luồng sống
        // suốt phiên để giữ. Đặt ở luồng vẽ giao diện thì Windows xoá nó ngay
        // khi luồng ấy làm việc khác.
        let thread = std::thread::Builder::new()
            .name("rd-keep-awake".into())
            .spawn({
                let stop = Arc::clone(&stop);
                move || {
                    unsafe {
                        SetThreadExecutionState(
                            ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED,
                        )
                    };
                    while !stop.load(Ordering::Acquire) {
                        std::thread::sleep(Duration::from_millis(200));
                    }
                    // Bỏ hai cờ kia đi, giữ lại ES_CONTINUOUS: đó là cách khai
                    // "tôi hết việc rồi, máy cứ ngủ như thường".
                    unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
                }
            });
        match thread {
            Ok(thread) => Self {
                stop,
                thread: Some(thread),
            },
            Err(err) => {
                tracing::warn!(%err, "không giữ được máy thức");
                Self { stop, thread: None }
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    pub fn start() -> Self {
        // `caffeinate` có sẵn trong mọi bản macOS: -d màn hình, -i tiến trình,
        // -m ổ đĩa, -s khi đang cắm điện. Không có API nào gọn hơn mà không
        // phải nói chuyện với IOKit.
        let child = std::process::Command::new("caffeinate")
            .args(["-dims"])
            .spawn();
        match child {
            Ok(child) => Self { child: Some(child) },
            Err(err) => {
                tracing::warn!(%err, "không giữ được máy thức");
                Self { child: None }
            }
        }
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        #[cfg(target_os = "windows")]
        {
            self.stop.store(true, std::sync::atomic::Ordering::Release);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
        #[cfg(not(target_os = "windows"))]
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Tên đăng ký ở mục tự khởi động của hệ điều hành.
const AUTOSTART_NAME: &str = "RemoteDesktop";

#[cfg(target_os = "windows")]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

/// Giá trị đang nằm trong mục tự khởi động, đọc thẳng từ hệ điều hành.
///
/// Khác [`autostart_enabled`] ở chỗ trả về *nội dung*: có đúng đường dẫn phần
/// mềm và có `--background` không. Bật được mục mà nội dung sai thì máy có điện
/// lại vẫn chẳng chia sẻ gì.
pub fn autostart_value() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        let out = std::process::Command::new("reg")
            .args(["query", RUN_KEY, "/v", AUTOSTART_NAME])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        // Dòng cần tìm: "    RemoteDesktop    REG_SZ    "C:\...\rd.exe" --background"
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find_map(|line| line.split("REG_SZ").nth(1))
            .map(|value| value.trim().to_string())
    }
    #[cfg(not(target_os = "windows"))]
    std::fs::read_to_string(launch_agent()?).ok()
}

/// Tự kiểm ba thứ chỉ hệ điều hành thật mới trả lời được.
///
/// Máy dựng bản Windows ở đây là macOS: không có clipboard của Windows, không
/// có registry, không có lần khởi động lại nào để mà xem. Gói cả ba phép thử
/// vào một lệnh (`--tu-kiem`) để người ngồi trước máy Windows chạy hộ một lần
/// rồi gửi lại bảng kết quả.
pub fn selftest() -> bool {
    println!("== Tự kiểm ({}) ==", std::env::consts::OS);
    let clipboard = kiem_clipboard();
    let autostart = kiem_tu_khoi_dong();
    let ngam = kiem_chay_ngam();
    let tat_ca = clipboard && autostart && ngam;
    println!(
        "\nKết luận: {}",
        if tat_ca {
            "tất cả đều đạt"
        } else {
            "có mục KHÔNG ĐẠT ở trên"
        }
    );
    tat_ca
}

/// Ghi rồi đọc lại clipboard thật của hệ điều hành, có đo thời gian.
fn kiem_clipboard() -> bool {
    let mut clipboard = match arboard::Clipboard::new() {
        Ok(clipboard) => clipboard,
        Err(err) => {
            println!("[KHÔNG ĐẠT] mở clipboard hệ thống: {err}");
            return false;
        }
    };
    // Giữ lại thứ người dùng đang có để trả lại nguyên trạng.
    let cu = clipboard.get_text().ok();

    let moc = format!("rd-tu-kiem-{}", std::process::id());
    let bat_dau = std::time::Instant::now();
    if let Err(err) = retry_set(|| clipboard.set_text(moc.clone()), SET_RETRY_DELAY) {
        println!("[KHÔNG ĐẠT] ghi clipboard: {err}");
        return false;
    }
    let doc = clipboard.get_text();
    let mat = bat_dau.elapsed();
    if let Some(cu) = cu {
        let _ = retry_set(|| clipboard.set_text(cu.clone()), SET_RETRY_DELAY);
    }

    match doc {
        Ok(text) if text == moc => {
            println!(
                "[đạt] vòng ghi/đọc clipboard mất {:.1} ms",
                mat.as_secs_f64() * 1000.0
            );
            true
        }
        Ok(text) => {
            println!("[KHÔNG ĐẠT] đọc lại clipboard ra thứ khác: {text:?}");
            false
        }
        Err(err) => {
            println!("[KHÔNG ĐẠT] đọc lại clipboard: {err}");
            false
        }
    }
}

/// Bật mục tự khởi động, đọc lại nội dung, rồi trả về đúng trạng thái ban đầu.
fn kiem_tu_khoi_dong() -> bool {
    let truoc = autostart_enabled();
    // Giữ nguyên văn cái đang có: mục cũ có thể trỏ tới bản cài đặt thật, còn
    // bản đang chạy phép thử này có khi chỉ là bản chép tạm ở đâu đó.
    let gia_tri_cu = autostart_value();
    if let Err(err) = set_autostart(true) {
        println!("[KHÔNG ĐẠT] bật tự khởi động: {err}");
        return false;
    }
    let gia_tri = autostart_value();
    match (truoc, &gia_tri_cu) {
        // Người dùng chưa bật thì đừng để lại gì sau lưng họ.
        (false, _) => {
            let _ = set_autostart(false);
        }
        (true, Some(cu)) => khoi_phuc_tu_khoi_dong(cu),
        (true, None) => {}
    }

    match gia_tri {
        Some(value) if value.contains("--background") => {
            println!("[đạt] mục tự khởi động ghi được, có --background");
            for dong in value.lines().filter(|dong| !dong.trim().is_empty()) {
                println!("      {}", dong.trim());
            }
            println!(
                "      (trạng thái ban đầu: {})",
                if truoc {
                    "đang bật, đã trả lại nguyên văn"
                } else {
                    "đang tắt, đã trả lại"
                }
            );
            true
        }
        Some(value) => {
            println!("[KHÔNG ĐẠT] mục tự khởi động thiếu --background: {value}");
            false
        }
        None => {
            println!("[KHÔNG ĐẠT] ghi xong mà đọc lại không thấy mục tự khởi động");
            false
        }
    }
}

/// Đặt lại y nguyên mục tự khởi động đã đọc được trước khi thử.
fn khoi_phuc_tu_khoi_dong(cu: &str) {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("reg")
            .args([
                "add",
                RUN_KEY,
                "/v",
                AUTOSTART_NAME,
                "/t",
                "REG_SZ",
                "/d",
                cu,
                "/f",
            ])
            .output();
    }
    #[cfg(not(target_os = "windows"))]
    if let Some(path) = launch_agent() {
        let _ = std::fs::write(path, cu);
    }
}

/// Cờ `--background` của chính bản đang chạy có kéo theo chế độ chia sẻ không.
fn kiem_chay_ngam() -> bool {
    match crate::app::Args::parse(["--background".to_string()].into_iter()) {
        Ok(args) if args.background && args.host => {
            println!("[đạt] --background vào thẳng chế độ chia sẻ, cửa sổ thu nhỏ");
            true
        }
        Ok(args) => {
            println!(
                "[KHÔNG ĐẠT] --background: background={} host={}",
                args.background, args.host
            );
            false
        }
        Err(err) => {
            println!("[KHÔNG ĐẠT] đọc tham số --background: {err}");
            false
        }
    }
}

/// Máy này có tự mở phần mềm ở chế độ chia sẻ mỗi lần đăng nhập không.
///
/// Đây là mảnh ghép cho trường hợp mất điện: máy có điện lại, người dùng (hoặc
/// chế độ đăng nhập tự động của Windows) đăng nhập, phần mềm tự lên ở chế độ
/// chạy ngầm và tự chờ. Không thay được cho một dịch vụ chạy trước lúc đăng
/// nhập — màn hình khoá thì vẫn chưa chia sẻ được.
pub fn autostart_enabled() -> bool {
    #[cfg(target_os = "windows")]
    {
        let out = std::process::Command::new("reg")
            .args(["query", RUN_KEY, "/v", AUTOSTART_NAME])
            .output();
        matches!(out, Ok(out) if out.status.success())
    }
    #[cfg(not(target_os = "windows"))]
    launch_agent().map(|path| path.exists()).unwrap_or(false)
}

/// Bật/tắt tự khởi động. Trả lỗi dưới dạng câu tiếng Việt để hiện thẳng lên
/// giao diện.
pub fn set_autostart(on: bool) -> Result<(), String> {
    let exe =
        std::env::current_exe().map_err(|err| format!("không biết đường dẫn phần mềm: {err}"))?;

    #[cfg(target_os = "windows")]
    {
        let mut command = std::process::Command::new("reg");
        if on {
            // `--background` để nó vào thẳng chế độ chia sẻ với mật khẩu đã
            // lưu, cửa sổ thu nhỏ sẵn — không ai ngồi trước máy để bấm.
            command.args([
                "add",
                RUN_KEY,
                "/v",
                AUTOSTART_NAME,
                "/t",
                "REG_SZ",
                "/d",
                &autostart_command(&exe),
                "/f",
            ]);
        } else {
            command.args(["delete", RUN_KEY, "/v", AUTOSTART_NAME, "/f"]);
        }
        let out = command
            .output()
            .map_err(|err| format!("không gọi được reg: {err}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        let path = launch_agent().ok_or("không tìm được thư mục LaunchAgents")?;
        if !on {
            return match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(err) => Err(format!("không xoá được {}: {err}", path.display())),
            };
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|err| format!("không tạo được {}: {err}", dir.display()))?;
        }
        std::fs::write(&path, launch_agent_plist(&exe))
            .map_err(|err| format!("không ghi được {}: {err}", path.display()))
    }
}

/// Dòng lệnh registry cho lần đăng nhập sau.
///
/// Nháy kép bọc đường dẫn vì Windows tách tham số theo khoảng trắng, mà
/// "Program Files" hay tên người dùng có dấu cách là chuyện thường; thiếu nó
/// thì máy có điện lại sẽ chẳng có gì chạy lên. `--background` để phần mềm vào
/// thẳng chế độ chia sẻ với cửa sổ thu nhỏ — không ai ngồi trước máy để bấm.
#[cfg(any(target_os = "windows", test))]
fn autostart_command(exe: &Path) -> String {
    format!("\"{}\" --background", exe.display())
}

/// Nội dung plist cho LaunchAgent của macOS — bản đối chiếu của [`autostart_command`].
#[cfg(any(not(target_os = "windows"), test))]
fn launch_agent_plist(exe: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{AUTOSTART_NAME}</string>
  <key>ProgramArguments</key>
  <array><string>{}</string><string>--background</string></array>
  <key>RunAtLoad</key><true/>
</dict>
</plist>
"#,
        exe.display()
    )
}

#[cfg(not(target_os = "windows"))]
fn launch_agent() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    Some(home.join(format!("Library/LaunchAgents/{AUTOSTART_NAME}.plist")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Máy khác đang giữ khoá clipboard vài chục mili giây là chuyện thường
    /// trên Windows; hỏng nhịp đầu mà bỏ luôn thì đoạn văn bản mất hẳn.
    #[test]
    fn ghi_clipboard_thu_lai_khi_ban_khoa() {
        let mut lan = 0;
        let ket_qua = retry_set(
            || {
                lan += 1;
                if lan < 3 { Err("bận") } else { Ok(()) }
            },
            Duration::ZERO,
        );
        assert_eq!(ket_qua, Ok(()));
        assert_eq!(lan, 3, "phải thử lại tới lúc ghi được");
    }

    #[test]
    fn ghi_clipboard_bo_cuoc_sau_du_so_lan() {
        let mut lan = 0;
        let ket_qua: Result<(), &str> = retry_set(
            || {
                lan += 1;
                Err("bận")
            },
            Duration::ZERO,
        );
        assert_eq!(ket_qua, Err("bận"));
        assert_eq!(lan, SET_TRIES, "không được thử vô hạn");
    }

    /// Nhận từ máy kia rồi đọc lại chính nó thì phải im — không thì hai máy
    /// đẩy qua đẩy lại không dứt.
    #[test]
    fn khong_gui_nguoc_cai_vua_nhan() {
        let mut da_biet = DaBiet::default();
        assert!(da_biet.can_ghi("xin chào"));
        da_biet.ghi_nhan("xin chào".into());
        assert!(!da_biet.can_ghi("xin chào"));
        assert_eq!(da_biet.doc("xin chào".into()), Doc::Cu);
    }

    #[test]
    fn nguoi_dung_copy_thi_day_sang_may_kia() {
        let mut da_biet = DaBiet("cũ".into());
        assert_eq!(da_biet.doc("mới".into()), Doc::Moi("mới".into()));
        assert_eq!(da_biet.doc("mới".into()), Doc::Cu, "phải ghi nhận ngay");
    }

    /// Đoạn quá dài vẫn phải ghi nhận, không thì cứ 200 ms lại đo lại một lần.
    #[test]
    fn doan_qua_dai_ghi_nhan_nhung_khong_gui() {
        let dai = "a".repeat(MAX_CLIPBOARD_TEXT + 1);
        let mut da_biet = DaBiet::default();
        assert_eq!(da_biet.doc(dai.clone()), Doc::QuaDai(dai.len()));
        assert_eq!(da_biet.doc(dai), Doc::Cu);
    }

    /// Đường dẫn có dấu cách mà thiếu nháy kép thì Windows chạy nhầm
    /// `C:\Program`, và máy có điện lại sẽ không có gì lên.
    #[test]
    fn lenh_tu_khoi_dong_boc_nhay_kep_va_chay_ngam() {
        let lenh = autostart_command(Path::new(r"C:\Program Files\Remote Desktop\rd.exe"));
        assert_eq!(
            lenh,
            r#""C:\Program Files\Remote Desktop\rd.exe" --background"#
        );
    }

    #[test]
    fn plist_macos_cung_chay_ngam() {
        let plist = launch_agent_plist(Path::new("/Applications/Remote Desktop.app/rd"));
        assert!(plist.contains("<string>--background</string>"));
        assert!(plist.contains("<key>RunAtLoad</key><true/>"));
    }
}
