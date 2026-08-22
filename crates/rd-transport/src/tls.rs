//! Chứng chỉ cho QUIC.
//!
//! Hai máy cá nhân kết nối trực tiếp thì không có CA nào ký chứng chỉ cho
//! chúng, nên host tự sinh chứng chỉ tự ký mỗi lần chạy. Danh tính thật sự
//! được xác thực ở lớp trên (Noise + mật khẩu phiên), còn TLS ở đây chỉ làm
//! nhiệm vụ dựng kênh mã hoá.
//!
//! Viewer vì thế phải chấp nhận chứng chỉ lạ — nhưng vẫn ghim (pin) được dấu
//! vân tay của host đã ghép cặp trước đó để chống man-in-the-middle.

use std::sync::Arc;

use rustls::SignatureScheme;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};

use crate::TransportError;

/// Cặp chứng chỉ + khoá riêng tự sinh cho một phiên chạy của host.
pub struct SelfSignedIdentity {
    pub cert_chain: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
    /// SHA-256 của chứng chỉ DER — dùng để viewer ghim host.
    pub fingerprint: [u8; 32],
}

pub fn generate_self_signed(subject: &str) -> Result<SelfSignedIdentity, TransportError> {
    let certified = rcgen::generate_simple_self_signed(vec![subject.to_string()])
        .map_err(|e| TransportError::Tls(e.to_string()))?;
    let cert_der = CertificateDer::from(certified.cert);
    let fingerprint = fingerprint_of(&cert_der);
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        certified.signing_key.serialize_der(),
    ));
    Ok(SelfSignedIdentity {
        cert_chain: vec![cert_der],
        key,
        fingerprint,
    })
}

/// Vân tay chứng chỉ. Dùng BLAKE3 vì cả hai đầu đều là phần mềm của ta (không
/// cần tương thích chuẩn ngoài) và BLAKE3 nhanh hơn SHA-256 đáng kể.
pub fn fingerprint_of(cert_der: &[u8]) -> [u8; 32] {
    *blake3::hash(cert_der).as_bytes()
}

