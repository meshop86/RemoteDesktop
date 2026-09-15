//! Ô thao tác nhanh: một cửa sổ tí hon luôn nổi trên mọi cửa sổ khác.
//!
//! Việc hay làm nhất giữa hai máy đã nối là ném cho nhau một tệp. Nếu mỗi lần
//! đều phải lôi cửa sổ chính từ thanh tác vụ lên, tìm đúng bảng rồi mới thả
//! được thì thao tác ba giây hoá ra ba mươi giây. Ô này nằm sẵn ở góc màn
//! hình: nhìn là biết kết nối còn sống hay không, và thả tệp vào là tệp đi.
//!
//! Nó phải là một *cửa sổ riêng* (viewport của egui) chứ không phải một khung
//! vẽ nằm trong cửa sổ chính — cửa sổ chính thu xuống thanh tác vụ thì mọi thứ
//! vẽ bên trong nó biến mất theo, mà đúng lúc ấy mới là lúc cần ô này nhất.
//!
//! Tệp thả vào đây đi thẳng xuống phần mạng qua một bản sao của đầu gửi lệnh,
//! không chờ vòng vẽ của cửa sổ chính: cửa sổ ấy đang thu nhỏ thì hệ điều hành
//! cho nó chạy chậm hẳn lại, có khi đứng im.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use eframe::egui;

use crate::net::UiCommand;
use crate::ui;

/// Bề ngang và chiều cao ô, tính bằng điểm giao diện.
const RONG: f32 = 248.0;
const CAO: f32 = 64.0;

/// Cách mép màn hình bấy nhiêu khi chưa ai kéo ô đi đâu.
const LE: f32 = 28.0;

/// Nhịp vẽ lại của ô. Nó chỉ có tiến độ tệp và một chấm trạng thái, không cần
/// nhanh hơn — mà cửa sổ luôn nổi thì càng phải tiết kiệm điện.
const NHIP_MS: u64 = 250;

/// Cửa sổ con cần một định danh cố định giữa các lượt vẽ, nếu không mỗi lượt
/// egui lại dựng một cửa sổ mới.
fn ma() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("thao-tac-nhanh")
}

/// Trạng thái phiên rút gọn — vừa đủ cho ô nhỏ, không hơn.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tin {
    /// Đã nối được với máy kia chưa.
    pub noi: bool,
    /// Tên máy kia, rỗng khi chưa biết.
    pub ten: String,
    /// Một dòng ngắn nói đang xảy ra gì.
    pub trang_thai: String,
    /// Tệp đang chuyển: tên và phần đã xong (0..1).
    pub dang_chuyen: Option<(String, f32)>,
    /// Tin nhắn và lời mời gửi tệp chưa xem.
    pub chua_doc: u32,
}

#[derive(Default)]
struct Trong {
    tin: Tin,
    /// Đầu gửi lệnh xuống phần mạng. `None` là chưa vào phiên nào.
    gui: Option<tokio::sync::mpsc::UnboundedSender<UiCommand>>,
    /// Người dùng đã tự tay đóng ô. Giữ tới hết phiên.
    tat: bool,
    /// Chỗ sẽ mở ô ra. Chỉ đọc lúc dựng cửa sổ, sau đó giữ nguyên: đổi giá trị
    /// này giữa chừng là egui gửi lệnh dời cửa sổ, giật ngay dưới tay người
    /// đang kéo nó.
    goc: Option<egui::Pos2>,
    /// Chỗ ô đang đứng thật, ô tự ghi vào mỗi lượt vẽ.
    dang_o: Option<egui::Pos2>,
    /// Vị trí đã ghi vào tệp thiết lập, để biết lúc nào cần ghi lại.
    da_ghi: Option<egui::Pos2>,
    /// Ô có đang được vẽ không, để biết lúc nó vừa mở ra.
    dang_hien: bool,
    /// Đang kéo ô đi: chỗ ngón tay nắm vào, tính từ góc trên trái của ô.
    nam: Option<egui::Vec2>,
    /// Số tệp đang lơ lửng trên ô ở lượt vẽ trước, chỉ để ghi nhật ký một lần.
    lo_lung_truoc: usize,
}

