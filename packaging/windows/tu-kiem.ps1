# Tự kiểm bản Windows: clipboard, mục tự khởi động, và chế độ chạy ngầm.
#
# Máy dựng bản này là macOS nên ba thứ trên chỉ máy Windows thật mới trả lời
# được. Chạy file này trong thư mục vừa giải nén:
#
#     powershell -ExecutionPolicy Bypass -File tu-kiem.ps1
#
# Rồi gửi lại toàn bộ những gì nó in ra.

$ErrorActionPreference = "Stop"
# Windows PowerShell 5.1 mặc định nói chuyện với tiến trình con bằng bảng mã
# ANSI: không đặt lại thì chữ có dấu trong nhật ký vỡ hết, và phép so chuỗi ở
# dưới báo hỏng dù thực tế chạy đúng. (File này cũng phải có BOM UTF-8 vì lý do
# tương tự — 5.1 đọc .ps1 không BOM theo ANSI.)
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$OutputEncoding = [Text.Encoding]::UTF8
$exe = Join-Path $PSScriptRoot "remote-desktop.exe"
if (-not (Test-Path $exe)) { throw "không thấy remote-desktop.exe cạnh script này" }

"=== 1. Tự kiểm nội bộ ==="
& $exe --tu-kiem 2>&1 | Out-String | Write-Output
if ($LASTEXITCODE -ne 0) { "==> --tu-kiem thoát với mã $LASTEXITCODE" }

"`n=== 2. Clipboard đi từ máy chia sẻ sang máy điều khiển ==="
# Hai tiến trình trên cùng một máy: một chia sẻ, một điều khiển. Cả hai ghi
# chung một file log nên đọc một chỗ là thấy cả hai chiều.
$log = "$env:LOCALAPPDATA\RemoteDesktop\rd.log"
Remove-Item $log -ErrorAction SilentlyContinue

$luu = $null
try { $luu = Get-Clipboard -Raw } catch { }

$chiaSe = Start-Process $exe -ArgumentList "--host","--password","tu-kiem","--port","47901" -PassThru
Start-Sleep -Seconds 8
$dieuKhien = Start-Process $exe -ArgumentList "--connect","127.0.0.1:47901","--password","tu-kiem" -PassThru
Start-Sleep -Seconds 10

$moc = "RD-TU-KIEM-$(Get-Random)"
Set-Clipboard -Value $moc
Start-Sleep -Seconds 5

Stop-Process -Id $dieuKhien.Id -Force -ErrorAction SilentlyContinue
Stop-Process -Id $chiaSe.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 2
if ($null -ne $luu) { Set-Clipboard -Value $luu }

$noiDung = if (Test-Path $log) { Get-Content $log -Raw -Encoding UTF8 } else { "" }
$gui = $noiDung -match "máy này vừa copy"
$nhan = $noiDung -match "đã dán clipboard của máy kia"
"máy chia sẻ thấy nội dung vừa copy : $(if ($gui) { 'ĐẠT' } else { 'KHÔNG ĐẠT' })"
"máy điều khiển dán được nội dung   : $(if ($nhan) { 'ĐẠT' } else { 'KHÔNG ĐẠT' })"
if (-not ($gui -and $nhan)) {
    "`n--- nhật ký để soi lỗi ---"
    $noiDung
}

"`n=== 3. Còn hai việc phải làm tay ==="
@"
  a) Tích ô "Máy bật lại là tự chia sẻ tiếp" ở thẻ chia sẻ (hoặc cài bằng bộ
     cài, mục đó tự bật), rồi khởi động lại máy: đăng nhập xong phần mềm phải
     tự lên, cửa sổ thu nhỏ, đã ở chế độ chia sẻ. Muốn máy mất điện bật lại là
     chạy được ngay mà không ai gõ mật khẩu Windows thì bấm thêm nút "Bật đăng
     nhập tự động…" ngay dưới ô ấy.
  b) Copy ở máy này, dán ở máy kia (hai máy thật, không phải hai cửa sổ).
"@