/// Rút gọn vân tay thành chuỗi ngắn cho người dùng đối chiếu bằng mắt.
pub fn fingerprint_short(fingerprint: &[u8; 32]) -> String {
    fingerprint[..6]
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Vân tay đầy đủ dạng hex liền, để dán vào cấu hình client mà ghim.
pub fn fingerprint_hex(fingerprint: &[u8; 32]) -> String {
    fingerprint.iter().map(|b| format!("{b:02x}")).collect()
}

/// Đọc lại chuỗi hex thành vân tay. Chấp nhận cả dạng có `:` cho dễ nhìn, và
/// không phân biệt hoa thường — người dùng chép tay thì kiểu gì cũng lẫn.
pub fn parse_fingerprint(text: &str) -> Option<[u8; 32]> {
    let clean: String = text
        .chars()
        .filter(|c| !matches!(c, ':' | ' ' | '-'))
        .collect();
    if clean.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (slot, pair) in out.iter_mut().zip(clean.as_bytes().chunks(2)) {
        *slot = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(out)
}

pub fn default_provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Verifier chấp nhận chứng chỉ tự ký, có tuỳ chọn ghim vân tay.
///
/// `expected_fingerprint = None` chỉ dùng cho lần ghép cặp đầu tiên hoặc trong
/// mạng LAN tin cậy; sau khi ghép, client lưu vân tay và ghim từ lần sau.
#[derive(Debug)]
pub struct PinnedServerVerifier {
    expected_fingerprint: Option<[u8; 32]>,
    provider: Arc<CryptoProvider>,
}

impl PinnedServerVerifier {
    pub fn new(expected_fingerprint: Option<[u8; 32]>) -> Arc<Self> {
        Arc::new(Self {
            expected_fingerprint,
            provider: default_provider(),
        })
    }
}

impl ServerCertVerifier for PinnedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if let Some(expected) = self.expected_fingerprint {
            let actual = fingerprint_of(end_entity);
            if actual != expected {
                return Err(rustls::Error::General(
                    "vân tay chứng chỉ của host không khớp với máy đã ghép cặp".into(),
                ));
            }
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

// ─────────────────── danh tính lưu lại giữa các lần chạy ───────────────────

/// Nhận dạng file danh tính. Có nó thì đọc nhầm một file khác cũng biết ngay.
const IDENTITY_MAGIC: &[u8; 8] = b"rd-ident";

/// Đọc danh tính đã lưu ở `path`; chưa có (hoặc file hỏng) thì sinh mới và lưu.
///
/// Sinh mới mỗi lần chạy thì vân tay đổi mỗi lần chạy, mà vân tay đổi thì
/// chẳng ai ghim được — người dùng sẽ quen tay bấm "vẫn nối" cho mọi cảnh báo,
/// và cảnh báo nào cũng bị bấm qua thì bằng không có. Giữ nguyên một danh tính
/// là điều kiện cần để chống được kẻ đứng giữa.
///
/// Lưu hỏng (đĩa đầy, thư mục chỉ đọc) không phải lý do để không chạy được:
/// vẫn trả về danh tính vừa sinh, chỉ mất tính ổn định qua các lần chạy.
pub fn load_or_create_identity(
    path: &std::path::Path,
    subject: &str,
) -> Result<SelfSignedIdentity, TransportError> {
    if let Some(identity) = read_identity(path) {
        return Ok(identity);
    }
    let identity = generate_self_signed(subject)?;
    if let Err(err) = write_identity(path, &identity) {
        tracing::warn!(%err, path = %path.display(), "không lưu được danh tính, vân tay sẽ đổi ở lần chạy sau");
    }
    Ok(identity)
}

fn read_identity(path: &std::path::Path) -> Option<SelfSignedIdentity> {
    let raw = std::fs::read(path).ok()?;
    let rest = raw.strip_prefix(IDENTITY_MAGIC)?;
    let (cert_der, rest) = take_block(rest)?;
    let (key_der, rest) = take_block(rest)?;
    if !rest.is_empty() {
        return None;
    }
    let cert_der = CertificateDer::from(cert_der.to_vec());
    let fingerprint = fingerprint_of(&cert_der);
    Some(SelfSignedIdentity {
        cert_chain: vec![cert_der],
        key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der.to_vec())),
        fingerprint,
    })
}

fn take_block(raw: &[u8]) -> Option<(&[u8], &[u8])> {
    let (len, rest) = raw.split_at_checked(4)?;
    let len = u32::from_le_bytes(len.try_into().ok()?) as usize;
    let (block, rest) = rest.split_at_checked(len)?;
    Some((block, rest))
}

fn write_identity(
    path: &std::path::Path,
    identity: &SelfSignedIdentity,
) -> Result<(), TransportError> {
    let cert = identity
        .cert_chain
        .first()
        .ok_or_else(|| TransportError::Tls("danh tính không có chứng chỉ".into()))?;
    let mut out = Vec::with_capacity(cert.len() + identity.key.secret_der().len() + 16);
    out.extend_from_slice(IDENTITY_MAGIC);
    out.extend_from_slice(&(cert.len() as u32).to_le_bytes());
    out.extend_from_slice(cert);
    out.extend_from_slice(&(identity.key.secret_der().len() as u32).to_le_bytes());
    out.extend_from_slice(identity.key.secret_der());

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Ghi ra file tạm rồi đổi tên: mất điện giữa chừng thì file cũ vẫn nguyên,
    // chứ không để lại một danh tính cụt mà lần sau đọc không ra.
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &out)?;
    restrict_to_owner(&tmp)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Khoá riêng nằm trong đó: chỉ chủ máy được đọc.
#[cfg(unix)]
fn restrict_to_owner(path: &std::path::Path) -> Result<(), TransportError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Windows thừa kế quyền của thư mục hồ sơ người dùng, vốn đã kín với người
/// khác — không có `chmod` để siết thêm.
#[cfg(not(unix))]
fn restrict_to_owner(_path: &std::path::Path) -> Result<(), TransportError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("rd-test-{name}-{}", std::process::id()));
        path
    }

    #[test]
    fn danh_tinh_giu_nguyen_van_tay_qua_cac_lan_chay() {
        let path = temp_path("identity");
        std::fs::remove_file(&path).ok();

        let first = load_or_create_identity(&path, "rd-test").expect("sinh danh tính");
        let second = load_or_create_identity(&path, "rd-test").expect("đọc lại danh tính");
        assert_eq!(first.fingerprint, second.fingerprint);
        assert_eq!(first.cert_chain[0], second.cert_chain[0]);
        assert_eq!(first.key.secret_der(), second.key.secret_der());

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn file_hong_thi_sinh_lai_chu_khong_chet() {
        let path = temp_path("identity-hong");
        std::fs::write(&path, b"khong phai danh tinh").expect("ghi file rác");

        let identity = load_or_create_identity(&path, "rd-test").expect("phải sinh lại được");
        let again = load_or_create_identity(&path, "rd-test").expect("đọc lại");
        assert_eq!(identity.fingerprint, again.fingerprint);

        std::fs::remove_file(&path).ok();
    }
}

#[cfg(test)]
mod van_tay_tests {
    use super::*;

    #[test]
    fn hex_di_va_ve_khong_doi() {
        let identity = generate_self_signed("test").unwrap();
        let text = fingerprint_hex(&identity.fingerprint);
        assert_eq!(parse_fingerprint(&text), Some(identity.fingerprint));
        let co_dau = text
            .as_bytes()
            .chunks(2)
            .map(|p| std::str::from_utf8(p).unwrap().to_uppercase())
            .collect::<Vec<_>>()
            .join(":");
        assert_eq!(parse_fingerprint(&co_dau), Some(identity.fingerprint));
    }

    #[test]
    fn chuoi_sai_thi_tu_choi() {
        assert_eq!(parse_fingerprint(""), None);
        assert_eq!(parse_fingerprint("abcd"), None);
        assert_eq!(parse_fingerprint(&"zz".repeat(32)), None);
    }
}
