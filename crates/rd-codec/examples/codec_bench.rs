//! Đo độ trễ mã hoá / giải mã phần cứng trên frame 1080p tổng hợp.
//!
//! Không cần quyền Screen Recording: frame được vẽ bằng tay vào `CVPixelBuffer`
//! có nền IOSurface — đúng loại buffer mà ScreenCaptureKit trả về, nên đường đi
//! vào bộ mã hoá giống hệt lúc chạy thật.
//!
//! Chạy: `cargo run --release -p rd-codec --example codec_bench`

fn main() {
    #[cfg(target_os = "macos")]
    macos::run();

    #[cfg(not(target_os = "macos"))]
    eprintln!("benchmark này chỉ chạy trên macOS");
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ptr::{NonNull, null_mut};
    use std::time::{Duration, Instant};

    use objc2_core_foundation::{CFDictionary, CFRetained, CFType};
    use objc2_core_video::{
        CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress,
        CVPixelBufferGetBytesPerRow, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
        CVPixelBufferUnlockBaseAddress, kCVPixelBufferIOSurfacePropertiesKey,
        kCVPixelFormatType_32BGRA,
    };
    use rd_codec::videotoolbox::{VtDecoder, VtEncoder};
    use rd_codec::{ChromaSubsampling, Codec, EncoderConfig};

    const WIDTH: u32 = 1920;
    const HEIGHT: u32 = 1080;
    const FRAMES: u32 = 300;

    pub fn run() {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .init();

        for codec in [Codec::Hevc, Codec::H264] {
            let config = EncoderConfig {
                width: WIDTH,
                height: HEIGHT,
                codec,
                chroma: ChromaSubsampling::Yuv422,
                target_bitrate_kbps: 30_000,
                target_fps: 60,
                keyframe_interval_secs: 10,
            };
            match bench(config) {
                Ok(()) => {}
                Err(err) => println!("{codec:?}: thất bại — {err}"),
            }
            println!();
        }
    }

    fn bench(config: EncoderConfig) -> rd_codec::Result<()> {
        let codec = config.codec;
        let target_fps = config.target_fps;
        let mut encoder = VtEncoder::new(config)?;
        let chroma = encoder.actual_chroma();

        println!("=== {codec:?} {WIDTH}x{HEIGHT} @ {target_fps}fps ===");
        println!(
            "phần cứng: {}   chroma thực tế: {:?}",
            if encoder.is_hardware() { "có" } else { "KHÔNG" },
            encoder.actual_chroma()
        );

        let mut surface = make_pixel_buffer(WIDTH, HEIGHT)?;
        let mut encode_us: Vec<u32> = Vec::with_capacity(FRAMES as usize);
        let mut encoded: Vec<rd_codec::EncodedFrame> = Vec::with_capacity(FRAMES as usize);

        // Nhịp gửi bằng đúng tốc độ khung hình mục tiêu. Nếu dồn cả 300 frame vào
        // một lúc, con số đo được sẽ là thời gian xếp hàng chứ không phải độ trễ
        // mã hoá — không phản ánh đường chạy thật.
        let frame_gap = Duration::from_micros(1_000_000 / target_fps as u64);
        let started = Instant::now();

        for index in 0..FRAMES {
            let deadline = started + frame_gap * index;
            let now = Instant::now();
            if deadline > now {
                std::thread::sleep(deadline - now);
            }

            draw_frame(&mut surface, index);
            let pts_us = (index as u64 * 1_000_000) / target_fps as u64;
            encoder.submit(&surface, pts_us)?;

            // Bộ mã hoá ở chế độ realtime, không B-frame nên trả kết quả gần như
            // ngay; vẫn để hạn chờ rộng để phân biệt "chậm" với "treo".
            match encoder.next_frame(Duration::from_millis(500)) {
                Ok(frame) => {
                    encode_us.push(frame.encode_us);
                    encoded.push(frame);
                }
                Err(rd_codec::CodecError::Timeout) => {
                    println!("  frame {index}: quá 500ms không có kết quả");
                }
                Err(err) => return Err(err),
            }
        }

        encoder.flush()?;
        while let Some(frame) = encoder.try_next_frame() {
            encode_us.push(frame.encode_us);
            encoded.push(frame);
        }

        let wall = started.elapsed();
        let stats = encoder.stats();

        if encoded.is_empty() {
            println!("  không nhận được frame nào");
            return Ok(());
        }

        let keyframes: Vec<&rd_codec::EncodedFrame> =
            encoded.iter().filter(|frame| frame.keyframe).collect();
        let key_bytes: usize = keyframes.iter().map(|frame| frame.data.len()).sum();
        let delta_bytes: usize = encoded
            .iter()
            .filter(|frame| !frame.keyframe)
            .map(|frame| frame.data.len())
            .sum();
        let total_bytes = key_bytes + delta_bytes;
        let delta_count = encoded.len() - keyframes.len();

        // Frame đầu tiên gánh cả chi phí khởi động phiên mã hoá (cấp phát buffer,
        // đánh thức media engine). Tách riêng để phần còn lại phản ánh đúng độ
        // trễ lúc phiên đã chạy ổn định.
        println!(
            "  khởi động (frame đầu): {:.2} ms",
            encode_us[0] as f64 / 1000.0
        );
        report("encode", &mut encode_us[1..].to_vec());
        println!(
            "  frame: {} phát ra / {} gửi vào, rơi {}, lỗi {}",
            stats.frames_emitted, stats.frames_submitted, stats.frames_dropped, stats.errors
        );
        println!(
            "  bitrate thực tế: {:.1} Mbps ({:.1} MB cho {:.2}s)",
            total_bytes as f64 * 8.0 / wall.as_secs_f64() / 1e6,
            total_bytes as f64 / 1e6,
            wall.as_secs_f64()
        );
        println!(
            "  keyframe: {} cái, trung bình {:.0} KB   delta: {} cái, trung bình {:.1} KB",
            keyframes.len(),
            if keyframes.is_empty() {
                0.0
            } else {
                key_bytes as f64 / keyframes.len() as f64 / 1024.0
            },
            delta_count,
            if delta_count == 0 {
                0.0
            } else {
                delta_bytes as f64 / delta_count as f64 / 1024.0
            }
        );

        // Frame đầu tiên bắt buộc là keyframe kèm parameter set, nếu không thì
        // viewer nối vào sẽ không bao giờ giải mã được.
        assert!(encoded[0].keyframe, "frame đầu tiên phải là keyframe");
        assert!(
            has_parameter_sets(&encoded[0].data, codec),
            "keyframe phải mang theo parameter set"
        );

        decode_back(codec, chroma, &encoded)?;
        Ok(())
    }

    fn decode_back(
        codec: Codec,
        chroma: ChromaSubsampling,
        encoded: &[rd_codec::EncodedFrame],
    ) -> rd_codec::Result<()> {
        let mut decoder = VtDecoder::new(codec, chroma)?;
        let mut decode_us: Vec<u32> = Vec::with_capacity(encoded.len());
        let mut mismatched = 0usize;

        let mut reported = false;
        for frame in encoded {
            match decoder.decode(&frame.data, frame.pts_us)? {
                Some(decoded) => {
                    if !reported {
                        reported = true;
                        let fourcc =
                            objc2_core_video::CVPixelBufferGetPixelFormatType(decoded.pixel_buffer())
                                .to_be_bytes();
                        println!(
                            "  định dạng pixel đầu ra: '{}' = {:?} ({} plane)",
                            String::from_utf8_lossy(&fourcc),
                            decoded.format,
                            objc2_core_video::CVPixelBufferGetPlaneCount(decoded.pixel_buffer())
                        );
                    }
                    if decoded.width != WIDTH || decoded.height != HEIGHT {
                        mismatched += 1;
                    }
                    decode_us.push(decoded.decode_us);
                }
                None => mismatched += 1,
            }
        }

        report("decode", &mut decode_us);
        let stats = decoder.stats();
        println!(
            "  frame: {} giải mã được / {} gửi vào, rơi {}, lỗi {}",
            stats.frames_emitted, stats.frames_submitted, stats.frames_dropped, stats.errors
        );
        assert_eq!(mismatched, 0, "có frame sai kích thước hoặc không giải mã được");
        assert_eq!(
            stats.frames_emitted as usize,
            encoded.len(),
            "số frame giải mã phải khớp số frame mã hoá"
        );
        Ok(())
    }

    fn report(label: &str, samples: &mut [u32]) {
        if samples.is_empty() {
            println!("  {label}: không có mẫu");
            return;
        }
        samples.sort_unstable();
        let mean = samples.iter().map(|v| *v as u64).sum::<u64>() as f64 / samples.len() as f64;
        println!(
            "  {label} (ms): tb {:.2}  p50 {:.2}  p95 {:.2}  p99 {:.2}  max {:.2}",
            mean / 1000.0,
            percentile(samples, 0.50) as f64 / 1000.0,
            percentile(samples, 0.95) as f64 / 1000.0,
            percentile(samples, 0.99) as f64 / 1000.0,
            *samples.last().expect("đã kiểm tra không rỗng") as f64 / 1000.0,
        );
    }

    fn percentile(sorted: &[u32], q: f64) -> u32 {
        let index = ((sorted.len() - 1) as f64 * q).round() as usize;
        sorted[index]
    }

    fn has_parameter_sets(data: &[u8], codec: Codec) -> bool {
        let classify = match codec {
            Codec::Hevc => rd_codec::annexb::classify_hevc,
            _ => rd_codec::annexb::classify_h264,
        };
        rd_codec::annexb::iter_nalus(data)
            .any(|nalu| classify(nalu) == rd_codec::annexb::NaluClass::ParameterSet)
    }

    /// Tạo `CVPixelBuffer` BGRA có nền IOSurface, giống hệt buffer của ScreenCaptureKit.
    fn make_pixel_buffer(width: u32, height: u32) -> rd_codec::Result<CFRetained<CVPixelBuffer>> {
        // Từ điển rỗng cho IOSurfaceProperties nghĩa là "dùng thiết lập mặc định
        // nhưng vẫn phải có nền IOSurface" — thiếu khoá này thì buffer chỉ nằm
        // trên RAM thường và bộ mã hoá phải copy thêm một lần.
        let empty: CFRetained<CFDictionary<CFType, CFType>> = CFDictionary::from_slices(&[], &[]);
        let key: &CFType = unsafe { kCVPixelBufferIOSurfacePropertiesKey };
        let value: &CFType = &empty;
        let attributes = CFDictionary::from_slices(&[key], &[value]);
        let attributes: &CFDictionary =
            unsafe { &*(&*attributes as *const CFDictionary<CFType, CFType> as *const CFDictionary) };

        let mut raw: *mut CVPixelBuffer = null_mut();
        let status = unsafe {
            CVPixelBufferCreate(
                None,
                width as usize,
                height as usize,
                kCVPixelFormatType_32BGRA,
                Some(attributes),
                NonNull::from(&mut raw),
            )
        };
        let Some(raw) = NonNull::new(raw).filter(|_| status == 0) else {
            return Err(rd_codec::CodecError::SessionCreate {
                what: "pixel buffer",
                status,
            });
        };
        Ok(unsafe { CFRetained::from_raw(raw) })
    }

    /// Vẽ nội dung giống màn hình desktop: nền tĩnh, một khối chạy ngang, và các
    /// sọc 1 pixel mô phỏng chữ.
    ///
    /// Nội dung phải *đổi thật* giữa các frame, nếu không bộ mã hoá chỉ phát ra
    /// frame rỗng vài trăm byte và con số đo được sẽ vô nghĩa.
    fn draw_frame(buffer: &mut CFRetained<CVPixelBuffer>, index: u32) {
        let flags = CVPixelBufferLockFlags(0);
        let status = unsafe { CVPixelBufferLockBaseAddress(buffer, flags) };
        assert_eq!(status, 0, "không khoá được pixel buffer");

        let base = CVPixelBufferGetBaseAddress(buffer).cast::<u8>();
        let stride = CVPixelBufferGetBytesPerRow(buffer);
        assert!(!base.is_null(), "pixel buffer không có địa chỉ CPU");

        let block_x = (index * 7) % (WIDTH - 200);
        let block_y = (index * 3) % (HEIGHT - 200);

        for y in 0..HEIGHT as usize {
            let row = unsafe { std::slice::from_raw_parts_mut(base.add(y * stride), stride) };
            // Nền: dốc màu tĩnh, gần như không tốn bit sau frame đầu.
            let shade = (y * 255 / HEIGHT as usize) as u8;
            for x in 0..WIDTH as usize {
                let px = &mut row[x * 4..x * 4 + 4];
                let in_block = x >= block_x as usize
                    && x < block_x as usize + 200
                    && y >= block_y as usize
                    && y < block_y as usize + 200;
                // Sọc dọc 1 pixel ở nửa dưới: chi tiết tần số cao như nét chữ,
                // đây chính là chỗ 4:2:0 làm nhoè mà 4:2:2 giữ được.
                let is_text = y > HEIGHT as usize / 2 && (x + index as usize) % 3 == 0;

                let (b, g, r) = if in_block {
                    (40u8, 200u8, 90u8)
                } else if is_text {
                    (250, 250, 250)
                } else {
                    (shade / 3, shade / 2, shade)
                };
                px[0] = b;
                px[1] = g;
                px[2] = r;
                px[3] = 255;
            }
        }

        let status = unsafe { CVPixelBufferUnlockBaseAddress(buffer, flags) };
        assert_eq!(status, 0, "không mở khoá được pixel buffer");
    }
}
