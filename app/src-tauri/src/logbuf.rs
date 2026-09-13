//! 运行日志环形缓冲。
//!
//! 除输出到 stdout 外，再留一份给「设置」页右侧的日志面板用：
//! tracing 的 fmt 层挂一个 TeeWriter，格式化后的每条日志同时进缓冲。
//! 前端按 seq 增量拉取，避免重复传大段文本。

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, OnceLock};

/// 最多保留多少行
pub const MAX_LINES: usize = 2000;

/// 单行日志上限（防止某个超长输出把缓冲撑爆）
const MAX_LINE_CHARS: usize = 2000;

#[derive(Debug, Clone)]
pub struct LogLine {
    pub seq: u64,
    pub text: String,
}

#[derive(Default)]
pub struct LogBuffer {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    seq: u64,
    lines: VecDeque<LogLine>,
}

impl LogBuffer {
    /// 追加一段（可能是多行的）文本
    pub fn push(&self, text: &str) {
        let mut guard = self.inner.lock().unwrap();
        for raw in text.split('\n') {
            let line = raw.trim_end_matches('\r');
            if line.trim().is_empty() {
                continue;
            }
            let text = if line.chars().count() > MAX_LINE_CHARS {
                let mut s: String = line.chars().take(MAX_LINE_CHARS).collect();
                s.push_str(" …（已截断）");
                s
            } else {
                line.to_string()
            };
            guard.seq += 1;
            let seq = guard.seq;
            guard.lines.push_back(LogLine { seq, text });
            while guard.lines.len() > MAX_LINES {
                guard.lines.pop_front();
            }
        }
    }

    /// 取 seq 之后的日志；返回 (日志, 当前最大 seq)
    pub fn since(&self, since: u64) -> (Vec<LogLine>, u64) {
        let guard = self.inner.lock().unwrap();
        let lines = guard
            .lines
            .iter()
            .filter(|l| l.seq > since)
            .cloned()
            .collect::<Vec<_>>();
        (lines, guard.seq)
    }

    pub fn clear(&self) {
        let mut guard = self.inner.lock().unwrap();
        guard.lines.clear();
    }

    #[allow(dead_code)] // 目前只有测试用，留着方便排查
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().lines.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

static BUFFER: OnceLock<Arc<LogBuffer>> = OnceLock::new();

/// 全局日志缓冲（首次调用时创建）
pub fn buffer() -> Arc<LogBuffer> {
    BUFFER.get_or_init(|| Arc::new(LogBuffer::default())).clone()
}

/// 同时写 stdout、日志缓冲与日志文件的 writer
pub struct TeeWriter {
    buf: Arc<LogBuffer>,
}

/// 日志文件上限（超过就截断重写，避免无限增长）
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// 日志文件路径：%APPDATA%\com.audiomix.app\audiomix.log
fn log_file_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("APPDATA")?;
    Some(
        std::path::PathBuf::from(base)
            .join("com.audiomix.app")
            .join("audiomix.log"),
    )
}

fn append_to_file(text: &str) {
    let Some(path) = log_file_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::metadata(&path).map(|m| m.len() > MAX_FILE_BYTES).unwrap_or(false) {
        let _ = std::fs::remove_file(&path);
    }
    use std::io::Write as _;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{text}");
    }
}

impl Write for TeeWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let text = String::from_utf8_lossy(data);
        self.buf.push(&text);
        append_to_file(text.trim_end_matches(['\r', '\n']));
        let mut out = io::stdout();
        let _ = out.write_all(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stdout().flush()
    }
}

/// tracing_subscriber 的 MakeWriter
#[derive(Clone, Copy, Default)]
pub struct TeeMakeWriter;

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for TeeMakeWriter {
    type Writer = TeeWriter;

    fn make_writer(&'a self) -> Self::Writer {
        TeeWriter { buf: buffer() }
    }
}

/// 给 tracing 用的 writer
pub fn make_writer() -> TeeMakeWriter {
    TeeMakeWriter
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_incremental_read() {
        let buf = LogBuffer::default();
        buf.push("第一行\n第二行\n");
        let (all, next) = buf.since(0);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].text, "第一行");
        assert_eq!(all[1].text, "第二行");
        assert_eq!(next, 2);

        // 增量：只取新行
        buf.push("第三行");
        let (more, next2) = buf.since(next);
        assert_eq!(more.len(), 1);
        assert_eq!(more[0].text, "第三行");
        assert_eq!(next2, 3);
        assert!(buf.since(next2).0.is_empty());
    }

    #[test]
    fn blank_lines_are_skipped_and_long_lines_truncated() {
        let buf = LogBuffer::default();
        buf.push("\n   \n");
        assert!(buf.is_empty(), "空行不入缓冲");
        buf.push(&"x".repeat(MAX_LINE_CHARS + 50));
        let (lines, _) = buf.since(0);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].text.ends_with("（已截断）"));
    }

    #[test]
    fn ring_buffer_keeps_last_lines() {
        let buf = LogBuffer::default();
        for i in 0..(MAX_LINES + 10) {
            buf.push(&format!("line {i}"));
        }
        assert_eq!(buf.len(), MAX_LINES);
        let (lines, _) = buf.since(0);
        assert_eq!(lines.first().unwrap().text, format!("line {}", 10));
    }

    #[test]
    fn clear_empties_buffer() {
        let buf = LogBuffer::default();
        buf.push("a\nb");
        buf.clear();
        assert!(buf.is_empty());
        assert!(buf.since(0).0.is_empty());
    }
}
