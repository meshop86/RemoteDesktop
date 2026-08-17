//! Truyền file end-to-end trên loopback: toàn vẹn nội dung, chống tên file độc,
//! và kiểm chứng lời hứa "file nặng không làm khựng video".

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use rd_protocol::{
    FileOffer,
    control::{Codec, HostEvent, ViewerCommand},
};
use rd_transport::{
    ControlReceiver, ControlSender, Session, VideoReceiver, VideoSender, client_endpoint,
    generate_self_signed, now_us, quinn, server_endpoint,
};

/// Thư mục tạm riêng cho từng test, tự dọn ở cuối.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "rd-file-{tag}-{}-{}",
            std::process::id(),
            now_us()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// Nội dung giả nhưng không nén được (mọi byte đều khác nhau theo vị trí), để
/// throughput đo được là throughput thật chứ không phải nhờ dữ liệu toàn số 0.
fn make_content(size: usize) -> Vec<u8> {
    (0..size)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect()
}

fn write_source(dir: &Path, name: &str, size: usize) -> (PathBuf, Vec<u8>) {
    let content = make_content(size);
    let path = dir.join(name);
    std::fs::write(&path, &content).unwrap();
    (path, content)
}

/// Một cặp host/viewer đã nối QUIC.
struct Link {
    host: Session,
    viewer: Session,
    // Giữ endpoint sống suốt test: thả ra là socket đóng và kết nối đứt.
    _server: quinn::Endpoint,
    _client: quinn::Endpoint,
}

