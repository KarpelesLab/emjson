//! String decoding: escapes, UTF-8 validation, and the sinks decoded bytes go to.

use crate::error::ErrorKind;
use crate::io::Source;
use crate::parser::{Parser, Res};

/// Destination of decoded string bytes.
pub(crate) trait Sink {
    /// How many more bytes can be accepted.
    fn room(&self) -> usize;
    /// Accepts bytes (callers never pass more than `room()` for strings).
    fn put(&mut self, bytes: &[u8]);
}

/// Discards everything (used to skip).
pub(crate) struct NullSink;

impl Sink for NullSink {
    #[inline]
    fn room(&self) -> usize {
        usize::MAX
    }
    #[inline]
    fn put(&mut self, _: &[u8]) {}
}

/// Collects bytes into a buffer; records overflow instead of failing.
pub(crate) struct BufSink<'b> {
    buf: &'b mut [u8],
    pub(crate) len: usize,
    pub(crate) overflow: bool,
}

impl<'b> BufSink<'b> {
    pub(crate) fn new(buf: &'b mut [u8]) -> Self {
        Self { buf, len: 0, overflow: false }
    }
}

impl Sink for BufSink<'_> {
    #[inline]
    fn room(&self) -> usize {
        self.buf.len() - self.len
    }
    #[inline]
    fn put(&mut self, bytes: &[u8]) {
        let n = bytes.len().min(self.room());
        self.buf[self.len..self.len + n].copy_from_slice(&bytes[..n]);
        self.len += n;
        if n < bytes.len() {
            self.overflow = true;
        }
    }
}

/// Compares decoded bytes with a pattern, optionally JSON-Pointer-escaped (`~0`, `~1`).
pub(crate) struct KeyMatcher<'p> {
    pat: &'p [u8],
    pos: usize,
    escaped: bool,
    ok: bool,
}

impl<'p> KeyMatcher<'p> {
    pub(crate) fn new(pat: &'p [u8], escaped: bool) -> Self {
        Self { pat, pos: 0, escaped, ok: true }
    }

    fn next_pat(&mut self) -> Option<u8> {
        let b = *self.pat.get(self.pos)?;
        self.pos += 1;
        if b != b'~' {
            return Some(b);
        }
        let e = *self.pat.get(self.pos)?;
        self.pos += 1;
        match e {
            b'0' => Some(b'~'),
            b'1' => Some(b'/'),
            _ => None,
        }
    }

    pub(crate) fn matched(&self) -> bool {
        self.ok && self.pos == self.pat.len()
    }
}

impl Sink for KeyMatcher<'_> {
    #[inline]
    fn room(&self) -> usize {
        usize::MAX
    }
    fn put(&mut self, bytes: &[u8]) {
        if !self.ok {
            return;
        }
        if !self.escaped {
            let end = self.pos + bytes.len();
            if self.pat.get(self.pos..end) == Some(bytes) {
                self.pos = end;
            } else {
                self.ok = false;
            }
            return;
        }
        for &b in bytes {
            if self.next_pat() != Some(b) {
                self.ok = false;
                return;
            }
        }
    }
}

impl<S: Source, const N: usize> Parser<S, N> {
    /// Decodes string content (after the opening quote) into `sink`.
    ///
    /// Returns `true` when the closing quote was consumed, `false` when the sink is full.
    /// Stops only between characters: a character that does not fit is stashed and
    /// emitted first on the next call.
    pub(crate) fn decode<K: Sink>(&mut self, sink: &mut K) -> Res<bool, S> {
        if self.stash_len > 0 {
            let n = self.stash_len as usize;
            if sink.room() < n {
                return Ok(false);
            }
            sink.put(&self.stash[..n]);
            self.stash_len = 0;
        }
        loop {
            let room = sink.room();
            let buf = match self.src.fill() {
                Ok(b) => b,
                Err(e) => return Err(crate::Error::Io(e)),
            };
            if buf.is_empty() {
                return self.fail(ErrorKind::UnexpectedEof);
            }
            let lim = buf.len().min(room);
            let mut i = 0;
            while i < lim {
                let b = buf[i];
                if b == b'"' || b == b'\\' || !(0x20..0x80).contains(&b) {
                    break;
                }
                i += 1;
            }
            if i > 0 {
                sink.put(&buf[..i]);
                self.src.consume(i);
                continue;
            }
            let b = buf[0];
            if b == b'"' {
                self.src.consume(1);
                return Ok(true);
            }
            if room == 0 {
                return Ok(false);
            }
            let mut tmp = [0u8; 4];
            let len = match b {
                b'\\' => {
                    self.src.consume(1);
                    self.decode_escape()?.encode_utf8(&mut tmp).len()
                }
                0..=0x1f => return self.fail(ErrorKind::ControlCharacter),
                _ => self.decode_utf8(&mut tmp)?,
            };
            if len <= sink.room() {
                sink.put(&tmp[..len]);
            } else {
                self.stash = tmp;
                self.stash_len = len as u8;
                return Ok(false);
            }
        }
    }