/// Tay cầm dùng chung giữa cửa sổ chính và ô nhỏ.
///
/// Ô nhỏ được egui gọi ở một lượt vẽ khác (cửa sổ khác, có khi lúc cửa sổ
/// chính đang ngủ), nên mọi thứ hai bên cùng đụng tới phải nằm sau một khoá.
#[derive(Clone, Default)]
pub struct Bang(Arc<Mutex<Trong>>);

impl Bang {
    /// `goc` là chỗ lần chạy trước người dùng để ô lại.
    pub fn moi(goc: Option<egui::Pos2>) -> Self {
        let bang = Self::default();
        bang.0.lock().unwrap().goc = goc;
        bang
    }

    fn khoa(&self) -> std::sync::MutexGuard<'_, Trong> {
        // Khoá này chỉ ôm mấy phép gán; luồng nào giữ nó cũng nhả ngay. Bị
        // đầu độc thì nghĩa là một lượt vẽ đã panic — cứ đọc tiếp còn hơn kéo
        // cả chương trình chết theo một cái ô trang trí.
        self.0.lock().unwrap_or_else(|hong| hong.into_inner())
    }

    pub fn tin(&self) -> Tin {
        self.khoa().tin.clone()
    }

    pub fn cap_nhat(&self, tin: Tin) {
        self.khoa().tin = tin;
    }

    /// Phiên mới bắt đầu: nối đường gửi lệnh và cho ô hiện lại.
    pub fn vao_phien(&self, gui: tokio::sync::mpsc::UnboundedSender<UiCommand>) {
        let mut trong = self.khoa();
        trong.gui = Some(gui);
        trong.tat = false;
    }

    /// Phiên mới dựng lại phần mạng sau khi rớt: đường cũ đã chết.
    pub fn doi_duong(&self, gui: tokio::sync::mpsc::UnboundedSender<UiCommand>) {
        self.khoa().gui = Some(gui);
    }

    /// Rời phiên: cắt đường gửi và xoá trạng thái cũ, đừng để ô còn khoe tên
    /// máy vừa ngắt.
    pub fn roi_phien(&self) {
        let mut trong = self.khoa();
        trong.gui = None;
        trong.tin = Tin::default();
        trong.tat = false;
    }

    pub fn tat_roi(&self) -> bool {
        self.khoa().tat
    }

    pub fn tat_di(&self) {
        self.khoa().tat = true;
    }

    pub fn bat_lai(&self) {
        self.khoa().tat = false;
    }

    /// Bắt đầu kéo ô: nhớ chỗ ngón tay nắm vào để lát nữa còn đặt ô cho khớp.
    fn bat_dau_keo(&self, nam: egui::Vec2) {
        self.khoa().nam = Some(nam);
    }

    fn cho_nam(&self) -> Option<egui::Vec2> {
        self.khoa().nam
    }

    fn thoi_keo(&self) {
        self.khoa().nam = None;
    }

    /// Vị trí cần ghi vào thiết lập, hoặc `None` khi không có gì mới.
    pub fn vi_tri_moi(&self) -> Option<egui::Pos2> {
        let mut trong = self.khoa();
        let dang_o = trong.dang_o?;
        if trong.da_ghi == Some(dang_o) {
            return None;
        }
        trong.da_ghi = Some(dang_o);
        Some(dang_o)
    }

    /// Gửi tệp đi ngay, không đợi cửa sổ chính vẽ lượt tiếp theo.
    fn gui_tep(&self, danh_sach: Vec<PathBuf>) {
        let trong = self.khoa();
        let Some(gui) = trong.gui.as_ref() else {
            return;
        };
        for path in danh_sach {
            tracing::info!(path = %path.display(), "thả tệp vào ô thao tác nhanh");
            let _ = gui.send(UiCommand::SendFile(path));
        }
    }
}

