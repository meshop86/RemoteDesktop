//! Ba việc phải nhờ hệ điều hành làm: đọc/ghi clipboard, mở hộp thoại chọn
//! tệp, và mở thư mục chứa tệp vừa nhận.
//!
//! Gom chung vì cùng một kiểu rắc rối: cái nào cũng có thể chặn luồng gọi vô
//! thời hạn, mà luồng gọi ở đây là luồng vẽ giao diện — chặn nó một giây là
//! cửa sổ đứng hình một giây. Nên tất cả đều chạy ở luồng khác và nói chuyện
//! với giao diện qua kênh.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use rd_protocol::MAX_CLIPBOARD_TEXT;

/// Bao lâu ngó clipboard một lần.
///
/// Không hệ nào cho đăng ký nhận thông báo khi clipboard đổi theo cách dùng
/// được từ Rust, nên đành hỏi vòng vòng. 400 ms là quãng người dùng copy xong,
/// chuyển cửa sổ rồi mới dán — tới lúc đó nội dung đã sang tới máy kia.
const POLL: Duration = Duration::from_millis(400);

/// Cầu nối clipboard giữa hai máy.
///
/// Một luồng nền giữ `arboard::Clipboard` và làm cả hai chiều. Phải là luồng
/// nền vì trên Windows việc đọc clipboard là mở một khoá toàn máy — ứng dụng
/// khác đang giữ khoá đó thì lời gọi nằm chờ.
pub struct Clipboard {
    /// Người dùng vừa copy gì đó ở máy này.
    changed: mpsc::Receiver<String>,
    /// Máy kia vừa copy: bảo luồng nền ghi đè clipboard máy này.
    to_set: mpsc::Sender<String>,
}

impl Clipboard {
    /// Dựng cầu nối và luồng nền của nó.
    ///
    /// Thả giá trị này là luồng nền tự dừng — nhờ vậy tắt đồng bộ clipboard
    /// cũng là ngừng đọc clipboard thật, chứ không phải vẫn đọc rồi vứt đi.
    pub fn start() -> Self {
        let (changed_tx, changed) = mpsc::channel();
        let (to_set, set_rx) = mpsc::channel();
        // Dựng `arboard::Clipboard` *bên trong* luồng chứ không dựng ở đây rồi
        // chuyển sang: nó gắn với trạng thái của luồng tạo ra nó.
        let spawned = std::thread::Builder::new()
            .name("rd-clipboard".into())
            .spawn(move || run(changed_tx, set_rx));
        if let Err(err) = spawned {
            tracing::warn!(%err, "không tạo được luồng clipboard");
        }
        Self { changed, to_set }
    }

    /// Văn bản người dùng vừa copy ở máy này, nếu có.
    pub fn take_local_change(&self) -> Option<String> {
        self.changed.try_recv().ok()
    }

    /// Máy kia vừa copy: ghi đè clipboard máy này.
    pub fn set(&self, text: String) {
        let _ = self.to_set.send(text);
    }
}

fn run(changed: mpsc::Sender<String>, to_set: mpsc::Receiver<String>) {
    let mut clipboard = match arboard::Clipboard::new() {
        Ok(clipboard) => clipboard,
        Err(err) => {
            tracing::warn!(%err, "không mở được clipboard hệ thống");
            return;
        }
    };

    // Nội dung mới nhất ta biết, dù do người dùng copy hay do chính ta vừa ghi
    // vào. Không có mốc này thì mỗi lần nhận từ máy kia ta lại thấy clipboard
    // "vừa đổi" và gửi ngược lại — hai máy đẩy qua đẩy lại không dứt.
    let mut last = clipboard.get_text().unwrap_or_default();

    loop {
        match to_set.recv_timeout(POLL) {
            Ok(text) => {
                // Dồn lấy cái mới nhất: dán lần lượt từng cái cũ chỉ làm
                // clipboard nhấp nháy qua mấy giá trị không ai cần.
                let mut text = text;
                while let Ok(newer) = to_set.try_recv() {
                    text = newer;
                }
                if text != last {
                    match clipboard.set_text(text.clone()) {
                        Ok(()) => last = text,
                        Err(err) => tracing::debug!(%err, "không ghi được clipboard"),
                    }
                }
                continue;
            }
            Err(RecvTimeoutError::Timeout) => {}
            // Giao diện đã thả cầu nối: hết việc.
            Err(RecvTimeoutError::Disconnected) => break,
        }

        match clipboard.get_text() {
            Ok(text) if text == last => {}
            Ok(text) => {
                // Ghi nhận trước khi lọc độ dài: đoạn quá dài mà không ghi nhận
                // thì vòng sau lại thấy nó "vừa đổi", cứ 400 ms một lần.
                last = text.clone();
                if text.len() > MAX_CLIPBOARD_TEXT {
                    tracing::debug!(len = text.len(), "clipboard quá dài, không đồng bộ");
                } else if changed.send(text).is_err() {
                    break;
                }
            }
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