    /// Decodes an escape sequence (after the backslash).
    fn decode_escape(&mut self) -> Res<char, S> {
        let Some(e) = self.peek_byte()? else { return self.fail(ErrorKind::UnexpectedEof) };
        let c = match e {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => {
                self.src.consume(1);
                let hi = self.hex4()?;
                let cp = match hi {
                    0xd800..=0xdbff => {
                        if self.peek_byte()? != Some(b'\\') {
                            return self.fail(ErrorKind::InvalidUnicode);
                        }
                        self.src.consume(1);
                        if self.peek_byte()? != Some(b'u') {
                            return self.fail(ErrorKind::InvalidUnicode);
                        }
                        self.src.consume(1);
                        let lo = self.hex4()?;
                        if !(0xdc00..=0xdfff).contains(&lo) {
                            return self.fail(ErrorKind::InvalidUnicode);
                        }
                        0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00)
                    }
                    0xdc00..=0xdfff => return self.fail(ErrorKind::InvalidUnicode),
                    _ => hi,
                };
                return match char::from_u32(cp) {
                    Some(c) => Ok(c),
                    None => self.fail(ErrorKind::InvalidUnicode),
                };
            }
            _ => return self.fail(ErrorKind::InvalidEscape),
        };
        self.src.consume(1);
        Ok(c)
    }

    fn hex4(&mut self) -> Res<u32, S> {
        let mut v = 0;
        for _ in 0..4 {
            let d = match self.peek_byte()? {
                Some(b @ b'0'..=b'9') => b - b'0',
                Some(b @ b'a'..=b'f') => b - b'a' + 10,
                Some(b @ b'A'..=b'F') => b - b'A' + 10,
                Some(_) => return self.fail(ErrorKind::InvalidEscape),
                None => return self.fail(ErrorKind::UnexpectedEof),
            };
            self.src.consume(1);
            v = v * 16 + d as u32;
        }
        Ok(v)
    }

    /// Validates and copies one multi-byte UTF-8 character.
    fn decode_utf8(&mut self, out: &mut [u8; 4]) -> Res<usize, S> {
        let b0 = self.peek_byte()?.unwrap_or(0);
        let (len, lo, hi) = match b0 {
            0xc2..=0xdf => (2, 0x80, 0xbf),
            0xe0 => (3, 0xa0, 0xbf),
            0xe1..=0xec | 0xee..=0xef => (3, 0x80, 0xbf),
            0xed => (3, 0x80, 0x9f),
            0xf0 => (4, 0x90, 0xbf),
            0xf1..=0xf3 => (4, 0x80, 0xbf),
            0xf4 => (4, 0x80, 0x8f),
            _ => return self.fail(ErrorKind::InvalidUnicode),
        };
        self.src.consume(1);
        out[0] = b0;
        for (k, slot) in out.iter_mut().enumerate().take(len).skip(1) {
            let (l, h) = if k == 1 { (lo, hi) } else { (0x80, 0xbf) };
            match self.peek_byte()? {
                Some(b) if (l..=h).contains(&b) => {
                    self.src.consume(1);
                    *slot = b;
                }
                Some(_) => return self.fail(ErrorKind::InvalidUnicode),
                None => return self.fail(ErrorKind::UnexpectedEof),
            }
        }
        Ok(len)
    }
}
