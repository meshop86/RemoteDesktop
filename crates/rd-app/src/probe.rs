//! Dò xem một máy trong tailnet có đang mở phần mềm này không.
//!
//! Danh sách Tailscale chỉ nói máy bật hay tắt. Máy bật mà chưa ai mở phần mềm
//! thì bấm vào vẫn chỉ nhận về một quãng chờ dài rồi báo lỗi — với người có
//! hàng chục máy trong tài khoản, đó là kiểu mò kim đáy bể.
//!
//! Cách dò: gõ cửa bằng một nhãn giao thức (ALPN) *khác* nhãn thật. Bên kia có
//! phần mềm này thì QUIC bắt tay tới bước chọn nhãn, thấy không khớp và đóng
//! ngay — ta nhận được câu trả lời, thế là đủ biết có người nghe. Không có gì
//! nghe ở cổng đó thì im lặng cho tới hết giờ chờ.
//!
//! Cố ý *không* gõ bằng nhãn thật: làm vậy là chiếm mất suất viewer duy nhất
//! của host trong một khoảnh khắc, và bên kia sẽ thấy dòng "viewer đã nối" mỗi
//! lần ta dò.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Nhãn giao thức riêng của phép dò. Không trùng `rd/1` là cố ý.
const ALPN: &[u8] = b"rd-do/1";

/// Chờ trả lời bao lâu. Trong tailnet, kể cả đi vòng qua DERP ở nước ngoài thì
/// một lượt đi về cũng chỉ cỡ vài trăm mili giây.
const CHO: Duration = Duration::from_millis(1500);

/// Kết quả cũ hơn ngần này thì dò lại. Người ta mở phần mềm ở máy kia xong
/// quay sang đây thì chờ chừng này là thấy.
const LAM_MOI: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tinh {
    /// Chưa có câu trả lời nào.
    Dang,
    /// Có người nghe ở cổng ấy.
    Co,
    /// Gõ mà không ai đáp.
    Khong,
}

struct KetQua {
    tinh: Tinh,
    luc: Instant,
    dang_do: bool,
}

/// Tay cầm phía giao diện. Tự dựng luồng nền ở lần hỏi đầu tiên, nên máy không
/// dùng tới Tailscale thì không tốn thêm luồng nào.
#[derive(Default)]
pub struct Prober {
    inner: Option<Inner>,
}

struct Inner {
    yeu_cau: tokio::sync::mpsc::UnboundedSender<SocketAddr>,
    ket_qua: Arc<Mutex<HashMap<SocketAddr, KetQua>>>,
    repaint: Arc<OnceLock<egui::Context>>,
}

impl Prober {
    /// Hỏi tình trạng của một địa chỉ, đồng thời đặt lịch dò lại nếu số liệu
    /// đã cũ. Trả về ngay, không chặn giao diện.
    pub fn hoi(&mut self, addr: SocketAddr) -> Tinh {
        let inner = self.inner.get_or_insert_with(Inner::spawn);
        let Ok(mut map) = inner.ket_qua.lock() else {
            return Tinh::Dang;
        };
        let muc = map.entry(addr).or_insert(KetQua {
            tinh: Tinh::Dang,
            // Lùi lại quá hạn để lần hỏi đầu tiên là dò luôn.
            luc: Instant::now() - LAM_MOI,
            dang_do: false,
        });
        if !muc.dang_do && muc.luc.elapsed() >= LAM_MOI {
            muc.dang_do = true;
            let _ = inner.yeu_cau.send(addr);
        }
        muc.tinh
    }

    /// Cho luồng nền biết cửa sổ nào cần vẽ lại khi có kết quả mới.
    pub fn attach(&self, ctx: &egui::Context) {
        if let Some(inner) = &self.inner {
            let _ = inner.repaint.set(ctx.clone());
        }
    }
}