/// Vẽ ô. Gọi mỗi lượt vẽ của cửa sổ chính khi còn muốn ô tồn tại — thôi gọi là
/// egui đóng cửa sổ con giúp.
pub fn ve(ctx: &egui::Context, bang: &Bang) {
    let goc = {
        let mut trong = bang.khoa();
        // Ô vừa mở lại trong cùng một lần chạy thì mở đúng chỗ nó vừa nằm.
        if !trong.dang_hien {
            trong.dang_hien = true;
            if let Some(dang_o) = trong.dang_o {
                trong.goc = Some(dang_o);
            }
            tracing::info!(goc = ?trong.goc, "mở ô thao tác nhanh");
        }
        trong.goc
    };
    let goc = goc.unwrap_or_else(|| goc_mac_dinh(ctx));

    let builder = egui::ViewportBuilder::default()
        .with_title("Thao tác nhanh")
        .with_inner_size([RONG, CAO])
        .with_position(goc)
        // Không khung viền: một thanh tiêu đề của hệ điều hành còn cao hơn cả
        // cái ô này.
        .with_decorations(false)
        .with_resizable(false)
        .with_always_on_top()
        // Đừng chiếm thêm một ô trên thanh tác vụ: cửa sổ chính đã có một ô ở
        // đó rồi, hai ô cho một phần mềm chỉ làm người dùng bấm nhầm.
        .with_taskbar(false)
        .with_drag_and_drop(true)
        // Mở ra mà cướp luôn con trỏ chữ thì đang gõ dở ở phần mềm khác là mất
        // chữ.
        .with_active(false);

    let bang = bang.clone();
    ctx.show_viewport_deferred(ma(), builder, move |ui, lop| noi_dung(ui, &bang, lop));
}

/// Cửa sổ chính thôi vẽ ô: nhớ lấy chỗ nó đang đứng trước khi nó biến mất.
pub fn dong(bang: &Bang) {
    let mut trong = bang.khoa();
    if trong.dang_hien {
        tracing::info!("đóng ô thao tác nhanh");
    }
    trong.dang_hien = false;
}

/// Góc trên bên phải màn hình. Bên phải vì thanh tác vụ của Windows nằm dưới
/// và khay đồng hồ nằm phải — mắt người dùng vốn đã quen liếc về đó.
fn goc_mac_dinh(ctx: &egui::Context) -> egui::Pos2 {
    let man = ctx
        .input(|i| i.viewport().monitor_size)
        .unwrap_or(egui::vec2(1920.0, 1080.0));
    egui::pos2((man.x - RONG - LE).max(LE), LE)
}

fn noi_dung(ui: &mut egui::Ui, bang: &Bang, lop: egui::ViewportClass) {
    let ctx = ui.ctx().clone();
    // Backend nào không mở nổi cửa sổ con thì egui nhúng ô vào cửa sổ chính.
    // Lúc ấy tệp thả xuống rơi vào cửa sổ chính và chính nó đã lo gửi rồi —
    // nhận thêm ở đây là gửi hai lần.
    let rieng = lop != egui::ViewportClass::EmbeddedWindow;

    if rieng {
        let tha: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|tep| tep.path().to_path_buf())
                .collect()
        });
        if !tha.is_empty() {
            bang.gui_tep(tha);
        }
        ghi_vi_tri(&ctx, bang);
    }
    let lo_lung = if rieng {
        ctx.input(|i| i.raw.hovered_files.len())
    } else {
        0
    };
    if rieng {
        ghi_nhat_ky(&ctx, bang, lo_lung);
    }

    let tin = bang.tin();
    let khung = ui.max_rect();
    ve_nen(ui, khung, &tin, lo_lung);

    // Cả nền là chỗ để kéo ô đi: cửa sổ không có thanh tiêu đề thì phải tự lo
    // lấy việc ấy. Đặt trước mọi thứ khác để các nút vẽ sau còn giành được
    // chuột của chúng.
    let nen = ui.interact(khung, ui.id().with("nền"), egui::Sense::click_and_drag());
    if rieng {
        keo_o(&ctx, bang, &nen);
    }
    if nen.double_clicked() {
        mo_cua_so_chinh(&ctx);
    }
    nen.on_hover_text(
        "Kéo tệp thả vào đây để gửi sang máy kia. Bấm đúp để mở cửa sổ chính, kéo để dời ô.",
    );

    if lo_lung > 0 {
        ve_chu_giua(ui, khung, &format!("Thả {lo_lung} tệp để gửi"));
        ctx.request_repaint();
        return;
    }

    let long = khung.shrink(9.0);
    let bieu_tuong = egui::Rect::from_min_size(long.left_top(), egui::vec2(26.0, 26.0));
    ve_bieu_tuong(ui, bieu_tuong, &tin);

    // Hai nút bên phải: mở cửa sổ chính, và đóng ô.
    let mut phai = long.right();
    for (nhan, meo, dong_o) in [
        ("✕", "Ẩn ô này tới hết phiên", true),
        ("⬈", "Mở cửa sổ chính", false),
    ] {
        let o =
            egui::Rect::from_min_size(egui::pos2(phai - 22.0, long.top()), egui::vec2(22.0, 22.0));
        if ui
            .put(o, egui::Button::new(nhan).small())
            .on_hover_text(meo)
            .clicked()
        {
            if dong_o {
                bang.tat_di();
            } else {
                mo_cua_so_chinh(&ctx);
            }
        }
        phai = o.left() - 4.0;
    }

    let chu = egui::Rect::from_min_max(
        egui::pos2(bieu_tuong.right() + 9.0, long.top() - 1.0),
        egui::pos2(phai - 4.0, long.bottom()),
    );
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(chu)
            .layout(egui::Layout::top_down(egui::Align::Min)),
        |ui| ve_chu(ui, &tin),
    );

    // Không có nhịp này thì ô đứng hình: tin tức của nó do cửa sổ chính ghi
    // vào từ một lượt vẽ khác, egui không có cách nào biết mà tự vẽ lại.
    ctx.request_repaint_after(std::time::Duration::from_millis(NHIP_MS));
}

