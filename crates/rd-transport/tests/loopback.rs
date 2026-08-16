//! Kiểm tra đường video end-to-end trên loopback: toàn vẹn dữ liệu, tỉ lệ mất
//! frame, và độ trễ thực đo được của riêng tầng transport.

use std::time::Duration;

use rd_protocol::{
    control::{Codec, HostEvent, ViewerCommand},
};
use rd_transport::{
    ClockSync, Session, VideoReceiver, VideoSender, client_endpoint, generate_self_signed, now_us,
    server_endpoint,
};

const FRAME_COUNT: u32 = 300;
const FRAME_SIZE: usize = 30_000; // xấp xỉ frame 1080p ở 15 Mbps @60fps

fn make_frame(index: u32) -> Vec<u8> {
    let mut data = vec![0u8; FRAME_SIZE];
    data[0..4].copy_from_slice(&index.to_le_bytes());
    for (i, byte) in data.iter_mut().enumerate().skip(4) {
        *byte = ((i as u32).wrapping_mul(index.wrapping_add(1)) % 251) as u8;
    }
    data
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn video_over_loopback_keeps_frames_intact_and_fast() {
    let identity = generate_self_signed("rd-host").unwrap();
    let fingerprint = identity.fingerprint;
    let server = server_endpoint("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
    let server_addr = server.local_addr().unwrap();

    let host = tokio::spawn(async move {
        let conn = server.accept().await.expect("có kết nối tới").await.unwrap();
        let session = Session::new(conn);
        let (mut tx, mut rx) = session
            .accept_control::<HostEvent, ViewerCommand>()
            .await
            .unwrap();

        // Bắt tay + trả lời ping để viewer đồng bộ đồng hồ.
        match rx.recv().await.unwrap() {
            ViewerCommand::Hello { version, .. } => assert_eq!(version, rd_protocol::PROTOCOL_VERSION),
            other => panic!("mong đợi Hello, nhận {other:?}"),
        }
        tx.send(&HostEvent::Welcome {
            version: rd_protocol::PROTOCOL_VERSION,
            host_name: "test-host".into(),
            monitors: vec![],
            active_monitor: 0,
        })
        .await
        .unwrap();

        let ping_session = session.clone();
        tokio::spawn(async move {
            let _ = ping_session;
            while let Ok(cmd) = rx.recv().await {
                if let ViewerCommand::Ping { sent_us } = cmd {
                    let _ = tx
                        .send(&HostEvent::Pong {
                            sent_us,
                            host_us: now_us(),
                        })
                        .await;
                }
            }
        });

        let mut sender = VideoSender::new(session.clone(), 0);
        let mut ticker = tokio::time::interval(Duration::from_micros(1500));
        let mut sent_packets = 0usize;
        for index in 0..FRAME_COUNT {
            ticker.tick().await;
            let frame = make_frame(index);
            sent_packets += sender
                .send_frame(&frame, Codec::Hevc, index % 60 == 0, now_us())
                .unwrap();
        }
        // Giữ kết nối đủ lâu để các datagram cuối kịp bay sang.
        tokio::time::sleep(Duration::from_millis(300)).await;
        (sent_packets, sender.dropped_frames())
    });

    let client = client_endpoint("127.0.0.1:0".parse().unwrap(), Some(fingerprint)).unwrap();
    let conn = client.connect(server_addr, "rd-host").unwrap().await.unwrap();
    let session = Session::new(conn);
    let (mut tx, mut rx) = session
        .open_control::<ViewerCommand, HostEvent>()
        .await
        .unwrap();
    tx.send(&ViewerCommand::Hello {
        version: rd_protocol::PROTOCOL_VERSION,
        viewer_name: "test-viewer".into(),
        auth: [0u8; 32],
        wants_10bit: false,
    })
    .await
    .unwrap();
    assert!(matches!(rx.recv().await.unwrap(), HostEvent::Welcome { .. }));

    // Đồng bộ đồng hồ trước khi đo (cùng máy nên offset ~0, nhưng vẫn chạy
    // đúng luồng như thật).
    let mut clock = ClockSync::new();
    for _ in 0..5 {
        let sent = now_us();
        tx.send(&ViewerCommand::Ping { sent_us: sent }).await.unwrap();
        if let HostEvent::Pong { sent_us, host_us } = rx.recv().await.unwrap() {
            clock.on_pong(sent_us, host_us, now_us());
        }
    }
    assert!(clock.rtt_us().is_some(), "phải có ít nhất một mẫu RTT");

    let mut receiver = VideoReceiver::new(session.clone(), 4);
    let mut latencies_us: Vec<i64> = Vec::with_capacity(FRAME_COUNT as usize);
    let mut received = 0u32;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while received < FRAME_COUNT {
        let frame = match tokio::time::timeout_at(deadline, receiver.next_frame()).await {
            Ok(Ok(frame)) => frame,
            _ => break,
        };
        let index = u32::from_le_bytes(frame.data[0..4].try_into().unwrap());
        assert_eq!(
            frame.data,
            make_frame(index),
            "frame {index} sai nội dung sau khi ghép"
        );
        assert_eq!(frame.data.len(), FRAME_SIZE);
        if let Some(latency) = clock.latency_us(frame.capture_us, now_us()) {
            latencies_us.push(latency);
        }
        received += 1;
    }

    let (sent_packets, dropped) = host.await.unwrap();
    let stats = receiver.stats();
    let link = session.link_stats();

    latencies_us.sort_unstable();
    let p50 = latencies_us[latencies_us.len() / 2];
    let p99 = latencies_us[latencies_us.len() * 99 / 100];
    println!(
        "nhận {received}/{FRAME_COUNT} frame | gói gửi {sent_packets} | frame bỏ khi gửi {dropped}\n\
         ghép: hoàn chỉnh {} hỏng {} gói mất {}\n\
         latency transport: p50 {:.2}ms  p99 {:.2}ms  |  RTT quinn {:.2}ms  MTU {}",
        stats.frames_completed,
        stats.frames_dropped,
        stats.packets_lost,
        p50 as f64 / 1000.0,
        p99 as f64 / 1000.0,
        link.rtt.as_secs_f64() * 1000.0,
        link.current_mtu,
    );

    assert!(
        received as f32 >= FRAME_COUNT as f32 * 0.9,
        "mất quá nhiều frame: chỉ nhận {received}/{FRAME_COUNT}"
    );
    assert!(
        p50 < 50_000,
        "độ trễ trung vị {p50}us quá cao cho loopback"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_channel_carries_input_events() {
    let identity = generate_self_signed("rd-host").unwrap();
    let fingerprint = identity.fingerprint;
    let server = server_endpoint("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
    let addr = server.local_addr().unwrap();

    let host = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await.unwrap();
        let session = Session::new(conn);
        let (mut _tx, mut rx) = session
            .accept_control::<HostEvent, ViewerCommand>()
            .await
            .unwrap();
        let mut events = Vec::new();
        for _ in 0..1000 {
            events.push(rx.recv().await.unwrap());
        }
        events
    });

    let client = client_endpoint("127.0.0.1:0".parse().unwrap(), Some(fingerprint)).unwrap();
    let conn = client.connect(addr, "rd-host").unwrap().await.unwrap();
    let session = Session::new(conn);
    let (mut tx, _rx) = session
        .open_control::<ViewerCommand, HostEvent>()
        .await
        .unwrap();

    let start = std::time::Instant::now();
    for i in 0..1000u32 {
        tx.send(&ViewerCommand::Input(rd_protocol::InputEvent::MouseMove {
            monitor: 0,
            x: i as f32 / 1000.0,
            y: 0.5,
        }))
        .await
        .unwrap();
    }
    let elapsed = start.elapsed();

    let events = host.await.unwrap();
    assert_eq!(events.len(), 1000);
    println!(
        "1000 input event mất {:.2}ms ({:.1}us/event)",
        elapsed.as_secs_f64() * 1000.0,
        elapsed.as_micros() as f64 / 1000.0
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "gửi input quá chậm: {elapsed:?}"
    );
}
