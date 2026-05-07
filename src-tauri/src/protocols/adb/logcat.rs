//! Binary logcat parser with reassembly buffer, priority/tag/PID filtering,
//! and ANSI colour formatting.
//!
//! Parses the raw binary stream produced by `adb logcat -B` (both v1 20-byte
//! headers and v3 24-byte headers) and returns structured [`LogcatEntry`] values.

/// Maximum internal buffer size (1 MiB). If the buffer exceeds this the parser
/// assumes a corrupt stream, warns, and clears the buffer to recover.
const MAX_BUF_SIZE: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// LogPriority
// ---------------------------------------------------------------------------

/// Android log priority levels (matches `android_LogPriority` in
/// `<android/log.h>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogPriority {
    Verbose = 2,
    Debug   = 3,
    Info    = 4,
    Warn    = 5,
    Error   = 6,
    Fatal   = 7,
}

impl LogPriority {
    /// Convert a raw `u8` priority byte to the enum, returning `None` for
    /// values outside the valid range.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            2 => Some(Self::Verbose),
            3 => Some(Self::Debug),
            4 => Some(Self::Info),
            5 => Some(Self::Warn),
            6 => Some(Self::Error),
            7 => Some(Self::Fatal),
            _ => None,
        }
    }

    /// Single-character abbreviation used by Android tooling.
    pub fn char(self) -> char {
        match self {
            Self::Verbose => 'V',
            Self::Debug   => 'D',
            Self::Info    => 'I',
            Self::Warn    => 'W',
            Self::Error   => 'E',
            Self::Fatal   => 'F',
        }
    }

    /// Parse from a one-character or full-name string (case-insensitive).
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "V" | "VERBOSE" => Some(Self::Verbose),
            "D" | "DEBUG"   => Some(Self::Debug),
            "I" | "INFO"    => Some(Self::Info),
            "W" | "WARN"    => Some(Self::Warn),
            "E" | "ERROR"   => Some(Self::Error),
            "F" | "FATAL"   => Some(Self::Fatal),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// LogcatEntry
// ---------------------------------------------------------------------------

/// One parsed logcat record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogcatEntry {
    pub pid: u32,
    pub tid: u32,
    pub sec: u32,
    pub nsec: u32,
    pub priority: LogPriority,
    pub tag: String,
    pub message: String,
}

impl LogcatEntry {
    /// Render the entry with ANSI escape codes matching Android Studio colours.
    pub fn to_ansi(&self) -> String {
        let (open, close) = match self.priority {
            LogPriority::Verbose => ("\x1b[2m",    "\x1b[0m"),
            LogPriority::Debug   => ("",           ""),
            LogPriority::Info    => ("\x1b[36m",   "\x1b[0m"),
            LogPriority::Warn    => ("\x1b[33m",   "\x1b[0m"),
            LogPriority::Error   => ("\x1b[31m",   "\x1b[0m"),
            LogPriority::Fatal   => ("\x1b[1;31m", "\x1b[0m"),
        };
        format!(
            "{}{}/{tag}({pid}): {msg}{close}",
            open,
            self.priority.char(),
            tag = self.tag,
            pid = self.pid,
            msg = self.message,
            close = close,
        )
    }
}

// ---------------------------------------------------------------------------
// LogcatFilter
// ---------------------------------------------------------------------------

/// Simple priority / tag-prefix / PID filter.
#[derive(Debug, Clone)]
pub struct LogcatFilter {
    pub min_priority: LogPriority,
    pub tag: Option<String>,
    pub pid: Option<u32>,
}

impl LogcatFilter {
    /// Returns `true` when the entry passes **all** active filter criteria.
    pub fn matches(&self, entry: &LogcatEntry) -> bool {
        if entry.priority < self.min_priority {
            return false;
        }
        if let Some(ref prefix) = self.tag {
            if !entry.tag.starts_with(prefix.as_str()) {
                return false;
            }
        }
        if let Some(pid) = self.pid {
            if entry.pid != pid {
                return false;
            }
        }
        true
    }
}