/// Ghi lại chỗ ô đang đứng, nhưng chỉ khi không có ngón tay nào đang giữ chuột:
/// ghi giữa cú kéo là lưu một vị trí người dùng chưa chọn xong.
/// Kéo ô đi bằng tay.
///
/// Không dùng `ViewportCommand::StartDrag`: lệnh ấy của egui chỉ chạy khi cửa
/// sổ đang giữ tiêu điểm, mà ô này cố tình mở ra không cướp tiêu điểm của ai —
/// nên kéo mãi mà ô vẫn đứng im. Tự tính lấy vị trí thì kéo được ngay từ cái
/// chạm đầu tiên.
fn keo_o(ctx: &egui::Context, bang: &Bang, nen: &egui::Response) {
    if nen.drag_started() {
        // Từ giây này người dùng đang thao tác với ô, cho nó nhận tiêu điểm.
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        if let Some(diem) = nen.interact_pointer_pos() {
            bang.bat_dau_keo(diem.to_vec2());
        }
    }
    if nen.drag_stopped() {
        bang.thoi_keo();
        return;
    }
    if !nen.dragged() {
        return;
    }
    let (Some(nam), Some(diem)) = (bang.cho_nam(), nen.interact_pointer_pos()) else {
        return;
    };
    let Some(ngoai) = ctx.input(|i| i.viewport().outer_rect) else {
        return;
    };
    // `diem` tính từ góc ô, `ngoai.min` là chỗ ô đang đứng trên màn hình: cộng
    // lại ra chỗ con trỏ, trừ chỗ đang nắm ra chỗ ô phải tới.
    let toi = ngoai.min + diem.to_vec2() - nam;
    if (toi - ngoai.min).length() >= 0.5 {
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(toi));
    }
}

/// Kể lại những gì hệ điều hành đưa tới ô. Ô là một cửa sổ riêng, chuột và tệp
/// của nó đi đường khác cửa sổ chính — hỏng thì phải xem nhật ký mới biết hỏng
/// ở khúc nào.
fn ghi_nhat_ky(ctx: &egui::Context, bang: &Bang, lo_lung: usize) {
    if ctx.input(|i| i.pointer.any_pressed()) {
        tracing::info!("ô thao tác nhanh: nhận được chuột");
    }
    let mut trong = bang.khoa();
    if trong.lo_lung_truoc != lo_lung {
        if lo_lung > 0 {
            tracing::info!(so_tep = lo_lung, "tệp đang lơ lửng trên ô thao tác nhanh");
        }
        trong.lo_lung_truoc = lo_lung;
    }
}

