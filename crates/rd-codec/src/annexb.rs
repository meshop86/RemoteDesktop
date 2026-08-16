//! Chuyển đổi giữa hai cách đóng gói NAL unit.
//!
//! Bộ mã hoá phần cứng (VideoToolbox, Media Foundation) trả về dạng **length
//! prefix**: mỗi NAL unit có 4 byte độ dài đứng trước. Nhưng tham số giải mã
//! (VPS/SPS/PPS) lại nằm tách riêng trong `CMFormatDescription`, không có trong
//! luồng dữ liệu — nếu gửi thẳng qua mạng thì bên kia không giải mã nổi.
//!
//! Vì vậy ta gửi dạng **Annex B**: mỗi NAL unit bắt đầu bằng `00 00 00 01`, và
//! parameter set được chèn vào ngay trước mỗi keyframe. Nhờ đó viewer nối vào
//! giữa chừng vẫn xem được ngay từ keyframe kế tiếp, không cần bắt tay gì thêm.

pub const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Số byte của phần độ dài trong dạng length-prefix. VideoToolbox mặc định 4.
pub const NAL_LENGTH_SIZE: usize = 4;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BitstreamError {
    #[error("NAL unit bị cắt cụt")]
    Truncated,
    #[error("độ dài NAL unit ({0}) vượt quá dữ liệu còn lại ({1})")]
    LengthOverflow(usize, usize),
}

/// Ghi một NAL unit kèm start code vào cuối `out`.
pub fn push_nalu(out: &mut Vec<u8>, nalu: &[u8]) {
    out.extend_from_slice(&START_CODE);
    out.extend_from_slice(nalu);
}

/// Đổi buffer length-prefix sang Annex B, ghi nối vào `out`.
pub fn length_prefixed_to_annexb(src: &[u8], out: &mut Vec<u8>) -> Result<(), BitstreamError> {
    let mut offset = 0usize;
    while offset < src.len() {
        if offset + NAL_LENGTH_SIZE > src.len() {
            return Err(BitstreamError::Truncated);
        }
        let len = u32::from_be_bytes(
            src[offset..offset + NAL_LENGTH_SIZE]
                .try_into()
                .expect("đã kiểm tra đủ 4 byte"),
        ) as usize;
        offset += NAL_LENGTH_SIZE;
        let remaining = src.len() - offset;
        if len > remaining {
            return Err(BitstreamError::LengthOverflow(len, remaining));
        }
        push_nalu(out, &src[offset..offset + len]);
        offset += len;
    }
    Ok(())
}

/// Đổi Annex B ngược lại thành length-prefix (dạng VideoToolbox cần khi giải mã).
pub fn annexb_to_length_prefixed(nalus: &[&[u8]], out: &mut Vec<u8>) {
    for nalu in nalus {
        out.extend_from_slice(&(nalu.len() as u32).to_be_bytes());
        out.extend_from_slice(nalu);
    }
}

/// Duyệt từng NAL unit trong luồng Annex B, bỏ qua start code.
///
/// Chấp nhận cả start code 3 byte (`00 00 01`) lẫn 4 byte vì một số bộ mã hoá
/// dùng loại ngắn cho NAL unit không phải đầu access unit.
pub fn iter_nalus(data: &[u8]) -> NaluIter<'_> {
    NaluIter { data, pos: 0 }
}

pub struct NaluIter<'a> {
    data: &'a [u8],
    pos: usize,
}

/// Tìm start code kế tiếp từ `from`, trả về `(vị trí start code, độ dài start code)`.
fn find_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                return Some((i, 3));
            }
            if i + 4 <= data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                return Some((i, 4));
            }
        }
        i += 1;
    }
    None
}

