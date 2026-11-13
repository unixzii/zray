//! Parser for zsh history files (`$HISTFILE`), used by `zray import`.

/// One entry of a zsh history file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub text: String,
    /// Milliseconds since the Unix epoch.
    pub started_at: i64,
    pub duration_ms: Option<i64>,
}

/// zsh stores bytes it treats as special (0x83..=0xa2, plus NUL) as the Meta
/// byte 0x83 followed by the original byte xor 0x20. Non-ASCII UTF-8 text is
/// unreadable until this is undone.
fn unmetafy(bytes: &[u8]) -> Vec<u8> {
    const META: u8 = 0x83;
    let mut out = Vec::with_capacity(bytes.len());
    let mut iter = bytes.iter().copied();
    while let Some(b) = iter.next() {
        if b == META {
            if let Some(next) = iter.next() {
                out.push(next ^ 0x20);
            }
        } else {
            out.push(b);
        }
    }
    out
}

/// Parses the contents of a history file in either plain or
/// `EXTENDED_HISTORY` (`: <start>:<elapsed>;<command>`) format.
///
/// Plain entries carry no timestamp, so they get synthetic ones counting back
/// one second per entry from `fallback_end_ms`; that keeps their relative
/// order and makes re-imports of an unchanged file idempotent as long as the
/// caller passes a stable value (the file's mtime).
pub fn parse(raw: &[u8], fallback_end_ms: i64) -> Vec<Entry> {
    let bytes = unmetafy(raw);
    let content = String::from_utf8_lossy(&bytes);

    // A trailing backslash means the newline belongs to the command.
    let mut logical: Vec<String> = Vec::new();
    let mut pending: Option<String> = None;
    for line in content.split('\n') {
        let mut cur = pending.take().unwrap_or_default();
        if let Some(stripped) = line.strip_suffix('\\') {
            cur.push_str(stripped);
            cur.push('\n');
            pending = Some(cur);
        } else {
            cur.push_str(line);
            logical.push(cur);
        }
    }
    if let Some(rest) = pending {
        logical.push(rest);
    }

    let total = logical.len() as i64;
    logical
        .into_iter()
        .enumerate()
        .filter_map(|(i, line)| {
            let (meta, text) = match parse_extended(&line) {
                Some((start, elapsed, text)) => (Some((start, elapsed)), text),
                None => (None, line.as_str()),
            };
            let text = text.trim_end_matches('\n');
            if text.trim().is_empty() {
                return None;
            }
            let (started_at, duration_ms) = match meta {
                Some((start, elapsed)) => (start * 1000, Some(elapsed * 1000)),
                None => (fallback_end_ms - (total - i as i64) * 1000, None),
            };
            Some(Entry { text: text.to_string(), started_at, duration_ms })
        })
        .collect()
}

fn parse_extended(line: &str) -> Option<(i64, i64, &str)> {
    let rest = line.strip_prefix(": ")?;
    let (meta, text) = rest.split_once(';')?;
    let (start, elapsed) = meta.split_once(':')?;
    Some((start.trim().parse().ok()?, elapsed.trim().parse().ok()?, text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extended_format() {
        let entries = parse(b": 1700000000:3;git status\n: 1700000010:0;ls -la\n", 0);
        assert_eq!(
            entries,
            vec![
                Entry {
                    text: "git status".into(),
                    started_at: 1_700_000_000_000,
                    duration_ms: Some(3000)
                },
                Entry {
                    text: "ls -la".into(),
                    started_at: 1_700_000_010_000,
                    duration_ms: Some(0)
                },
            ]
        );
    }

    #[test]
    fn multiline_command() {
        let entries = parse(b": 1700000000:0;for i in 1 2; do\\\necho $i\\\ndone\n", 0);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "for i in 1 2; do\necho $i\ndone");
    }

    #[test]
    fn plain_format_keeps_order() {
        let entries = parse(b"first\nsecond\n", 10_000_000);
        assert_eq!(entries.len(), 2);
        assert!(entries[0].started_at < entries[1].started_at);
        assert_eq!(entries[0].duration_ms, None);
    }

    #[test]
    fn metafied_utf8() {
        // "中" is e4 b8 ad; none of those bytes are in zsh's meta range, but
        // "你" (e4 bd a0) has 0xa0, which zsh writes as 83 80.
        let raw = b": 1700000000:0;echo \xe4\xbd\x83\x80\n";
        let entries = parse(raw, 0);
        assert_eq!(entries[0].text, "echo 你");
    }
}
