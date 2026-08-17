//! Truyền file trên uni stream QUIC.
//!
//! Mỗi lần truyền chiếm một uni stream riêng, còn phần mô tả (tên, kích thước,
//! hash) đi trên kênh control dưới dạng [`FileOffer`]. Tách làm hai như vậy vì
//! bên nhận phải được quyền từ chối *trước khi* một byte nội dung nào chạy sang,
//! và vì QUIC không chèn hàng giữa các stream — file nặng chạy nền không làm
//! khựng video như khi dồn chung một kết nối TCP.
//!
//! Trên stream chỉ có đúng 8 byte header là `transfer_id`, phần còn lại là nội
//! dung file thô cho tới khi stream đóng. Không cần đánh số chunk hay ack từng
//! đoạn: QUIC đã lo thứ tự, retransmit và flow control rồi. [`FileChunkAck`]
//! trên kênh control chỉ để bên gửi vẽ thanh tiến trình theo *bên nhận đã ghi
//! xuống đĩa*, chứ không tham gia vào việc đảm bảo dữ liệu.
//!
//! [`FileChunkAck`]: rd_protocol::FileChunkAck

use std::path::{Path, PathBuf};

use quinn::{RecvStream, SendStream};
use rd_protocol::FileOffer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{Session, TransportError};

/// Kích thước một lần đọc/ghi. Đủ lớn để không gọi syscall liên tục, đủ nhỏ để
/// thanh tiến trình nhúc nhích đều và bộ nhớ không phình theo số file song song.
pub const CHUNK: usize = 128 * 1024;

/// Giới hạn độ dài tên file. 255 là trần của hầu hết filesystem (APFS, NTFS,
/// ext4); dài hơn thì `create` sẽ lỗi ở tận lúc ghi nên chặn sớm cho rõ.
pub const MAX_FILE_NAME: usize = 255;

/// Header đứng đầu mỗi uni stream truyền file.
const HEADER_LEN: usize = 8;

/// Đọc file để dựng lời mời: lấy kích thước và băm BLAKE3 toàn bộ nội dung.
///
/// Băm trước khi gửi nghĩa là đọc file hai lượt. Đổi lại bên nhận biết được
/// kết quả đúng hay sai ngay khi nhận xong, chứ không phải tin vào lời hứa của
/// bên gửi. BLAKE3 chạy nhanh hơn tốc độ đọc đĩa nên lượt băm gần như chỉ tốn
/// đúng thời gian I/O.
pub async fn prepare_offer(path: &Path, transfer_id: u64) -> Result<FileOffer, TransportError> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| TransportError::BadFileName(path.display().to_string()))?
        .to_string();
    if name.len() > MAX_FILE_NAME {
        return Err(TransportError::BadFileName(name));
    }

    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; CHUNK];
    let mut size = 0u64;
    loop {
        let read = file.read(&mut buf).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        size += read as u64;
    }

    Ok(FileOffer {
        transfer_id,
        name,
        size,
        hash: *hasher.finalize().as_bytes(),
    })
}

/// Gửi nội dung file. Gọi sau khi đầu kia đã trả lời `FileAccept`.
///
/// `progress` nhận số byte đã đẩy vào stream — đó là tiến độ *gửi*, luôn chạy
/// trước tiến độ *nhận* một quãng bằng cửa sổ nghẽn.
pub async fn send_file<F>(
    session: &Session,
    offer: &FileOffer,
    path: &Path,
    mut progress: F,
) -> Result<(), TransportError>
where
    F: FnMut(u64),
{
    let mut file = tokio::fs::File::open(path).await?;
    let mut stream = session.open_uni().await?;
    // Xếp sau kênh điều khiển. Không có dòng này thì QUIC chia đều băng thông
    // giữa các stream, nên đang gửi một file 4 GB là chuột phím phải chen chân
    // với nó — mà chuột trễ nửa giây thì người dùng thấy ngay, còn file chậm
    // thêm vài phần trăm thì không ai để ý.
    stream.set_priority(-1).ok();
    stream.write_all(&offer.transfer_id.to_le_bytes()).await?;

    let mut buf = vec![0u8; CHUNK];
    let mut sent = 0u64;
    loop {
        let read = file.read(&mut buf).await?;
        if read == 0 {
            break;
        }
        stream.write_all(&buf[..read]).await?;
        sent += read as u64;
        progress(sent);
    }

    if sent != offer.size {
        // File bị sửa hoặc bị cắt trong lúc đang gửi. Reset stream để bên nhận
        // thấy lỗi chứ không tưởng là đã nhận đủ.
        stream.reset(0u32.into()).ok();
        return Err(TransportError::FileSizeMismatch {
            name: offer.name.clone(),
            got: sent,
            want: offer.size,
        });
    }

    finish_and_flush(&mut stream).await
}