// ---------------------------------------------------------------------------
// LogcatParser
// ---------------------------------------------------------------------------

/// Streaming binary logcat parser with internal reassembly buffer.
pub struct LogcatParser {
    buf: Vec<u8>,
}

impl LogcatParser {
    /// Create a new parser with a pre-allocated 4 KiB buffer.
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(4096),
        }
    }

    /// Append raw bytes from the transport layer and return any complete
    /// [`LogcatEntry`] values that can now be parsed.
    pub fn feed(&mut self, data: &[u8]) -> Vec<LogcatEntry> {
        self.buf.extend_from_slice(data);

        // Overflow recovery — protects against run-away / corrupt streams.
        if self.buf.len() > MAX_BUF_SIZE {
            tracing::warn!(
                "logcat parser buffer exceeded {} bytes – clearing to recover",
                MAX_BUF_SIZE,
            );
            self.buf.clear();
            return Vec::new();
        }

        let mut entries = Vec::new();

        loop {
            // Need at least 4 bytes to read payload_len + header_size.
            if self.buf.len() < 4 {
                break;
            }

            let payload_len = u16::from_le_bytes([self.buf[0], self.buf[1]]) as usize;
            let header_size = u16::from_le_bytes([self.buf[2], self.buf[3]]) as usize;

            // Sanity-check header_size (v1 = 20, v3 = 24, allow some slack).
            if !(20..=64).contains(&header_size) {
                tracing::warn!(
                    "logcat parser: invalid header_size {header_size} – clearing buffer"
                );
                self.buf.clear();
                break;
            }

            let total = header_size + payload_len;
            if self.buf.len() < total {
                break; // partial entry — wait for more data
            }

            // --- Parse header fields (offsets are fixed for both v1 & v3) ---
            let pid  = i32::from_le_bytes([self.buf[4], self.buf[5], self.buf[6], self.buf[7]]) as u32;
            let tid  = i32::from_le_bytes([self.buf[8], self.buf[9], self.buf[10], self.buf[11]]) as u32;
            let sec  = i32::from_le_bytes([self.buf[12], self.buf[13], self.buf[14], self.buf[15]]) as u32;
            let nsec = i32::from_le_bytes([self.buf[16], self.buf[17], self.buf[18], self.buf[19]]) as u32;
            // v3 has an additional lid field at [20..24] which we skip.

            // --- Parse payload ---
            let payload = &self.buf[header_size..total];

            let priority = if payload.is_empty() {
                LogPriority::Verbose
            } else {
                LogPriority::from_u8(payload[0]).unwrap_or(LogPriority::Verbose)
            };

            // tag: starts at payload[1], null-terminated
            let tag_start = 1;
            let tag_end = payload[tag_start..]
                .iter()
                .position(|&b| b == 0)
                .map(|p| tag_start + p)
                .unwrap_or(payload.len());
            let tag = String::from_utf8_lossy(&payload[tag_start..tag_end]).into_owned();

            // message: starts after the tag's null terminator
            let msg_start = if tag_end < payload.len() { tag_end + 1 } else { payload.len() };
            let msg_end = payload[msg_start..]
                .iter()
                .position(|&b| b == 0)
                .map(|p| msg_start + p)
                .unwrap_or(payload.len());
            let message = String::from_utf8_lossy(&payload[msg_start..msg_end]).into_owned();

            entries.push(LogcatEntry {
                pid,
                tid,
                sec,
                nsec,
                priority,
                tag,
                message,
            });

            // Drain the consumed bytes.
            self.buf.drain(..total);
        }

        entries
    }
}

