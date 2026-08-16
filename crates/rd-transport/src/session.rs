//! Phiên kết nối: kênh video (datagram) + kênh control (stream tin cậy).

use std::{marker::PhantomData, net::SocketAddr, time::Duration};

use bytes::Bytes;
use quinn::{Connection, RecvStream, SendStream};
use rd_protocol::{
    AssembledFrame, FrameAssembler, assembler::AssemblerStats, control::Codec, video::fragment_frame,
};
use serde::{Serialize, de::DeserializeOwned};

use crate::TransportError;

/// Trần kích thước một control message. Chat và metadata file đều rất nhỏ;
/// giới hạn này chỉ để chặn đầu kia gửi độ dài rác làm ta cấp phát khổng lồ.
pub const MAX_CONTROL_MESSAGE: usize = 1024 * 1024;

#[derive(Clone)]
pub struct Session {
    conn: Connection,
}

impl Session {
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn remote_address(&self) -> SocketAddr {
        self.conn.remote_address()
    }

    pub fn close(&self, reason: &str) {
        self.conn.close(0u32.into(), reason.as_bytes());
    }

    /// Mở kênh control (bên chủ động gọi dùng hàm này).
    pub async fn open_control<Tx, Rx>(
        &self,
    ) -> Result<(ControlSender<Tx>, ControlReceiver<Rx>), TransportError>
    where
        Tx: Serialize,
        Rx: DeserializeOwned,
    {
        let (send, recv) = self.conn.open_bi().await?;
        Ok((ControlSender::new(send), ControlReceiver::new(recv)))
    }

    /// Nhận kênh control (bên bị gọi dùng hàm này).
    pub async fn accept_control<Tx, Rx>(
        &self,
    ) -> Result<(ControlSender<Tx>, ControlReceiver<Rx>), TransportError>
    where
        Tx: Serialize,
        Rx: DeserializeOwned,
    {
        let (send, recv) = self.conn.accept_bi().await?;
        Ok((ControlSender::new(send), ControlReceiver::new(recv)))
    }

    pub async fn open_uni(&self) -> Result<SendStream, TransportError> {
        Ok(self.conn.open_uni().await?)
    }

    pub async fn accept_uni(&self) -> Result<RecvStream, TransportError> {
        Ok(self.conn.accept_uni().await?)
    }

    pub async fn read_datagram(&self) -> Result<Bytes, TransportError> {
        Ok(self.conn.read_datagram().await?)
    }

    pub fn max_datagram_size(&self) -> Option<usize> {
        self.conn.max_datagram_size()
    }

