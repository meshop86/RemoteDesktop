//! Gom các fragment rời rạc thành frame hoàn chỉnh.
//!
//! Triết lý ở đây là **thà bỏ frame còn hơn chờ**: datagram không có
//! retransmit, nên một frame thiếu gói sẽ không bao giờ đủ. Giữ nó lại chỉ làm
//! tăng độ trễ. Khi phát hiện frame hỏng, viewer xin host gửi keyframe mới.

use std::collections::BTreeMap;

use crate::{
    Result,
    control::Codec,
    video::{VideoFlags, VideoHeader},
};

#[derive(Debug, Clone)]
pub struct AssembledFrame {
    pub frame_id: u32,
    pub monitor: u8,
    pub codec: Codec,
    pub keyframe: bool,
    pub capture_us: u64,
    pub data: Vec<u8>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AssemblerStats {
    pub frames_completed: u64,
    /// Frame bị bỏ vì thiếu fragment hoặc đến quá muộn.
    pub frames_dropped: u64,
    pub packets_received: u64,
    /// Số fragment còn thiếu của các frame đã bị bỏ — xấp xỉ số gói mất.
    pub packets_lost: u64,
}

struct Partial {
    fragments: Vec<Option<Vec<u8>>>,
    received: u16,
    total_len: usize,
    monitor: u8,
    codec: Codec,
    keyframe: bool,
    capture_us: u64,
}

pub struct FrameAssembler {
    partial: BTreeMap<u32, Partial>,
    newest_completed: Option<u32>,
    max_pending: usize,
    stats: AssemblerStats,
}

impl Default for FrameAssembler {
    fn default() -> Self {
        Self::new(4)
    }
}

impl FrameAssembler {
    /// `max_pending` là số frame dở dang tối đa được giữ cùng lúc. Đặt nhỏ
    /// (2-4) cho độ trễ thấp; đặt lớn hơn nếu đường truyền hay đảo thứ tự gói.
    pub fn new(max_pending: usize) -> Self {
        Self {
            partial: BTreeMap::new(),
            newest_completed: None,
            max_pending: max_pending.max(1),
            stats: AssemblerStats::default(),
        }
    }

    pub fn stats(&self) -> AssemblerStats {
        self.stats
    }

    /// Nạp một datagram. Trả về `Some(frame)` ngay khi frame đủ fragment.
    pub fn push(&mut self, packet: &[u8]) -> Result<Option<AssembledFrame>> {
        let (header, body) = VideoHeader::parse(packet)?;
        self.stats.packets_received += 1;

        // Bỏ fragment thuộc frame cũ hơn frame đã hoàn chỉnh gần nhất: decoder
        // đã đi qua điểm đó rồi, nạp vào chỉ gây giật.
        if let Some(newest) = self.newest_completed
            && !is_newer(header.frame_id, newest)
        {
            return Ok(None);
        }

        let entry = self.partial.entry(header.frame_id).or_insert_with(|| Partial {
            fragments: vec![None; header.fragment_count as usize],
            received: 0,
            total_len: 0,
            monitor: header.monitor,
            codec: header.codec,
            keyframe: header.flags.is_keyframe(),
            capture_us: header.capture_us,
        });

        let idx = header.fragment_idx as usize;
        if idx >= entry.fragments.len() {
            // fragment_count không khớp giữa các gói của cùng frame: gói hỏng.
            return Ok(None);
        }
        if entry.fragments[idx].is_some() {
            return Ok(None); // gói lặp
        }

        entry.fragments[idx] = Some(body.to_vec());
        entry.received += 1;
        entry.total_len += body.len();
        if header.flags.0 & VideoFlags::KEYFRAME != 0 {
            entry.keyframe = true;
        }

        let complete = entry.received as usize == entry.fragments.len();
        if complete {
            let frame_id = header.frame_id;
            let partial = self.partial.remove(&frame_id).expect("vừa lấy ở trên");
            let mut data = Vec::with_capacity(partial.total_len);
            for fragment in partial.fragments {
                data.extend_from_slice(&fragment.expect("frame đủ nên mọi fragment đều có"));
            }
            self.newest_completed = Some(frame_id);
            self.stats.frames_completed += 1;
            self.drop_older_than(frame_id);
            return Ok(Some(AssembledFrame {
                frame_id,
                monitor: partial.monitor,
                codec: partial.codec,
                keyframe: partial.keyframe,
                capture_us: partial.capture_us,
                data,
            }));
        }

        self.enforce_window();
        Ok(None)
    }

