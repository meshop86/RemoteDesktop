//! Header nhị phân cố định cho gói video chạy trên QUIC datagram.
//!
//! Bố cục 20 byte (little-endian):
//!
//! ```text
//! offset  size  field
//!  0      1     kind          luôn = 0x01 (video)
//!  1      1     flags         bit0 keyframe, bit1 fragment cuối của frame
//!  2      1     monitor       chỉ số màn hình (multi-monitor)
//!  3      1     codec         0 H264, 1 HEVC, 2 AV1
//!  4      4     frame_id      u32, tăng dần, wrap-around chấp nhận được
//!  8      2     fragment_idx  u16
//! 10      2     fragment_cnt  u16
//! 12      8     capture_us    u64, micro giây khi frame được capture
//! ```
//!
//! `capture_us` đi kèm từng gói (không phải chỉ gói đầu) để viewer tính được
//! latency end-to-end ngay cả khi gói đầu của frame bị mất.

use bytes::Bytes;

use crate::{MAX_VIDEO_PAYLOAD, ProtocolError, Result, control::Codec};

pub const VIDEO_HEADER_LEN: usize = 20;
pub const PACKET_KIND_VIDEO: u8 = 0x01;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VideoFlags(pub u8);

impl VideoFlags {
    pub const KEYFRAME: u8 = 0b0000_0001;
    pub const LAST_FRAGMENT: u8 = 0b0000_0010;

    pub fn is_keyframe(self) -> bool {
        self.0 & Self::KEYFRAME != 0
    }

    pub fn is_last_fragment(self) -> bool {
        self.0 & Self::LAST_FRAGMENT != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoHeader {
    pub flags: VideoFlags,
    pub monitor: u8,
    pub codec: Codec,
    pub frame_id: u32,
    pub fragment_idx: u16,
    pub fragment_count: u16,
    pub capture_us: u64,
}

impl VideoHeader {
    pub fn write_to(&self, out: &mut [u8; VIDEO_HEADER_LEN]) {
        out[0] = PACKET_KIND_VIDEO;
        out[1] = self.flags.0;
        out[2] = self.monitor;
        out[3] = self.codec as u8;
        out[4..8].copy_from_slice(&self.frame_id.to_le_bytes());
        out[8..10].copy_from_slice(&self.fragment_idx.to_le_bytes());
        out[10..12].copy_from_slice(&self.fragment_count.to_le_bytes());
        out[12..20].copy_from_slice(&self.capture_us.to_le_bytes());
    }

    pub fn parse(buf: &[u8]) -> Result<(Self, &[u8])> {
        if buf.len() < VIDEO_HEADER_LEN {
            return Err(ProtocolError::TooShort(buf.len()));
        }
        if buf[0] != PACKET_KIND_VIDEO {
            return Err(ProtocolError::UnknownPacketKind(buf[0]));
        }
        let header = VideoHeader {
            flags: VideoFlags(buf[1]),
            monitor: buf[2],
            codec: Codec::from_u8(buf[3])?,
            frame_id: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
            fragment_idx: u16::from_le_bytes(buf[8..10].try_into().unwrap()),
            fragment_count: u16::from_le_bytes(buf[10..12].try_into().unwrap()),
            capture_us: u64::from_le_bytes(buf[12..20].try_into().unwrap()),
        };
        Ok((header, &buf[VIDEO_HEADER_LEN..]))
    }
}

/// Cắt một frame đã encode thành các datagram vừa MTU.
///
/// Trả về vector các gói đã gắn header sẵn, gửi thẳng qua QUIC datagram được.
/// Frame rỗng vẫn sinh đúng một gói để phía nhận biết frame tồn tại.
pub fn fragment_frame(
    encoded: &[u8],
    frame_id: u32,
    monitor: u8,
    codec: Codec,
    keyframe: bool,
    capture_us: u64,
) -> Vec<Bytes> {
    let chunk_size = MAX_VIDEO_PAYLOAD;
    let fragment_count = encoded.len().div_ceil(chunk_size).max(1);

    // Quá 65535 fragment nghĩa là frame > 77MB, không thể xảy ra với video thật.
    debug_assert!(fragment_count <= u16::MAX as usize);

    let mut packets = Vec::with_capacity(fragment_count);
    for (idx, chunk) in encoded
        .chunks(chunk_size)
        .chain(std::iter::repeat_n(&[][..], usize::from(encoded.is_empty())))
        .enumerate()
    {
        let mut flags = 0u8;
        if keyframe {
            flags |= VideoFlags::KEYFRAME;
        }
        if idx + 1 == fragment_count {
            flags |= VideoFlags::LAST_FRAGMENT;
        }
        let header = VideoHeader {
            flags: VideoFlags(flags),
            monitor,
            codec,
            frame_id,
            fragment_idx: idx as u16,
            fragment_count: fragment_count as u16,
            capture_us,
        };
        let mut packet = vec![0u8; VIDEO_HEADER_LEN + chunk.len()];
        let (head, body) = packet.split_at_mut(VIDEO_HEADER_LEN);
        header.write_to(<&mut [u8; VIDEO_HEADER_LEN]>::try_from(head).unwrap());
        body.copy_from_slice(chunk);
        packets.push(Bytes::from(packet));
    }
    packets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let header = VideoHeader {
            flags: VideoFlags(VideoFlags::KEYFRAME | VideoFlags::LAST_FRAGMENT),
            monitor: 2,
            codec: Codec::Hevc,
            frame_id: 123_456,
            fragment_idx: 7,
            fragment_count: 8,
            capture_us: 1_700_000_000_000_000,
        };
        let mut buf = [0u8; VIDEO_HEADER_LEN];
        header.write_to(&mut buf);
        let (parsed, rest) = VideoHeader::parse(&buf).unwrap();
        assert_eq!(parsed, header);
        assert!(rest.is_empty());
        assert!(parsed.flags.is_keyframe());
        assert!(parsed.flags.is_last_fragment());
    }

    #[test]
    fn fragment_covers_whole_frame() {
        let payload: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        let packets = fragment_frame(&payload, 42, 0, Codec::H264, true, 999);
        assert_eq!(packets.len(), payload.len().div_ceil(MAX_VIDEO_PAYLOAD));

        let mut rebuilt = Vec::new();
        for (i, packet) in packets.iter().enumerate() {
            let (header, body) = VideoHeader::parse(packet).unwrap();
            assert_eq!(header.frame_id, 42);
            assert_eq!(header.fragment_idx, i as u16);
            assert_eq!(header.fragment_count as usize, packets.len());
            assert!(packet.len() <= crate::MAX_DATAGRAM_SIZE);
            rebuilt.extend_from_slice(body);
        }
        assert_eq!(rebuilt, payload);
        assert!(
            VideoHeader::parse(packets.last().unwrap())
                .unwrap()
                .0
                .flags
                .is_last_fragment()
        );
    }

    #[test]
    fn empty_frame_still_produces_one_packet() {
        let packets = fragment_frame(&[], 1, 0, Codec::Av1, false, 0);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].len(), VIDEO_HEADER_LEN);
    }
}
