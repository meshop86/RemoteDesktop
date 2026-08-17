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

### Windows báo "Windows protected your PC"

Bản phát hành chưa mua chứng chỉ ký mã (code signing), nên SmartScreen chặn ở
lần chạy đầu: bấm **More info** → **Run anyway**. Đây là cảnh báo về danh tính
người phát hành, không phải kết quả quét virus.

Nếu Windows Defender báo hẳn một tên mã độc (`Trojan:Win32/Wacatac`,
`Program:Win32/Wacapew`... ) thì gần như chắc là báo nhầm: chương trình chụp
màn hình, giả lập chuột phím và mở kết nối ra Internet — đúng những việc mà
phần mềm gián điệp cũng làm, nên bộ quét đoán mò theo hành vi. Cách kiểm tra là
đối chiếu mã băm SHA-256 của file tải về với mã ghi trong trang Releases, rồi
tra mã đó trên VirusTotal:

```powershell
Get-FileHash .\RemoteDesktop-x.y.z-windows-setup.exe -Algorithm SHA256
```

### Cài xong bấm vào không lên gì

Từ bản 0.1.4, mọi lỗi lúc khởi động đều hiện thành hộp thoại và được ghi lại.
Nếu cửa sổ chương trình không mở, mở file nhật ký lên đọc dòng cuối:

```powershell
notepad $env:LOCALAPPDATA\RemoteDesktop\rd.log
```

Nguyên nhân hay gặp nhất là trình điều khiển card màn hình quá cũ hoặc chưa cài
— trong nhật ký sẽ có dòng `thấy card màn hình` liệt kê từng card mà chương
trình nhìn thấy. Không có dòng nào nghĩa là Windows không đưa ra card nào dùng
được; cập nhật driver rồi chạy lại.

Nhật ký dừng ngay sau dòng `dùng card` nghĩa là driver card đó kéo cả chương
trình chết theo, không kịp báo gì. Cứ **mở lại lần nữa**: lần sau chương trình
tự bỏ qua đường vẽ vừa hỏng và thử đường khác. Muốn tự chọn thì đặt biến môi
trường `WGPU_BACKEND` thành `dx12`, `vulkan` hoặc `gl`.

### Nối được nhưng không thấy màn hình, không điều khiển được

Chạy lệnh sau ở máy đang trục trặc — nó liệt kê màn hình, chụp thử một khung
hình thật, rồi in ra codec máy này mã hoá và giải mã được:

```
remote-desktop --probe
```

Dòng `Chụp thử màn hình chính: KHÔNG được` nghĩa là chương trình không lấy được
hình từ hệ điều hành, nên bên kia chỉ thấy hình tổng hợp; lý do in kèm ngay đó.

Ở mục Video, `giải mã được: [H264]` (không có `Hevc`) là chuyện bình thường trên
Windows: bộ giải mã HEVC không có sẵn mà nằm trong gói *HEVC Video Extensions*
của Microsoft Store. Từ bản 0.1.6, hai máy tự thoả thuận codec chung nên vẫn
chạy được bằng H.264; cài thêm gói đó thì hình nét hơn ở cùng băng thông.

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

Máy chia sẻ hiện **mã 9 chữ số**, **mật khẩu phiên** và **địa chỉ IP của chính
nó** (cả địa chỉ trong mạng nhà lẫn địa chỉ Tailscale nếu có) — đọc cho người
bên kia gõ vào là xong, khỏi phải đi lục trong cài đặt mạng.

Phím tắt khi đang xem: `F9` bật/tắt điều khiển, `F8` ẩn/hiện bảng chat và tệp,
`F10` ẩn/hiện thông số. Gửi tệp bằng cách kéo thả vào cửa sổ.

`remote-desktop --help` in đủ danh sách tham số.

## Nối qua Tailscale (cách dễ nhất)

Nếu hai máy đều là máy của bạn, cài [Tailscale](https://tailscale.com/download)
rồi đăng nhập cùng một tài khoản là xong: không cần rendezvous server, không
phải mở cổng trên router, và địa chỉ không đổi khi chuyển mạng.

Chương trình tự nhận ra Tailscale và hiện một khung ở màn hình đầu:

- chưa đăng nhập thì có nút **Đăng nhập Tailscale** (mở trang đăng nhập của họ);
- đăng nhập rồi mà đang tắt thì có nút **Bật kết nối**;
- xong xuôi thì khung hiện địa chỉ `100.x.y.z:47823` của máy này để đọc cho
  người kia, kèm danh sách các máy khác trong tailnet — bấm một máy là địa chỉ
  tự điền vào ô kết nối.

Máy chia sẻ vẫn bấm **Bắt đầu chia sẻ** như thường; mật khẩu phiên vẫn phải khớp.

## Nối qua Internet không cần Tailscale

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