async fn connect() -> Link {
    let identity = generate_self_signed("rd-host").unwrap();
    let fingerprint = identity.fingerprint;
    let server = server_endpoint("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
    let addr = server.local_addr().unwrap();

    let listening = server.clone();
    let accept = tokio::spawn(async move { listening.accept().await.unwrap().await.unwrap() });

    let client = client_endpoint("127.0.0.1:0".parse().unwrap(), Some(fingerprint)).unwrap();
    let conn = client.connect(addr, "rd-host").unwrap().await.unwrap();

    Link {
        host: Session::new(accept.await.unwrap()),
        viewer: Session::new(conn),
        _server: server,
        _client: client,
    }
}

type HostControl = (ControlSender<HostEvent>, ControlReceiver<ViewerCommand>);
type ViewerControl = (ControlSender<ViewerCommand>, ControlReceiver<HostEvent>);

/// Mở kênh control hai chiều rồi bắt tay.
///
/// QUIC tạo stream lười: `open_bi` bên viewer trả về ngay tại chỗ, nhưng host
/// chỉ *thấy* stream đó khi byte đầu tiên bay sang. Nên viewer phải gửi Hello
/// ngay, và hai bên phải chạy song song — làm tuần tự thì `accept_bi` chờ mãi
/// một stream mà lượt gọi sau mới mở.
async fn open_control(link: &Link) -> (HostControl, ViewerControl) {
    let viewer_side = async {
        let (mut tx, rx) = link
            .viewer
            .open_control::<ViewerCommand, HostEvent>()
            .await
            .unwrap();
        tx.send(&ViewerCommand::Hello {
            version: rd_protocol::PROTOCOL_VERSION,
            viewer_name: "test-viewer".into(),
            auth: [0u8; 32],
            wants_10bit: false,
            codecs: vec![rd_protocol::control::Codec::H264],
        })
        .await
        .unwrap();
        (tx, rx)
    };
    let host_side = async {
        link.host
            .accept_control::<HostEvent, ViewerCommand>()
            .await
            .unwrap()
    };

    let (mut host, viewer) = tokio::join!(host_side, viewer_side);
    assert!(matches!(
        host.1.recv().await.unwrap(),
        ViewerCommand::Hello { .. }
    ));
    (host, viewer)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn file_arrives_intact_and_fast() {
    const SIZE: usize = 16 * 1024 * 1024;

    let src_dir = TempDir::new("src");
    let dst_dir = TempDir::new("dst");
    let (src_path, content) = write_source(src_dir.path(), "bao-cao.pdf", SIZE);

    let link = connect().await;
    // Host mô tả file rồi mời; viewer đồng ý; host mới bắt đầu đẩy nội dung.
    let ((mut host_tx, mut host_rx), (mut viewer_tx, mut viewer_rx)) = open_control(&link).await;

    let offer = rd_transport::prepare_offer(&src_path, 7).await.unwrap();
    assert_eq!(offer.size, SIZE as u64);
    assert_eq!(offer.name, "bao-cao.pdf");

    let sent_bytes = Arc::new(AtomicU64::new(0));
    let sent_probe = sent_bytes.clone();
    let send_offer = offer.clone();
    let host = link.host.clone();
    let sender = tokio::spawn(async move {
        host_tx
            .send(&HostEvent::FileOffer(send_offer.clone()))
            .await
            .unwrap();
        match host_rx.recv().await.unwrap() {
            ViewerCommand::FileAccept { transfer_id } => assert_eq!(transfer_id, 7),
            other => panic!("mong đợi FileAccept, nhận {other:?}"),
        }
        let started = std::time::Instant::now();
        rd_transport::send_file(&host, &send_offer, &src_path, |done| {
            sent_probe.store(done, Ordering::Relaxed);
        })
        .await
        .unwrap();
        started.elapsed()
    });

    let offer_received = match viewer_rx.recv().await.unwrap() {
        HostEvent::FileOffer(offer) => offer,
        other => panic!("mong đợi FileOffer, nhận {other:?}"),
    };
    assert_eq!(offer_received, offer);
    viewer_tx
        .send(&ViewerCommand::FileAccept { transfer_id: 7 })
        .await
        .unwrap();

    let mut stream = link.viewer.accept_uni().await.unwrap();
    assert_eq!(rd_transport::read_transfer_id(&mut stream).await.unwrap(), 7);

    let mut progress_steps = 0u32;
    let written = rd_transport::recv_file(stream, &offer_received, dst_dir.path(), |_| {
        progress_steps += 1;
    })
    .await
    .unwrap();

    let elapsed = sender.await.unwrap();

    assert_eq!(written.file_name().unwrap(), "bao-cao.pdf");
    assert_eq!(std::fs::read(&written).unwrap(), content);
    assert_eq!(sent_bytes.load(Ordering::Relaxed), SIZE as u64);
    assert!(progress_steps > 1, "thanh tiến trình phải nhúc nhích nhiều lần");

    let mbps = SIZE as f64 / elapsed.as_secs_f64() / 1_000_000.0;
    println!("gửi {SIZE} byte mất {elapsed:?} — {mbps:.0} MB/s trên loopback");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupted_content_is_caught_and_leaves_no_file() {
    let src_dir = TempDir::new("bad-src");
    let dst_dir = TempDir::new("bad-dst");
    let (src_path, _) = write_source(src_dir.path(), "anh.png", 256 * 1024);

    let link = connect().await;
    let mut offer = rd_transport::prepare_offer(&src_path, 1).await.unwrap();
    let honest = offer.clone();
    // Đổi một bit trong hash: đúng thứ xảy ra khi đường truyền làm hỏng dữ liệu.
    offer.hash[0] ^= 1;

    let host = link.host.clone();
    let sender =
        tokio::spawn(async move { rd_transport::send_file(&host, &honest, &src_path, |_| {}).await });

    let mut stream = link.viewer.accept_uni().await.unwrap();
    rd_transport::read_transfer_id(&mut stream).await.unwrap();
    let err = rd_transport::recv_file(stream, &offer, dst_dir.path(), |_| {})
        .await
        .expect_err("hash sai thì phải báo lỗi");
    assert!(
        matches!(err, rd_transport::TransportError::FileHashMismatch(_)),
        "lỗi sai loại: {err}"
    );

    sender.await.unwrap().ok();

    let leftovers: Vec<_> = std::fs::read_dir(dst_dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(
        leftovers.is_empty(),
        "hỏng rồi mà vẫn để lại file: {leftovers:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hostile_file_name_cannot_escape_download_dir() {
    let src_dir = TempDir::new("evil-src");
    let dst_dir = TempDir::new("evil-dst");
    let (src_path, content) = write_source(src_dir.path(), "payload.bin", 4096);

    let link = connect().await;
    let real = rd_transport::prepare_offer(&src_path, 2).await.unwrap();
    // Bên gửi tự đặt tên, nên tên là dữ liệu không tin được.
    let hostile = FileOffer {
        name: "../../../../../../tmp/rd-escaped.txt".into(),
        ..real.clone()
    };

    let host = link.host.clone();
    let sender =
        tokio::spawn(async move { rd_transport::send_file(&host, &real, &src_path, |_| {}).await });

    let mut stream = link.viewer.accept_uni().await.unwrap();
    rd_transport::read_transfer_id(&mut stream).await.unwrap();
    let written = rd_transport::recv_file(stream, &hostile, dst_dir.path(), |_| {})
        .await
        .unwrap();
    sender.await.unwrap().unwrap();

    assert_eq!(written.parent().unwrap(), dst_dir.path());
    assert_eq!(written.file_name().unwrap(), "rd-escaped.txt");
    assert_eq!(std::fs::read(&written).unwrap(), content);
}

const FRAMES: u32 = 200;
const FRAME_SIZE: usize = 30_000;

/// Kết quả một đợt phát video.
struct Burst {
    received: u32,
    p50_us: u64,
    p99_us: u64,
}

impl std::fmt::Display for Burst {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}/{FRAMES} frame, p50 {:.2}ms p99 {:.2}ms",
            self.received,
            self.p50_us as f64 / 1000.0,
            self.p99_us as f64 / 1000.0,
        )
    }
}

/// Bắn `FRAMES` frame qua kết nối rồi đếm và đo bên nhận.
///
/// Frame video đi bằng datagram, mà datagram thì không được gửi lại: nghẽn
/// đường là rớt luôn. Vậy nên con số đếm được ở đây phụ thuộc vào sức máy, và
/// chỉ có ý nghĩa khi đem so với một đợt khác chạy trên cùng cái máy đó.
async fn video_burst(link: &Link) -> Burst {
    let video_host = link.host.clone();
    let video_task = tokio::spawn(async move {
        let mut sender = VideoSender::new(video_host, 0);
        let mut ticker = tokio::time::interval(Duration::from_millis(2));
        // Máy chậm mà trễ nhịp thì gửi bù dồn một cục, làm tràn hàng đợi
        // datagram và tự tay đánh rớt frame của chính mình.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        for index in 0..FRAMES {
            ticker.tick().await;
            let frame = vec![(index % 251) as u8; FRAME_SIZE];
            sender
                .send_frame(&frame, Codec::Hevc, index % 60 == 0, now_us())
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    });

    let mut receiver = VideoReceiver::new(link.viewer.clone(), 8);
    let mut latencies_us = Vec::new();
    // Hạn rộng chỉ để máy CI chậm không bị cắt ngang giữa chừng rồi báo là mất
    // frame; máy thật xong cả đợt trong khoảng một giây.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while (latencies_us.len() as u32) < FRAMES {
        match tokio::time::timeout_at(deadline, receiver.next_frame()).await {
            Ok(Ok(frame)) => latencies_us.push(now_us().saturating_sub(frame.capture_us)),
            _ => break,
        }
    }
    video_task.await.unwrap();

    assert!(!latencies_us.is_empty(), "không nhận được frame nào");
    latencies_us.sort_unstable();
    Burst {
        received: latencies_us.len() as u32,
        p50_us: latencies_us[latencies_us.len() / 2],
        p99_us: latencies_us[latencies_us.len() * 99 / 100],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn big_transfer_does_not_stall_video() {
    // File đủ lớn để chiếm đường truyền suốt cả đợt video, nhưng không lớn hơn
    // mức cần thiết: test chạy ở bản debug, mà mã hoá QUIC ở bản debug trên
    // máy CI hai nhân chậm hơn máy thật vài chục lần.
    const FILE_SIZE: usize = 16 * 1024 * 1024;

    // Đợt đối chứng: chỉ có video, không có gì tranh đường. Không có mốc này thì
    // test đang đo sức của cái máy chứ không đo ảnh hưởng của việc truyền file —
    // và mỗi lần máy CI chậm đi là một lần báo đỏ oan.
    let baseline = video_burst(&connect().await).await;

    let src_dir = TempDir::new("mix-src");
    let dst_dir = TempDir::new("mix-dst");
    let (src_path, _) = write_source(src_dir.path(), "iso.img", FILE_SIZE);

    let link = connect().await;
    let offer = rd_transport::prepare_offer(&src_path, 3).await.unwrap();

    // File chạy nền, video chạy song song trên cùng một kết nối.
    let file_host = link.host.clone();
    let file_offer = offer.clone();
    let file_task = tokio::spawn(async move {
        rd_transport::send_file(&file_host, &file_offer, &src_path, |_| {}).await
    });

    let recv_viewer = link.viewer.clone();
    let dst = dst_dir.path().to_path_buf();
    let recv_task = tokio::spawn(async move {
        let mut stream = recv_viewer.accept_uni().await.unwrap();
        rd_transport::read_transfer_id(&mut stream).await.unwrap();
        rd_transport::recv_file(stream, &offer, &dst, |_| {}).await
    });

    let mixed = video_burst(&link).await;

    file_task.await.unwrap().unwrap();
    let written = recv_task.await.unwrap().unwrap();
    // Giữ thư mục sống tới đây rồi mới cho `Drop` dọn.
    assert_eq!(std::fs::metadata(&written).unwrap().len(), FILE_SIZE as u64);

    println!("video một mình:  {baseline}");
    println!("video + {} MB file: {mixed}", FILE_SIZE / 1_000_000);

    // Ngưỡng đặt ở 60% chứ không phải 90%: video và file dùng chung một cửa sổ
    // nghẽn, nên tải file nặng thì rớt thêm ít frame là đúng thiết kế — thà rớt
    // frame cũ còn hơn xếp hàng chờ. Cái phải chặn là *khựng hẳn*.
    assert!(
        mixed.received as f32 >= baseline.received as f32 * 0.6,
        "file nặng làm mất quá nhiều frame: {mixed} so với {baseline} khi chạy một mình"
    );
    // Đây mới là lời hứa của kiến trúc: file đi bằng stream tin cậy, video đi
    // bằng datagram, nên gói file mất và phải gửi lại *không* được kéo theo
    // video chờ cùng. Sàn 20ms để máy nhanh không bị so bằng con số nhiễu.
    let cho_phep = baseline.p99_us.max(20_000) * 4;
    assert!(
        mixed.p99_us <= cho_phep,
        "file nặng đẩy độ trễ video lên {}us (cho phép {cho_phep}us) — head-of-line blocking",
        mixed.p99_us
    );
}
