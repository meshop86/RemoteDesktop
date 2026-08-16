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

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::SignatureScheme;
use rustls_pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime,
};

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
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