impl<'a> Iterator for NaluIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        let (start, size) = find_start_code(self.data, self.pos)?;
        let body_start = start + size;
        let end = match find_start_code(self.data, body_start) {
            Some((next, _)) => next,
            None => self.data.len(),
        };
        self.pos = end;
        if body_start >= end {
            // Start code rỗng (hai start code dính nhau) — bỏ qua, đọc tiếp.
            return self.next();
        }
        Some(&self.data[body_start..end])
    }
}

/// Phân loại NAL unit đủ dùng cho việc tách parameter set và nhận diện keyframe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NaluClass {
    /// VPS/SPS/PPS — mô tả cách giải mã, phải có trước frame đầu tiên.
    ParameterSet,
    /// Frame độc lập, giải mã được ngay không cần frame trước.
    Keyframe,
    /// Frame phụ thuộc frame trước.
    Delta,
    Other,
}

/// H.264: 5 bit thấp của byte đầu là loại NAL unit.
pub fn classify_h264(nalu: &[u8]) -> NaluClass {
    match nalu.first().map(|b| b & 0x1F) {
        Some(7) | Some(8) => NaluClass::ParameterSet, // SPS, PPS
        Some(5) => NaluClass::Keyframe,               // IDR
        Some(1) => NaluClass::Delta,
        _ => NaluClass::Other,
    }
}

/// HEVC: 6 bit của byte đầu (bỏ bit forbidden_zero) là loại NAL unit.
pub fn classify_hevc(nalu: &[u8]) -> NaluClass {
    match nalu.first().map(|b| (b >> 1) & 0x3F) {
        Some(32..=34) => NaluClass::ParameterSet, // VPS, SPS, PPS
        // 16..=23 là nhóm IRAP: mọi frame trong đó đều tự giải mã được.
        Some(16..=23) => NaluClass::Keyframe,
        Some(0..=9) => NaluClass::Delta,
        _ => NaluClass::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_prefix_roundtrip() {
        let nalus: [&[u8]; 3] = [&[0x67, 1, 2, 3], &[0x68, 9], &[0x65, 4, 5, 6, 7, 8]];
        let mut prefixed = Vec::new();
        annexb_to_length_prefixed(&nalus, &mut prefixed);

        let mut annexb = Vec::new();
        length_prefixed_to_annexb(&prefixed, &mut annexb).unwrap();

        let parsed: Vec<&[u8]> = iter_nalus(&annexb).collect();
        assert_eq!(parsed, nalus);
    }

    #[test]
    fn iter_handles_three_byte_start_codes() {
        let data = [0, 0, 0, 1, 0x67, 0xAA, 0, 0, 1, 0x65, 0xBB, 0xCC];
        let parsed: Vec<&[u8]> = iter_nalus(&data).collect();
        assert_eq!(parsed, vec![&[0x67u8, 0xAA][..], &[0x65u8, 0xBB, 0xCC][..]]);
    }

    #[test]
    fn truncated_input_is_rejected_not_panicking() {
        // Khai báo 100 byte nhưng chỉ có 2 byte thật.
        let src = [0, 0, 0, 100, 0xAA, 0xBB];
        let mut out = Vec::new();
        assert_eq!(
            length_prefixed_to_annexb(&src, &mut out),
            Err(BitstreamError::LengthOverflow(100, 2))
        );
    }

    #[test]
    fn classification_matches_spec_bits() {
        assert_eq!(classify_hevc(&[32 << 1]), NaluClass::ParameterSet); // VPS
        assert_eq!(classify_hevc(&[34 << 1]), NaluClass::ParameterSet); // PPS
        assert_eq!(classify_hevc(&[19 << 1]), NaluClass::Keyframe); // IDR_W_RADL
        assert_eq!(classify_hevc(&[1 << 1]), NaluClass::Delta);

        assert_eq!(classify_h264(&[0x67]), NaluClass::ParameterSet); // SPS
        assert_eq!(classify_h264(&[0x65]), NaluClass::Keyframe); // IDR
        assert_eq!(classify_h264(&[0x41]), NaluClass::Delta);
    }
}
