//! Bounded capture of a verification command's output.

/// Keeps the first and last bytes of a stream within a fixed budget, so a command that
/// prints gigabytes costs constant memory and the agent still sees how the output began
/// (build errors) and ended (test summary).
#[derive(Debug)]
pub(crate) struct HeadTail {
    head_cap: usize,
    tail_cap: usize,
    head: Vec<u8>,
    /// A ring buffer once full.
    tail: Vec<u8>,
    /// The next write position in `tail` when full.
    tail_next: usize,
    total: u64,
}

impl HeadTail {
    pub(crate) fn new(limit: usize) -> Self {
        let head_cap = limit / 2;
        Self {
            head_cap,
            tail_cap: limit - head_cap,
            head: Vec::new(),
            tail: Vec::new(),
            tail_next: 0,
            total: 0,
        }
    }

    pub(crate) fn write(&mut self, mut p: &[u8]) {
        self.total += p.len() as u64;
        let room = self.head_cap.saturating_sub(self.head.len());
        if room > 0 {
            let take = room.min(p.len());
            self.head.extend_from_slice(&p[..take]);
            p = &p[take..];
        }
        while !p.is_empty() && self.tail_cap > 0 {
            if self.tail.len() < self.tail_cap {
                let take = (self.tail_cap - self.tail.len()).min(p.len());
                self.tail.extend_from_slice(&p[..take]);
                p = &p[take..];
                continue;
            }
            if p.len() >= self.tail_cap {
                self.tail.copy_from_slice(&p[p.len() - self.tail_cap..]);
                self.tail_next = 0;
                break;
            }
            let n = (self.tail_cap - self.tail_next).min(p.len());
            self.tail[self.tail_next..self.tail_next + n].copy_from_slice(&p[..n]);
            self.tail_next = (self.tail_next + n) % self.tail_cap;
            p = &p[n..];
        }
    }

    /// The kept output and whether anything was dropped. Cuts may split a multi-byte
    /// character; invalid UTF-8 at the seams is replaced.
    pub(crate) fn result(&self) -> (String, bool) {
        let mut tail = self.tail[self.tail_next..].to_vec();
        tail.extend_from_slice(&self.tail[..self.tail_next]);
        let kept = (self.head.len() + tail.len()) as u64;
        let head = String::from_utf8_lossy(&self.head);
        let tail = String::from_utf8_lossy(&tail);
        if kept == self.total {
            return (format!("{head}{tail}"), false);
        }
        (
            format!(
                "{head}\n\n[… {} bytes omitted …]\n\n{tail}",
                self.total - kept
            ),
            true,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::HeadTail;

    #[test]
    fn head_tail() {
        let cases: [(usize, &[&str], &str, bool); 5] = [
            (10, &["hello"], "hello", false),
            (10, &["0123456789"], "0123456789", false),
            (
                10,
                &["01234", "56789", "abcde"],
                "01234\n\n[… 5 bytes omitted …]\n\nabcde",
                true,
            ),
            (
                10,
                &["0123456789abcdefghij"],
                "01234\n\n[… 10 bytes omitted …]\n\nfghij",
                true,
            ),
            (
                10,
                &["01234", "5", "6", "7", "8", "9", "a", "b"],
                "01234\n\n[… 2 bytes omitted …]\n\n789ab",
                true,
            ),
        ];
        for (i, (limit, writes, want, truncated)) in cases.into_iter().enumerate() {
            let mut h = HeadTail::new(limit);
            for w in writes {
                h.write(w.as_bytes());
            }
            assert_eq!(h.result(), (want.to_owned(), truncated), "case {i}");
        }
        let mut h = HeadTail::new(4);
        h.write("éééé".as_bytes()); // 8 bytes; cuts land inside characters
        let (got, _) = h.result();
        assert!(
            got.contains('é') && !got.contains('\0'),
            "utf-8 seams: {got:?}"
        );
    }
}
