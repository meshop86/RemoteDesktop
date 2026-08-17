#!/usr/bin/env bash
#
# Đóng gói bản macOS thành .app rồi bỏ vào .dmg.
#
# Phải là .app chứ không phải file chạy trần: quyền Screen Recording và
# Accessibility của macOS gắn với *ứng dụng*, và hệ điều hành chỉ ghi nhớ được
# lựa chọn của người dùng khi thứ xin quyền là một bundle có mã định danh riêng.
# Chạy file trần thì mỗi lần build lại là mỗi lần phải cấp quyền lại.
#
# Dùng:  packaging/macos/bundle.sh [phiên bản] [đích...]
# Đích mặc định là kiến trúc của máy đang chạy; truyền cả hai để ra bản universal:
#   packaging/macos/bundle.sh 0.1.0 aarch64-apple-darwin x86_64-apple-darwin

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
version="${1:-0.1.0}"
shift || true
targets=("$@")
if [ ${#targets[@]} -eq 0 ]; then
	case "$(uname -m)" in
	arm64) targets=(aarch64-apple-darwin) ;;
	*) targets=(x86_64-apple-darwin) ;;
	esac
fi

dist="$root/dist"
app="$dist/Remote Desktop.app"
macos_dir="$app/Contents/MacOS"

rm -rf "$app"
mkdir -p "$macos_dir" "$app/Contents/Resources"

binaries=()
for target in "${targets[@]}"; do
	echo ">> build $target"
	(cd "$root" && cargo build --release --target "$target" -p rd-app -p rd-signal)
	binaries+=("$root/target/$target/release/remote-desktop")
done

if [ ${#binaries[@]} -gt 1 ]; then
	lipo -create -output "$macos_dir/remote-desktop" "${binaries[@]}"
else
	cp "${binaries[0]}" "$macos_dir/remote-desktop"
fi
chmod +x "$macos_dir/remote-desktop"

# Biểu tượng dựng ngay từ file PNG chung với bản Windows, khỏi giữ thêm một file
# nhị phân trong repo. sips và iconutil máy macOS nào cũng có sẵn.
iconset="$(mktemp -d)/icon.iconset"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
	sips -z "$size" "$size" "$root/packaging/icon.png" \
		--out "$iconset/icon_${size}x${size}.png" >/dev/null
	sips -z "$((size * 2))" "$((size * 2))" "$root/packaging/icon.png" \
		--out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/icon.icns"
rm -rf "$(dirname "$iconset")"

sed "s/__VERSION__/$version/g" "$root/packaging/macos/Info.plist" >"$app/Contents/Info.plist"
printf 'APPL????' >"$app/Contents/PkgInfo"

# Ký tạm (ad-hoc). Không phải chứng chỉ Developer ID nên máy khác vẫn hỏi lại
# lần đầu mở, nhưng chữ ký giúp macOS nhận diện ứng dụng theo danh tính thay vì
# theo đường dẫn — cấp quyền một lần là xong, kể cả khi đổi chỗ để file.
codesign --force --deep --sign - --identifier com.luongxuanhoa.remotedesktop "$app"

# Server rendezvous đi kèm dưới dạng file chạy rời: nó không có giao diện, và
# ai tự dựng server mới cần tới.
for target in "${targets[@]}"; do
	cp "$root/target/$target/release/rd-rendezvous" "$dist/rd-rendezvous-$target"
done

dmg="$dist/RemoteDesktop-$version-macos.dmg"
rm -f "$dmg"
staging="$(mktemp -d)"
cp -R "$app" "$staging/"
ln -s /Applications "$staging/Applications"
hdiutil create -volname "Remote Desktop" -srcfolder "$staging" -ov -format UDZO "$dmg" >/dev/null
rm -rf "$staging"

echo "xong: $dmg"
