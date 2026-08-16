# Remote Desktop

Điều khiển máy tính từ xa: xem và điều khiển màn hình máy khác, gửi/nhận tệp,
chat. Viết bằng Rust, chạy trên macOS và Windows, hai chiều — máy nào cũng vừa
chia sẻ được vừa điều khiển được.

Ưu tiên thiết kế là **độ trễ thấp ở độ phân giải gốc**: chụp màn hình, mã hoá,
giải mã và vẽ đều ở trên GPU, không có lần nào ảnh đi qua RAM thường.

| Chặng    | macOS                    | Windows                        |
| -------- | ------------------------ | ------------------------------ |
| Chụp     | ScreenCaptureKit         | DXGI Desktop Duplication       |
| Mã hoá   | VideoToolbox (HEVC 4:2:2 10-bit) | Media Foundation (HEVC 4:2:0 8-bit) |
| Giải mã  | VideoToolbox             | Media Foundation (DXVA)        |
| Vẽ       | wgpu ← IOSurface         | wgpu ← texture D3D12 dùng chung |
| Bơm input| CGEvent                  | SendInput                      |

Đường truyền là QUIC: video đi bằng datagram (mất gói thì bỏ frame, không chờ
gửi lại), còn điều khiển, chat và tệp đi bằng stream tin cậy.

Đo tại chỗ trên MacBook Apple Silicon, 1920×1080 @ 60 fps, HEVC 4:2:2 phần cứng:
**p50 ≈ 10 ms, p99 ≈ 12 ms, 20–24 Mbps** cho cả chuỗi chụp → mã hoá → truyền →
giải mã → vẽ.

## Cài đặt

Tải ở mục [Releases](../../releases).

- **macOS**: mở `.dmg`, kéo `Remote Desktop.app` vào Applications. Bản build là
  ad-hoc signature chứ không có Developer ID, nên lần đầu phải bấm chuột phải →
  *Open* để qua Gatekeeper.
- **Windows**: chạy `RemoteDesktop-x.y.z-windows-setup.exe`, hoặc giải nén bản
  `.zip` rồi chạy thẳng `remote-desktop.exe`.

### Quyền trên macOS

Máy **chia sẻ màn hình** phải được cấp hai quyền, nếu không chương trình vẫn
chạy nhưng chỉ phát ra hình tổng hợp và không bấm được gì:

1. System Settings → Privacy & Security → **Screen Recording** → bật Remote Desktop
2. System Settings → Privacy & Security → **Accessibility** → bật Remote Desktop

Cấp xong phải khởi động lại chương trình. Windows không cần cấp quyền gì.

## Dùng

Mở chương trình rồi chọn một trong ba nút, hoặc vào thẳng bằng tham số:

```
remote-desktop --host --password 123456
remote-desktop --connect 192.168.1.20:47823 --password 123456
```

Máy chia sẻ hiện **mã 9 chữ số** và **mật khẩu phiên**; đọc cho người bên kia
gõ vào là xong. Trong cùng mạng LAN thì gõ thẳng `IP:47823` cũng được.

Phím tắt khi đang xem: `F9` bật/tắt điều khiển, `F8` ẩn/hiện bảng chat và tệp,
`F10` ẩn/hiện thông số. Gửi tệp bằng cách kéo thả vào cửa sổ.

`remote-desktop --help` in đủ danh sách tham số.

## Nối qua Internet

Hai máy ở hai mạng khác nhau cần một **rendezvous server** đặt ở nơi có IP công
cộng để chúng tìm nhau và đục lỗ NAT:

```bash
RD_PUBLIC_IP=203.0.113.10 rd-rendezvous 0.0.0.0:7000
```

Mở cổng UDP 7000 (hẹn gặp) và 7001 (relay cho cặp máy không đục được lỗ). Lúc
khởi động server in ra vân tay chứng chỉ; đưa địa chỉ server cho client bằng
`--rendezvous ĐỊA_CHỈ:7000`.

Server chỉ giữ danh sách máy đang online và giúp hai bên bắt tay. Nội dung phiên
mã hoá đầu-cuối giữa hai máy, server không đọc được.

## Build từ mã nguồn

Cần Rust 1.85 trở lên. Trên macOS cần Xcode Command Line Tools, trên Windows cần
Visual Studio Build Tools (thành phần MSVC C++).

```bash
cargo build --release
cargo test --workspace
```

Đóng gói:

```bash
packaging/macos/bundle.sh 0.1.0                      # .app + .dmg
iscc /DVersion=0.1.0 packaging/windows/installer.iss # bộ cài Windows
```

## Giấy phép

MIT.