    pub fn link_stats(&self) -> LinkStats {
        let stats = self.conn.stats();
        LinkStats {
            rtt: stats.path.rtt,
            congestion_window: stats.path.cwnd,
            lost_packets: stats.path.lost_packets,
            sent_packets: stats.path.sent_packets,
            current_mtu: stats.path.current_mtu,
            bytes_sent: stats.udp_tx.bytes,
            bytes_received: stats.udp_rx.bytes,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LinkStats {
    pub rtt: Duration,
    pub congestion_window: u64,
    pub lost_packets: u64,
    pub sent_packets: u64,
    pub current_mtu: u16,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

impl LinkStats {
    pub fn loss_ratio(&self) -> f32 {
        if self.sent_packets == 0 {
            return 0.0;
        }
        self.lost_packets as f32 / self.sent_packets as f32
    }
}

/// Bên gửi video: cắt frame thành datagram và bắn đi.
///
/// Datagram không có retransmit — mất là mất. Đó là chủ ý: một frame đến muộn
/// còn tệ hơn một frame mất, vì frame sau đã sẵn sàng rồi.
pub struct VideoSender {
    session: Session,
    monitor: u8,
    frame_id: u32,
    dropped_frames: u64,
}

impl VideoSender {
    pub fn new(session: Session, monitor: u8) -> Self {
        Self {
            session,
            monitor,
            frame_id: 0,
            dropped_frames: 0,
        }
    }

    pub fn dropped_frames(&self) -> u64 {
        self.dropped_frames
    }

    /// Gửi một frame đã encode. Trả về số datagram đã bắn.
    pub fn send_frame(
        &mut self,
        encoded: &[u8],
        codec: Codec,
        keyframe: bool,
        capture_us: u64,
    ) -> Result<usize, TransportError> {
        let frame_id = self.frame_id;
        self.frame_id = self.frame_id.wrapping_add(1);

        let packets = fragment_frame(encoded, frame_id, self.monitor, codec, keyframe, capture_us);
        let total = packets.len();
        for packet in packets {
            match self.session.conn.send_datagram(packet) {
                Ok(()) => {}
                Err(quinn::SendDatagramError::ConnectionLost(e)) => {
                    return Err(TransportError::Connection(e));
                }
                Err(err) => {
                    // Hàng đợi gửi đầy hoặc gói quá lớn: bỏ phần còn lại của
                    // frame này thay vì chờ. Viewer sẽ xin keyframe mới.
                    self.dropped_frames += 1;
                    tracing::debug!(?err, frame_id, "bỏ frame vì không gửi được datagram");
                    return Ok(0);
                }
            }
        }
        Ok(total)
    }
}

/// Bên nhận video: đọc datagram, ghép fragment, trả frame hoàn chỉnh.
pub struct VideoReceiver {
    session: Session,
    assembler: FrameAssembler,
}

impl VideoReceiver {
    pub fn new(session: Session, max_pending_frames: usize) -> Self {
        Self {
            session,
            assembler: FrameAssembler::new(max_pending_frames),
        }
    }

    pub fn stats(&self) -> AssemblerStats {
        self.assembler.stats()
    }

    /// Chờ đến khi có một frame hoàn chỉnh.
    pub async fn next_frame(&mut self) -> Result<AssembledFrame, TransportError> {
        loop {
            let datagram = self.session.read_datagram().await?;
            match self.assembler.push(&datagram) {
                Ok(Some(frame)) => return Ok(frame),
                Ok(None) => continue,
                Err(err) => {
                    tracing::debug!(?err, "bỏ datagram hỏng");
                    continue;
                }
            }
        }
    }
}

pub struct ControlSender<T> {
    stream: SendStream,
    _marker: PhantomData<fn(T)>,
}

impl<T: Serialize> ControlSender<T> {
    fn new(stream: SendStream) -> Self {
        Self {
            stream,
            _marker: PhantomData,
        }
    }

    pub async fn send(&mut self, msg: &T) -> Result<(), TransportError> {
        let body = rd_protocol::encode_control(msg);
        if body.len() > MAX_CONTROL_MESSAGE {
            return Err(TransportError::MessageTooLarge(body.len()));
        }
        // Ghép độ dài và thân vào một lần ghi để không tách thành 2 gói TCP-like.
        let mut framed = Vec::with_capacity(4 + body.len());
        framed.extend_from_slice(&(body.len() as u32).to_le_bytes());
        framed.extend_from_slice(&body);
        self.stream.write_all(&framed).await?;
        Ok(())
    }

    pub async fn finish(&mut self) -> Result<(), TransportError> {
        self.stream.finish()?;
        Ok(())
    }
}

pub struct ControlReceiver<T> {
    stream: RecvStream,
    buf: Vec<u8>,
    _marker: PhantomData<fn() -> T>,
}

impl<T: DeserializeOwned> ControlReceiver<T> {
    fn new(stream: RecvStream) -> Self {
        Self {
            stream,
            buf: Vec::new(),
            _marker: PhantomData,
        }
    }

    pub async fn recv(&mut self) -> Result<T, TransportError> {
        let mut len_bytes = [0u8; 4];
        self.stream.read_exact(&mut len_bytes).await?;
        let len = u32::from_le_bytes(len_bytes) as usize;
        if len > MAX_CONTROL_MESSAGE {
            return Err(TransportError::MessageTooLarge(len));
        }
        self.buf.clear();
        self.buf.resize(len, 0);
        self.stream.read_exact(&mut self.buf).await?;
        Ok(postcard::from_bytes(&self.buf)?)
    }
}
