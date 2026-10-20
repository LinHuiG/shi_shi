use serde::Serialize;

#[derive(Default, Serialize)]
pub struct LiveProgress {
    pub request_id: String,
    pub phase: String,
    pub last_received: i64,
    pub revision: u64,
    pub entries: Vec<Entry>,
}
#[derive(Serialize)]
pub struct Entry {
    id: u64,
    kind: String,
    text: String,
}
impl LiveProgress {
    pub fn start(&mut self, id: &str) {
        *self = Self {
            request_id: id.into(),
            phase: "正在连接".into(),
            ..Self::default()
        };
    }
    pub fn accept(&mut self, kind: &str, text: &str, append: bool) {
        if text.is_empty() {
            return;
        }
        self.revision = self.revision.wrapping_add(1);
        if kind == "phase" {
            self.phase = text.into();
        } else {
            self.last_received = crate::model::now();
            self.phase = match kind {
                "reasoning" => "正在思考",
                "tool" => "读取上下文",
                _ => "接收结果",
            }
            .into();
        }
        if let Some(last) = self.entries.last_mut().filter(|e| e.kind == kind) {
            if append {
                last.text.push_str(text);
            } else {
                last.text = text.into();
            }
            last.text = tail(&last.text, 12000);
        } else {
            self.entries.push(Entry {
                id: self.revision,
                kind: kind.into(),
                text: tail(text, 12000),
            });
        }
        while self.entries.iter().map(|e| e.text.len()).sum::<usize>() > 48_000
            || self.entries.len() > 64
        {
            self.entries.remove(0);
        }
    }
}
fn tail(text: &str, count: usize) -> String {
    let skip = text.chars().count().saturating_sub(count);
    text.chars().skip(skip).collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chunks_append_snapshots_replace_and_request_reset_isolated() {
        let mut p = LiveProgress::default();
        p.start("first");
        p.accept("reasoning", "核", true);
        p.accept("reasoning", "对", true);
        assert_eq!(p.entries[0].text, "核对");
        p.accept("reasoning", "核对完成", false);
        assert_eq!(p.entries[0].text, "核对完成");
        p.accept("content", &"字".repeat(100_000), true);
        assert!(p.entries.iter().map(|e| e.text.len()).sum::<usize>() <= 48_000);
        p.start("second");
        assert!(p.entries.is_empty());
        assert_eq!(p.last_received, 0);
        p.accept("phase", "已开始处理，等待公开内容", false);
        assert_eq!(p.entries[0].kind, "phase");
        assert_eq!(
            p.last_received, 0,
            "a phase must not claim provider text arrived"
        );
    }
}