    /// Frame dở dang cũ hơn frame vừa hoàn chỉnh không còn giá trị.
    fn drop_older_than(&mut self, frame_id: u32) {
        let stale: Vec<u32> = self
            .partial
            .keys()
            .copied()
            .filter(|id| !is_newer(*id, frame_id))
            .collect();
        for id in stale {
            self.discard(id);
        }
    }

    /// Giới hạn số frame dở dang; frame cũ nhất bị loại trước.
    fn enforce_window(&mut self) {
        while self.partial.len() > self.max_pending {
            let oldest = *self.partial.keys().next().expect("map không rỗng");
            self.discard(oldest);
        }
    }

    fn discard(&mut self, frame_id: u32) {
        if let Some(partial) = self.partial.remove(&frame_id) {
            self.stats.frames_dropped += 1;
            self.stats.packets_lost += (partial.fragments.len() - partial.received as usize) as u64;
        }
    }
}

/// So sánh frame id có tính wrap-around của u32.
fn is_newer(candidate: u32, reference: u32) -> bool {
    (candidate.wrapping_sub(reference) as i32) > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::fragment_frame;

    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn reassembles_multi_fragment_frame() {
        let data = payload(10_000);
        let packets = fragment_frame(&data, 1, 0, Codec::Hevc, true, 12345);
        let mut assembler = FrameAssembler::default();

        let mut out = None;
        for packet in &packets {
            if let Some(frame) = assembler.push(packet).unwrap() {
                out = Some(frame);
            }
        }
        let frame = out.expect("phải ghép được frame");
        assert_eq!(frame.data, data);
        assert_eq!(frame.frame_id, 1);
        assert!(frame.keyframe);
        assert_eq!(frame.capture_us, 12345);
        assert_eq!(assembler.stats().frames_completed, 1);
    }

    #[test]
    fn out_of_order_fragments_still_assemble() {
        let data = payload(4000);
        let mut packets = fragment_frame(&data, 7, 0, Codec::H264, false, 1);
        packets.reverse();
        let mut assembler = FrameAssembler::default();
        let mut out = None;
        for packet in &packets {
            if let Some(frame) = assembler.push(packet).unwrap() {
                out = Some(frame);
            }
        }
        assert_eq!(out.unwrap().data, data);
    }

    #[test]
    fn missing_fragment_drops_frame_and_counts_loss() {
        let data = payload(10_000);
        let broken = fragment_frame(&data, 1, 0, Codec::Hevc, true, 0);
        let good = fragment_frame(&payload(500), 2, 0, Codec::Hevc, false, 0);
        let mut assembler = FrameAssembler::default();

        // Frame 1 mất gói đầu nên không bao giờ đủ.
        for packet in &broken[1..] {
            assert!(assembler.push(packet).unwrap().is_none());
        }
        // Frame 2 đủ gói -> hoàn thành, đồng thời loại bỏ frame 1 đang dở.
        let mut done = false;
        for packet in &good {
            if assembler.push(packet).unwrap().is_some() {
                done = true;
            }
        }
        assert!(done);
        let stats = assembler.stats();
        assert_eq!(stats.frames_completed, 1);
        assert_eq!(stats.frames_dropped, 1);
        assert_eq!(stats.packets_lost, 1);
    }

    #[test]
    fn late_fragments_of_old_frame_are_ignored() {
        let old = fragment_frame(&payload(300), 5, 0, Codec::Hevc, false, 0);
        let new = fragment_frame(&payload(300), 9, 0, Codec::Hevc, false, 0);
        let mut assembler = FrameAssembler::default();
        assert!(assembler.push(&new[0]).unwrap().is_some());
        assert!(assembler.push(&old[0]).unwrap().is_none());
        assert_eq!(assembler.stats().frames_completed, 1);
    }

    #[test]
    fn window_limits_pending_frames() {
        let mut assembler = FrameAssembler::new(2);
        // Mỗi frame gửi thiếu 1 gói nên không frame nào hoàn thành.
        for id in 1..=5u32 {
            let packets = fragment_frame(&payload(5000), id, 0, Codec::Hevc, false, 0);
            assembler.push(&packets[0]).unwrap();
        }
        assert!(assembler.stats().frames_dropped >= 2);
    }
}