impl Inner {
    fn spawn() -> Self {
        let (yeu_cau, mut rx) = tokio::sync::mpsc::unbounded_channel::<SocketAddr>();
        let ket_qua: Arc<Mutex<HashMap<SocketAddr, KetQua>>> = Arc::default();
        let repaint: Arc<OnceLock<egui::Context>> = Arc::default();

        let (bang, ve) = (ket_qua.clone(), repaint.clone());
        let dung = std::thread::Builder::new()
            .name("rd-do".into())
            .spawn(move || {
                // Một luồng là đủ: phép dò gần như chỉ ngồi chờ mạng.
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(err) => {
                        tracing::warn!(%err, "không dựng được luồng dò máy");
                        return;
                    }
                };
                runtime.block_on(async move {
                    while let Some(addr) = rx.recv().await {
                        let (bang, ve) = (bang.clone(), ve.clone());
                        // Dò song song: danh sách dài mà xếp hàng thì máy cuối
                        // phải chờ hàng chục giây mới có chấm.
                        tokio::spawn(async move {
                            let tinh = if go_cua(addr).await {
                                Tinh::Co
                            } else {
                                Tinh::Khong
                            };
                            if let Ok(mut map) = bang.lock() {
                                map.insert(
                                    addr,
                                    KetQua {
                                        tinh,
                                        luc: Instant::now(),
                                        dang_do: false,
                                    },
                                );
                            }
                            if let Some(ctx) = ve.get() {
                                ctx.request_repaint();
                            }
                        });
                    }
                });
            })
            .is_ok();
        if !dung {
            tracing::warn!("không tạo được luồng dò máy");
        }

        Self {
            yeu_cau,
            ket_qua,
            repaint,
        }
    }
}

/// Gõ cửa một địa chỉ. `true` nghĩa là có thứ gì đó nói được QUIC ở đó.
async fn go_cua(addr: SocketAddr) -> bool {
    let bind: SocketAddr = if addr.is_ipv4() {
        (std::net::Ipv4Addr::UNSPECIFIED, 0).into()
    } else {
        (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
    };
    let Ok(config) = rd_transport::client_config_for(None, ALPN) else {
        return false;
    };
    let Ok(mut endpoint) = rd_transport::quinn::Endpoint::client(bind) else {
        return false;
    };
    endpoint.set_default_client_config(config);
    let Ok(dang_noi) = endpoint.connect(addr, crate::net::SERVER_NAME) else {
        return false;
    };
    // Đáp kiểu gì cũng là có người nghe — kể cả đáp bằng lời từ chối, vì từ
    // chối nghĩa là bên kia đã đọc tới nhãn giao thức. Chỉ im lặng mới là không.
    tokio::time::timeout(CHO, dang_noi).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gõ cửa đúng một host thật thì phải nhận ra là có người nghe, còn gõ vào
    /// một cổng bỏ không thì phải im.
    #[tokio::test]
    async fn nhan_ra_may_dang_mo_phan_mem() {
        let identity = rd_transport::generate_self_signed("rd-host").expect("tạo được chứng chỉ");
        let endpoint =
            rd_transport::server_endpoint((std::net::Ipv4Addr::LOCALHOST, 0).into(), &identity)
                .expect("mở được cổng");
        let addr = endpoint.local_addr().expect("biết cổng đang nghe");
        // Host thật cũng chỉ đứng ở `accept()`; không cần phục vụ gì thêm để
        // trả lời được phép dò, vì nó dừng ở bước chọn nhãn giao thức.
        tokio::spawn(async move { while endpoint.accept().await.is_some() {} });
        assert!(go_cua(addr).await, "host đang nghe mà dò không thấy");

        // Cổng vừa đóng: mở rồi thả ngay để chắc chắn không ai chiếm.
        let trong =
            rd_transport::quinn::Endpoint::client((std::net::Ipv4Addr::LOCALHOST, 0).into())
                .expect("mở được cổng");
        let addr = trong.local_addr().expect("biết cổng");
        drop(trong);
        assert!(!go_cua(addr).await, "cổng bỏ không mà dò lại thấy");
    }
}