// ===========================================================================
// Tests
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;

    // -- helpers ------------------------------------------------------------

    /// Build a v1 binary logcat entry (header_size = 20).
    fn make_entry_v1(
        pid: u32,
        tid: u32,
        sec: u32,
        nsec: u32,
        priority: u8,
        tag: &str,
        msg: &str,
    ) -> Vec<u8> {
        // payload = priority(1) + tag + '\0' + msg + '\0'
        let mut payload = Vec::new();
        payload.push(priority);
        payload.extend_from_slice(tag.as_bytes());
        payload.push(0);
        payload.extend_from_slice(msg.as_bytes());
        payload.push(0);

        let payload_len = payload.len() as u16;
        let header_size: u16 = 20;

        let mut out = Vec::new();
        out.extend_from_slice(&payload_len.to_le_bytes());
        out.extend_from_slice(&header_size.to_le_bytes());
        out.extend_from_slice(&(pid as i32).to_le_bytes());
        out.extend_from_slice(&(tid as i32).to_le_bytes());
        out.extend_from_slice(&(sec as i32).to_le_bytes());
        out.extend_from_slice(&(nsec as i32).to_le_bytes());
        out.extend_from_slice(&payload);
        out
    }

    /// Build a v3 binary logcat entry (header_size = 24, has lid field).
    fn make_entry_v3(
        pid: u32,
        tid: u32,
        sec: u32,
        nsec: u32,
        lid: u32,
        priority: u8,
        tag: &str,
        msg: &str,
    ) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.push(priority);
        payload.extend_from_slice(tag.as_bytes());
        payload.push(0);
        payload.extend_from_slice(msg.as_bytes());
        payload.push(0);

        let payload_len = payload.len() as u16;
        let header_size: u16 = 24;

        let mut out = Vec::new();
        out.extend_from_slice(&payload_len.to_le_bytes());
        out.extend_from_slice(&header_size.to_le_bytes());
        out.extend_from_slice(&(pid as i32).to_le_bytes());
        out.extend_from_slice(&(tid as i32).to_le_bytes());
        out.extend_from_slice(&(sec as i32).to_le_bytes());
        out.extend_from_slice(&(nsec as i32).to_le_bytes());
        out.extend_from_slice(&lid.to_le_bytes());
        out.extend_from_slice(&payload);
        out
    }

    // -- tests --------------------------------------------------------------

    #[test]
    fn test_parse_v1_entry() {
        let data = make_entry_v1(1234, 5678, 1000, 999_999, 4, "MyTag", "hello world");
        let mut parser = LogcatParser::new();
        let entries = parser.feed(&data);

        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.pid, 1234);
        assert_eq!(e.tid, 5678);
        assert_eq!(e.sec, 1000);
        assert_eq!(e.nsec, 999_999);
        assert_eq!(e.priority, LogPriority::Info);
        assert_eq!(e.tag, "MyTag");
        assert_eq!(e.message, "hello world");
    }

    #[test]
    fn test_parse_v3_entry() {
        let data = make_entry_v3(42, 43, 500, 123_456, 3, 5, "SysUI", "warning msg");
        let mut parser = LogcatParser::new();
        let entries = parser.feed(&data);

        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.pid, 42);
        assert_eq!(e.tid, 43);
        assert_eq!(e.sec, 500);
        assert_eq!(e.nsec, 123_456);
        assert_eq!(e.priority, LogPriority::Warn);
        assert_eq!(e.tag, "SysUI");
        assert_eq!(e.message, "warning msg");
    }

    #[test]
    fn test_parse_split_packet() {
        let data = make_entry_v1(10, 20, 1, 2, 3, "Tag", "split");
        let mid = data.len() / 2;

        let mut parser = LogcatParser::new();
        let first = parser.feed(&data[..mid]);
        assert!(first.is_empty(), "partial data should yield no entries");

        let second = parser.feed(&data[mid..]);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].tag, "Tag");
        assert_eq!(second[0].message, "split");
    }

    #[test]
    fn test_parse_multiple_entries() {
        let mut data = make_entry_v1(1, 1, 0, 0, 4, "A", "first");
        data.extend(make_entry_v1(2, 2, 0, 0, 6, "B", "second"));

        let mut parser = LogcatParser::new();
        let entries = parser.feed(&data);

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].tag, "A");
        assert_eq!(entries[0].message, "first");
        assert_eq!(entries[1].tag, "B");
        assert_eq!(entries[1].message, "second");
    }

    #[test]
    fn test_priority_from_u8() {
        assert_eq!(LogPriority::from_u8(2), Some(LogPriority::Verbose));
        assert_eq!(LogPriority::from_u8(3), Some(LogPriority::Debug));
        assert_eq!(LogPriority::from_u8(4), Some(LogPriority::Info));
        assert_eq!(LogPriority::from_u8(5), Some(LogPriority::Warn));
        assert_eq!(LogPriority::from_u8(6), Some(LogPriority::Error));
        assert_eq!(LogPriority::from_u8(7), Some(LogPriority::Fatal));
        assert_eq!(LogPriority::from_u8(0), None);
        assert_eq!(LogPriority::from_u8(1), None);
        assert_eq!(LogPriority::from_u8(8), None);
        assert_eq!(LogPriority::from_u8(255), None);
    }

    #[test]
    fn test_filter_by_priority() {
        let entry = LogcatEntry {
            pid: 1, tid: 1, sec: 0, nsec: 0,
            priority: LogPriority::Info,
            tag: "Test".into(),
            message: "msg".into(),
        };

        let pass = LogcatFilter { min_priority: LogPriority::Debug, tag: None, pid: None };
        assert!(pass.matches(&entry), "Info >= Debug should pass");

        let fail = LogcatFilter { min_priority: LogPriority::Warn, tag: None, pid: None };
        assert!(!fail.matches(&entry), "Info < Warn should fail");
    }

    #[test]
    fn test_filter_by_tag() {
        let entry = LogcatEntry {
            pid: 1, tid: 1, sec: 0, nsec: 0,
            priority: LogPriority::Info,
            tag: "ActivityManager".into(),
            message: "msg".into(),
        };

        let pass = LogcatFilter {
            min_priority: LogPriority::Verbose,
            tag: Some("Activity".into()),
            pid: None,
        };
        assert!(pass.matches(&entry), "'Activity' prefix should match 'ActivityManager'");

        let fail = LogcatFilter {
            min_priority: LogPriority::Verbose,
            tag: Some("System".into()),
            pid: None,
        };
        assert!(!fail.matches(&entry), "'System' prefix should not match 'ActivityManager'");
    }

    #[test]
    fn test_filter_by_pid() {
        let entry = LogcatEntry {
            pid: 999, tid: 1, sec: 0, nsec: 0,
            priority: LogPriority::Info,
            tag: "X".into(),
            message: "m".into(),
        };

        let pass = LogcatFilter { min_priority: LogPriority::Verbose, tag: None, pid: Some(999) };
        assert!(pass.matches(&entry));

        let fail = LogcatFilter { min_priority: LogPriority::Verbose, tag: None, pid: Some(100) };
        assert!(!fail.matches(&entry));
    }

    #[test]
    fn test_ansi_formatting() {
        let entry = LogcatEntry {
            pid: 42, tid: 1, sec: 0, nsec: 0,
            priority: LogPriority::Error,
            tag: "CrashTag".into(),
            message: "something broke".into(),
        };
        let ansi = entry.to_ansi();

        assert!(ansi.contains("\x1b[31m"),  "Error should contain red escape");
        assert!(ansi.contains("E/CrashTag"), "should contain E/tag");
        assert!(ansi.contains("(42)"),       "should contain (pid)");
        assert!(ansi.contains("something broke"), "should contain message");
        assert!(ansi.contains("\x1b[0m"),    "should end with reset");
    }

    #[test]
    fn test_buffer_overflow_recovery() {
        let mut parser = LogcatParser::new();
        let junk = vec![0xFFu8; MAX_BUF_SIZE + 100];
        let entries = parser.feed(&junk);

        // Should not panic, buffer should be cleared, no entries produced.
        assert!(entries.is_empty(), "overflow junk should produce no entries");

        // Parser should still be usable afterwards.
        let data = make_entry_v1(1, 1, 0, 0, 4, "OK", "recovered");
        let entries = parser.feed(&data);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].message, "recovered");
    }
}