/// Đóng stream rồi chờ đầu kia xác nhận đã nhận hết.
///
/// `finish` chỉ đánh dấu "hết dữ liệu"; lúc nó trả về thì phần lớn nội dung vẫn
/// còn nằm trong bộ đệm gửi. Báo "xong" ngay tại đó là nói dối người dùng, và
/// nếu chương trình thoát luôn thì file sang bên kia bị cụt.
async fn finish_and_flush(stream: &mut SendStream) -> Result<(), TransportError> {
    stream.finish()?;
    match stream.stopped().await {
        Ok(None) => Ok(()),
        // Bên nhận chủ động huỷ giữa chừng (bấm Cancel chẳng hạn).
        Ok(Some(code)) => Err(TransportError::FileRejected(code.into_inner())),
        Err(err) => Err(TransportError::Stopped(err)),
    }
}

/// Đọc `transfer_id` ở đầu một uni stream vừa nhận, để biết stream này ứng với
/// lời mời nào.
pub async fn read_transfer_id(stream: &mut RecvStream) -> Result<u64, TransportError> {
    let mut header = [0u8; HEADER_LEN];
    stream.read_exact(&mut header).await?;
    Ok(u64::from_le_bytes(header))
}

/// Nhận nội dung file vào `dest_dir` và trả về đường dẫn đã ghi.
///
/// Ghi vào tên tạm rồi mới đổi tên: nửa chừng đứt mạng thì thư mục đích không
/// có file mang tên thật nhưng nội dung cụt — thứ người dùng sẽ mở ra rồi tưởng
/// là file hỏng.
///
/// `stream` phải đã được [`read_transfer_id`] đọc qua header.
pub async fn recv_file<F>(
    mut stream: RecvStream,
    offer: &FileOffer,
    dest_dir: &Path,
    mut progress: F,
) -> Result<PathBuf, TransportError>
where
    F: FnMut(u64),
{
    let name = safe_file_name(&offer.name)
        .ok_or_else(|| TransportError::BadFileName(offer.name.clone()))?;
    tokio::fs::create_dir_all(dest_dir).await?;

    let partial = dest_dir.join(format!(".rd-partial-{:016x}", offer.transfer_id));
    let mut file = tokio::fs::File::create(&partial).await?;

    let result = drain(&mut stream, &mut file, offer, &mut progress).await;
    // Đóng file trước khi rename hoặc xoá: Windows không cho đụng vào file đang
    // còn handle mở.
    let flushed = file.shutdown().await.map_err(TransportError::from);
    drop(file);

    if let Err(err) = result.and(flushed) {
        tokio::fs::remove_file(&partial).await.ok();
        return Err(err);
    }

    let final_path = unique_path(dest_dir, &name).await;
    tokio::fs::rename(&partial, &final_path).await?;
    Ok(final_path)
}

/// Bơm dữ liệu từ stream xuống file, vừa bơm vừa băm và đếm.
async fn drain<F>(
    stream: &mut RecvStream,
    file: &mut tokio::fs::File,
    offer: &FileOffer,
    progress: &mut F,
) -> Result<(), TransportError>
where
    F: FnMut(u64),
{
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; CHUNK];
    let mut written = 0u64;

    while let Some(read) = stream.read(&mut buf).await? {
        if read == 0 {
            continue;
        }
        written += read as u64;
        // Chặn ngay khi vượt: bên gửi hứa `size` byte, gửi nhiều hơn thì hoặc
        // là lỗi hoặc là cố tình làm đầy đĩa bên nhận.
        if written > offer.size {
            return Err(TransportError::FileSizeMismatch {
                name: offer.name.clone(),
                got: written,
                want: offer.size,
            });
        }
        hasher.update(&buf[..read]);
        file.write_all(&buf[..read]).await?;
        progress(written);
    }

    if written != offer.size {
        return Err(TransportError::FileSizeMismatch {
            name: offer.name.clone(),
            got: written,
            want: offer.size,
        });
    }
    if hasher.finalize().as_bytes() != &offer.hash {
        return Err(TransportError::FileHashMismatch(offer.name.clone()));
    }
    Ok(())
}