fn ghi_vi_tri(ctx: &egui::Context, bang: &Bang) {
    if ctx.input(|i| i.pointer.any_down()) {
        return;
    }
    let Some(o) = ctx.input(|i| i.viewport().outer_rect) else {
        return;
    };
    bang.khoa().dang_o = Some(o.min);
}

fn mo_cua_so_chinh(ctx: &egui::Context) {
    let goc = egui::ViewportId::ROOT;
    ctx.send_viewport_cmd_to(goc, egui::ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd_to(goc, egui::ViewportCommand::Focus);
}

fn mau_vien(tin: &Tin, lo_lung: usize) -> egui::Color32 {
    if tin.noi || lo_lung > 0 {
        ui::GOOD
    } else {
        ui::WARN
    }
}

fn ve_nen(ui: &egui::Ui, khung: egui::Rect, tin: &Tin, lo_lung: usize) {
    let painter = ui.painter();
    painter.rect_filled(khung, 10.0, egui::Color32::from_rgb(24, 26, 30));
    painter.rect_stroke(
        khung.shrink(0.5),
        10.0,
        egui::Stroke::new(1.5, mau_vien(tin, lo_lung)),
        egui::StrokeKind::Inside,
    );
}

fn ve_chu_giua(ui: &egui::Ui, khung: egui::Rect, text: &str) {
    ui.painter().text(
        khung.center(),
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(14.0),
        egui::Color32::WHITE,
    );
}

/// Một cái màn hình con con, cộng chấm trạng thái ở góc.
///
/// Vẽ bằng hình khối chứ không kèm tệp ảnh: thêm một tệp ảnh là thêm một thứ
/// phải đóng gói đúng trên cả hai hệ, cho một hình 26 điểm.
fn ve_bieu_tuong(ui: &egui::Ui, o: egui::Rect, tin: &Tin) {
    let painter = ui.painter();
    let mau = if tin.noi { ui::GOOD } else { ui::WARN };
    let man = egui::Rect::from_min_size(
        o.min + egui::vec2(1.0, 2.0),
        egui::vec2(o.width() - 2.0, o.height() - 9.0),
    );
    painter.rect_stroke(
        man,
        3.0,
        egui::Stroke::new(1.6, mau),
        egui::StrokeKind::Inside,
    );
    // Chân đế: một gạch ngang dưới màn hình.
    painter.line_segment(
        [
            egui::pos2(man.center().x - 5.0, o.bottom() - 1.0),
            egui::pos2(man.center().x + 5.0, o.bottom() - 1.0),
        ],
        egui::Stroke::new(1.6, mau),
    );

    if tin.chua_doc > 0 {
        let tam = egui::pos2(o.right(), o.top());
        painter.circle_filled(tam, 6.5, ui::BAD);
        painter.text(
            tam,
            egui::Align2::CENTER_CENTER,
            tin.chua_doc.min(9).to_string(),
            egui::FontId::proportional(9.0),
            egui::Color32::WHITE,
        );
    }
}

fn ve_chu(ui: &mut egui::Ui, tin: &Tin) {
    ui.spacing_mut().item_spacing.y = 2.0;
    let ten = if tin.ten.is_empty() {
        "chưa nối".to_string()
    } else {
        ui::rut_gon(&tin.ten, 22)
    };
    ui.label(egui::RichText::new(ten).strong().size(13.0));

    match &tin.dang_chuyen {
        Some((ten_tep, phan)) => {
            ui.label(
                egui::RichText::new(format!("{} {:.0}%", ui::rut_gon(ten_tep, 18), phan * 100.0))
                    .size(11.0)
                    .color(egui::Color32::from_gray(190)),
            );
            let (o, _) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), 4.0), egui::Sense::hover());
            ui.painter()
                .rect_filled(o, 2.0, egui::Color32::from_gray(60));
            let mut chay = o;
            chay.set_width(o.width() * phan.clamp(0.0, 1.0));
            ui.painter().rect_filled(chay, 2.0, ui::GOOD);
        }
        None => {
            let dong = if tin.noi {
                "kéo tệp thả vào đây".to_string()
            } else if tin.trang_thai.is_empty() {
                "đang chờ".to_string()
            } else {
                ui::rut_gon(&tin.trang_thai, 30)
            };
            ui.label(
                egui::RichText::new(dong)
                    .size(11.0)
                    .color(egui::Color32::from_gray(170)),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tat_roi_thi_giu_toi_khi_bat_lai() {
        let bang = Bang::default();
        assert!(!bang.tat_roi());
        bang.khoa().tat = true;
        assert!(bang.tat_roi());
        bang.bat_lai();
        assert!(!bang.tat_roi());
    }

    #[test]
    fn phien_moi_xoa_tin_cu() {
        let bang = Bang::default();
        bang.cap_nhat(Tin {
            noi: true,
            ten: "máy kia".into(),
            ..Default::default()
        });
        bang.roi_phien();
        assert_eq!(bang.tin(), Tin::default());
    }

    #[test]
    fn vi_tri_chi_bao_mot_lan_cho_moi_cho_moi() {
        let bang = Bang::default();
        assert_eq!(bang.vi_tri_moi(), None);
        bang.khoa().dang_o = Some(egui::pos2(10.0, 20.0));
        assert_eq!(bang.vi_tri_moi(), Some(egui::pos2(10.0, 20.0)));
        assert_eq!(bang.vi_tri_moi(), None);
        bang.khoa().dang_o = Some(egui::pos2(11.0, 20.0));
        assert_eq!(bang.vi_tri_moi(), Some(egui::pos2(11.0, 20.0)));
    }

    /// Vẽ thật một lượt rồi đo: ô chỉ có 248x64 điểm, thêm một dòng chữ dài
    /// là nó tràn ra ngoài mà không ai biết cho tới khi nhìn thấy tận mắt.
    fn ve_thu(tin: Tin) -> egui::Rect {
        let ctx = egui::Context::default();
        let bang = Bang::default();
        bang.cap_nhat(tin);
        let khung = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(RONG, CAO));
        let input = egui::RawInput {
            screen_rect: Some(khung),
            ..Default::default()
        };
        // Hai lượt: lượt đầu egui còn đang đo chữ, kích thước chưa chốt.
        // run_ui dựng sẵn Ui phủ kín khung, đúng như eframe đưa cho viewport con.
        let mut ra = ctx.run_ui(input.clone(), |ui| {
            noi_dung(ui, &bang, egui::ViewportClass::Deferred);
        });
        // Không có backend nào nạp texture chữ, phải tự dọn kẻo epaint kêu.
        ra.textures_delta.clear();
        ra = ctx.run_ui(input, |ui| {
            noi_dung(ui, &bang, egui::ViewportClass::Deferred);
        });
        ra.textures_delta.clear();
        ra.shapes
            .iter()
            .map(|hinh| hinh.shape.visual_bounding_rect())
            .filter(|o| o.is_finite() && o.is_positive())
            .fold(egui::Rect::NOTHING, |gop, o| gop.union(o))
    }

    #[test]
    fn moi_thu_ve_ra_deu_nam_trong_o() {
        let ve = ve_thu(Tin {
            noi: true,
            ten: "MAY-VAN-PHONG-01.nhom-ke-toan".into(),
            dang_chuyen: Some(("bản-ghi-cuộc-họp-quý-tư.zip".into(), 0.42)),
            chua_doc: 12,
            trang_thai: String::new(),
        });
        let khung = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(RONG, CAO));
        assert!(ve.is_positive(), "không vẽ ra gì cả");
        // Nới một điểm cho nét viền vẽ đè lên mép.
        assert!(khung.expand(1.0).contains_rect(ve), "tràn ra ngoài: {ve:?}");
    }

    #[test]
    fn chua_noi_thi_ve_dong_trang_thai_thay_vi_ten() {
        let ve = ve_thu(Tin {
            noi: false,
            trang_thai: "đang chờ máy kia gõ mật khẩu".into(),
            ..Default::default()
        });
        let khung = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(RONG, CAO));
        assert!(khung.expand(1.0).contains_rect(ve), "tràn ra ngoài: {ve:?}");
    }

    #[test]
    fn khong_co_duong_gui_thi_tha_tep_khong_no() {
        let bang = Bang::default();
        bang.gui_tep(vec![PathBuf::from("/tmp/a.txt")]);
    }
}
