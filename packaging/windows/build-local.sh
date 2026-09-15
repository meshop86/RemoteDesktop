#!/usr/bin/env bash
# Dựng bản Windows ngay trên máy macOS, không tốn phút GitHub Actions.
#
# Cách làm: cargo-xwin tải bộ SDK + thư viện C của MSVC về (một lần, nằm trong
# ~/.cache/cargo-xwin), rồi biên dịch bằng clang-cl và nối bằng lld-link của
# LLVM. Kết quả là file exe thật, chạy được trên Windows như bản build bằng
# Visual Studio.
#
# Cần cài trước, mỗi máy một lần:
#     brew install llvm lld makensis
#     cargo install cargo-xwin
#     rustup target add x86_64-pc-windows-msvc
#
# Dùng:  packaging/windows/build-local.sh [phiên_bản]
set -euo pipefail

version="${1:-$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)}"
target=x86_64-pc-windows-msvc
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"

export PATH="/opt/homebrew/opt/llvm/bin:/opt/homebrew/opt/lld/bin:$PATH"
for tool in clang-cl lld-link llvm-lib; do
    command -v "$tool" >/dev/null || {
        echo "thiếu $tool — chạy: brew install llvm lld" >&2
        exit 1
    }
done

cargo xwin build --release --target "$target" -p rd-app -p rd-signal

out="$root/dist/windows"
rm -rf "$out"
mkdir -p "$out"
cp "target/$target/release/remote-desktop.exe" "$out/"
cp "target/$target/release/rd-rendezvous.exe" "$out/"
cp "$root/packaging/windows/tu-kiem.ps1" "$out/"

zip="$root/dist/RemoteDesktop-$version-windows.zip"
rm -f "$zip"
(cd "$out" && zip -q -r "$zip" .)

echo "xong: $zip"
shasum -a 256 "$zip"

# Bộ cài mới là thứ nên đưa cho người dùng: gói .zip hay bị mở thẳng trong
# Explorer, chương trình chạy từ %TEMP% và mất tăm sau lần khởi động sau, nên
# mọi thứ dựa vào đường dẫn cố định (tự mở lại khi có điện) đều gãy.
if command -v makensis >/dev/null; then
    setup="$root/dist/RemoteDesktop-$version-windows-bo-cai.exe"
    rm -f "$setup"
    # Kèm bộ cài Tailscale chính thức, tải một lần rồi giữ trong cache.
    ts_msi="$HOME/Library/Caches/rd-tailscale/tailscale-setup-amd64.msi"
    if [ ! -f "$ts_msi" ]; then
        mkdir -p "$(dirname "$ts_msi")"
        curl -fL -o "$ts_msi" https://pkgs.tailscale.com/stable/tailscale-setup-latest-amd64.msi
    fi
    makensis -V2 \
        -DTAILSCALE_MSI="$ts_msi" \
        -DVERSION="$version" \
        -DSRC="$out" \
        -DOUT="$setup" \
        "$root/packaging/windows/installer.nsi"
    echo "xong: $setup"
    shasum -a 256 "$setup"
else
    echo "bỏ qua bộ cài — chưa có makensis (brew install makensis)" >&2
fi