/// Rút tên file an toàn từ tên do đầu kia gửi sang.
///
/// Tên này là dữ liệu của đầu kia, không phải của ta: `../../.ssh/authorized_keys`
/// mà đem `join` thẳng vào thư mục tải về là ghi đè được file ngoài thư mục đó.
/// Nên chỉ giữ đúng phần tên cuối cùng và loại mọi thứ có thể đổi nghĩa đường
/// dẫn trên bất kỳ hệ nào.
pub fn safe_file_name(raw: &str) -> Option<String> {
    // Cắt theo cả hai dấu phân cách: bên gửi có thể là Windows còn bên nhận là
    // macOS, mà `Path::file_name` chỉ hiểu dấu phân cách của hệ mình.
    let name = raw.rsplit(['/', '\\']).next()?;
    // Windows còn tách ổ đĩa bằng dấu hai chấm, và "C:evil" trỏ tới thư mục
    // hiện hành của ổ C chứ không phải một file tên "C:evil".
    let name = name.rsplit(':').next()?;
    // Windows lặng lẽ cắt dấu chấm và khoảng trắng ở cuối, nên "abc.txt. " và
    // "abc.txt" là cùng một file — cắt sẵn để tên ta thấy đúng là tên tạo ra.
    let name = name.trim_end_matches(['.', ' ']).trim_start();
    if name.is_empty() || name.len() > MAX_FILE_NAME {
        return None;
    }
    // Ký tự điều khiển không hợp lệ trên NTFS và làm rối terminal khi in ra.
    if name.chars().any(char::is_control) {
        return None;
    }

    // Tên thiết bị của Windows: mở "CON" ra là mở console chứ không phải file.
    // Thêm gạch dưới thay vì từ chối, để file tên "aux.txt" của người dùng
    // macOS vẫn nhận được.
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        return Some(format!("_{name}"));
    }
    Some(name.to_string())
}

/// Tìm tên chưa bị chiếm trong thư mục đích: `bao-cao.pdf` → `bao-cao (1).pdf`.
///
/// Không có khoá nào ở đây nên hai lần truyền cùng tên chạy song song vẫn có
/// thể chọn trùng; đổi lại là không phải giữ trạng thái toàn cục. Trường hợp đó
/// hiếm và hậu quả chỉ là một file bị ghi đè bởi file kia, nên chấp nhận được.
async fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !tokio::fs::try_exists(&candidate).await.unwrap_or(false) {
        return candidate;
    }

    let path = Path::new(name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let extension = path.extension().and_then(|s| s.to_str());

    for index in 1..1000u32 {
        let attempt = match extension {
            Some(ext) => format!("{stem} ({index}).{ext}"),
            None => format!("{stem} ({index})"),
        };
        let candidate = dir.join(attempt);
        if !tokio::fs::try_exists(&candidate).await.unwrap_or(false) {
            return candidate;
        }
    }
    // Ngàn cái trùng tên thì thôi ghi đè cái cuối, còn hơn treo vòng lặp.
    dir.join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_directory_from_both_platforms() {
        assert_eq!(safe_file_name("/etc/passwd").unwrap(), "passwd");
        assert_eq!(safe_file_name(r"C:\Windows\system32\cmd.exe").unwrap(), "cmd.exe");
        assert_eq!(safe_file_name("../../.ssh/authorized_keys").unwrap(), "authorized_keys");
        assert_eq!(safe_file_name(r"..\..\evil.dll").unwrap(), "evil.dll");
    }

    #[test]
    fn rejects_names_that_are_not_files() {
        assert!(safe_file_name("").is_none());
        assert!(safe_file_name("..").is_none());
        assert!(safe_file_name(".").is_none());
        assert!(safe_file_name("/").is_none());
        assert!(safe_file_name("bad\0name").is_none());
        assert!(safe_file_name("bad\nname").is_none());
        assert!(safe_file_name(&"a".repeat(MAX_FILE_NAME + 1)).is_none());
    }

    #[test]
    fn defuses_windows_device_names() {
        assert_eq!(safe_file_name("CON").unwrap(), "_CON");
        assert_eq!(safe_file_name("nul.txt").unwrap(), "_nul.txt");
        assert_eq!(safe_file_name("com1.log").unwrap(), "_com1.log");
        // Tên chỉ *bắt đầu* bằng tên thiết bị thì vẫn bình thường.
        assert_eq!(safe_file_name("console.log").unwrap(), "console.log");
    }

    #[test]
    fn trims_trailing_dots_and_spaces() {
        assert_eq!(safe_file_name("bao-cao.pdf. ").unwrap(), "bao-cao.pdf");
        assert!(safe_file_name("...").is_none());
    }

    #[tokio::test]
    async fn unique_path_avoids_overwriting() {
        let dir = std::env::temp_dir().join(format!("rd-unique-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let first = unique_path(&dir, "bao-cao.pdf").await;
        assert_eq!(first.file_name().unwrap(), "bao-cao.pdf");
        tokio::fs::write(&first, b"x").await.unwrap();

        let second = unique_path(&dir, "bao-cao.pdf").await;
        assert_eq!(second.file_name().unwrap(), "bao-cao (1).pdf");
        tokio::fs::write(&second, b"x").await.unwrap();

        let third = unique_path(&dir, "bao-cao.pdf").await;
        assert_eq!(third.file_name().unwrap(), "bao-cao (2).pdf");

        // File không có phần mở rộng cũng phải đánh số được.
        let plain = unique_path(&dir, "README").await;
        tokio::fs::write(&plain, b"x").await.unwrap();
        let plain_again = unique_path(&dir, "README").await;
        assert_eq!(plain_again.file_name().unwrap(), "README (1)");

        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }
}
