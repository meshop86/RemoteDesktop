//! Khung chat: lưu lịch sử, đếm tin chưa đọc, và làm sạch tin của đầu kia.
//!
//! Tin nhắn đến là dữ liệu người khác soạn, không phải dữ liệu của ta. Nó được
//! vẽ thẳng lên giao diện nên phải cắt độ dài và bỏ ký tự điều khiển trước:
//! một tin dài vài megabyte đủ làm khựng vòng vẽ, còn ký tự điều khiển thì làm
//! loạn cả log lẫn terminal khi ai đó copy ra dán lại.

use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

use rd_protocol::ChatMessage;

/// Trần độ dài một tin, tính theo byte UTF-8. 4000 byte đủ cho một đoạn văn
/// dài; dài hơn nữa thì gần như chắc chắn là dán nhầm hoặc cố tình phá.
pub const MAX_CHAT_BODY: usize = 4000;

/// Số tin giữ lại trong bộ nhớ. Cũ hơn thì rơi ra — phiên điều khiển từ xa
/// không phải chỗ lưu trữ lịch sử, và giữ vô hạn là rò rỉ bộ nhớ có hẹn giờ.
pub const HISTORY_LIMIT: usize = 500;

#[derive(Debug, Default)]
pub struct ChatLog {
    entries: VecDeque<ChatMessage>,
    unread: usize,
}

impl ChatLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn entries(&self) -> impl ExactSizeIterator<Item = &ChatMessage> {
        self.entries.iter()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn unread(&self) -> usize {
        self.unread
    }

    pub fn mark_read(&mut self) {
        self.unread = 0;
    }

    /// Soạn một tin của chính ta để gửi đi.
    ///
    /// Trả `None` khi nội dung rỗng sau khi cắt khoảng trắng — bấm Enter nhầm
    /// không nên đẩy một dòng trống sang máy bên kia. Tin của ta không tính là
    /// chưa đọc.
    pub fn compose(&mut self, from: &str, body: &str) -> Option<ChatMessage> {
        let body = clean(body)?;
        let message = ChatMessage {
            from: from.to_string(),
            body,
            sent_at_ms: now_ms(),
        };
        self.push(message.clone());
        Some(message)
    }

    /// Nhận tin từ đầu kia. Trả `false` nếu tin rỗng và bị bỏ.
    pub fn receive(&mut self, message: ChatMessage) -> bool {
        let Some(body) = clean(&message.body) else {
            tracing::debug!(from = %message.from, "bỏ tin chat rỗng");
            return false;
        };
        // Tên người gửi cũng do đầu kia đặt nên cũng phải làm sạch; giữ ngắn vì
        // nó nằm trên một dòng với nội dung.
        let from = clean_name(&message.from);
        self.push(ChatMessage {
            from,
            body,
            sent_at_ms: message.sent_at_ms,
        });
        self.unread += 1;
        true
    }

    fn push(&mut self, message: ChatMessage) {
        if self.entries.len() == HISTORY_LIMIT {
            self.entries.pop_front();
        }
        self.entries.push_back(message);
    }
}

/// Cắt khoảng trắng thừa, chặn độ dài, và bỏ ký tự điều khiển trừ xuống dòng.
fn clean(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut out: String = trimmed
        .chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect();
    truncate_bytes(&mut out, MAX_CHAT_BODY);
    if out.trim().is_empty() { None } else { Some(out) }
}

fn clean_name(from: &str) -> String {
    let mut name: String = from
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    truncate_bytes(&mut name, 64);
    if name.is_empty() {
        "?".to_string()
    } else {
        name
    }
}

/// Cắt chuỗi về tối đa `limit` byte mà không cắt giữa một ký tự UTF-8.
///
/// `String::truncate` panic nếu rơi vào giữa ký tự nhiều byte — mà tiếng Việt
/// có dấu thì ký tự nào cũng nhiều byte, nên đây không phải trường hợp hiếm.
fn truncate_bytes(text: &mut String, limit: usize) {
    if text.len() <= limit {
        return;
    }
    let mut cut = limit;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(body: &str) -> ChatMessage {
        ChatMessage {
            from: "peer".into(),
            body: body.into(),
            sent_at_ms: 1,
        }
    }

    #[test]
    fn compose_trims_and_skips_empty() {
        let mut log = ChatLog::new();
        assert!(log.compose("hoa", "   ").is_none());
        assert!(log.compose("hoa", "\n\t ").is_none());
        let msg = log.compose("hoa", "  chào cậu  ").unwrap();
        assert_eq!(msg.body, "chào cậu");
        assert_eq!(log.len(), 1);
        // Tin mình gửi không phải tin chưa đọc.
        assert_eq!(log.unread(), 0);
    }

    #[test]
    fn receive_counts_unread_until_read() {
        let mut log = ChatLog::new();
        assert!(log.receive(remote("một")));
        assert!(log.receive(remote("hai")));
        assert_eq!(log.unread(), 2);
        log.mark_read();
        assert_eq!(log.unread(), 0);
        assert_eq!(log.len(), 2);
    }

    #[test]
    fn hostile_message_is_defanged() {
        let mut log = ChatLog::new();
        // Ký tự điều khiển bị bỏ, xuống dòng thì giữ.
        assert!(log.receive(remote("dòng 1\ndòng 2\u{1b}[31m đỏ\u{0}")));
        let entry = log.entries().last().unwrap();
        assert_eq!(entry.body, "dòng 1\ndòng 2[31m đỏ");

        // Tin dài bị cắt, và cắt đúng biên ký tự tiếng Việt chứ không panic.
        let mut log = ChatLog::new();
        assert!(log.receive(remote(&"ữ".repeat(MAX_CHAT_BODY))));
        let entry = log.entries().last().unwrap();
        assert!(entry.body.len() <= MAX_CHAT_BODY);
        assert!(entry.body.chars().all(|c| c == 'ữ'));

        // Tên rỗng vẫn phải hiện được ra thứ gì đó.
        let mut log = ChatLog::new();
        log.receive(ChatMessage {
            from: "   ".into(),
            body: "hi".into(),
            sent_at_ms: 0,
        });
        assert_eq!(log.entries().last().unwrap().from, "?");
    }

    #[test]
    fn history_stays_bounded() {
        let mut log = ChatLog::new();
        for i in 0..HISTORY_LIMIT + 50 {
            log.receive(remote(&i.to_string()));
        }
        assert_eq!(log.len(), HISTORY_LIMIT);
        // Giữ lại phần mới nhất chứ không phải phần cũ nhất.
        assert_eq!(
            log.entries().last().unwrap().body,
            (HISTORY_LIMIT + 49).to_string()
        );
    }
}
