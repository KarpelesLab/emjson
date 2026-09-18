//! Modifying documents: in place on random-access [`Storage`], or while copying a stream.
//!
//! Every modification is a [`Patch`]: "replace bytes `start..end` with new content". The
//! content is encoded straight to its destination; its length is measured beforehand
//! with [`encoded_len`], so the tail of the document can be moved exactly once.
//!
//! - [`Editor`]: edits a document in [`Storage`] (a RAM buffer via [`MemStorage`], a file,
//!   flash, ...) in place: locate, measure, move the tail, write. Memory used: the parser
//!   (~16 bytes) plus a caller-provided scratch buffer of any size.
//! - [`copy_edit`]: single pass over a stream, writing the modified document to a
//!   [`Write`]r (replace / set / insert / push).
//! - [`plan`] / [`plan_remove`] + [`apply_copy`]: two passes over any [`Source`], for
//!   all operations including removal.
//!
//! In-place edits are not atomic: if interrupted (power loss) while the tail is being
//! moved, the document is corrupted. Use the copy functions to write a new file and swap
//! it in when that matters.

use core::fmt;

use crate::error::{Error, ErrorKind};
use crate::io::{PipeError, Source, Tee, Write};
use crate::number::fmt_u64;
use crate::parser::{Parser, Res};
use crate::path::{Path, Seg};
use crate::util::PipeResult;
use crate::writer::{JsonWriter, ToJson, encoded_len};
use crate::{Span, Token};

/// Random-access byte storage holding a JSON document (RAM, a file, flash, EEPROM...).
#[allow(clippy::len_without_is_empty)]
pub trait Storage {
    /// Error type.
    type Error;
    /// Current length of the document.
    fn len(&mut self) -> Result<u64, Self::Error>;
    /// Reads exactly `buf.len()` bytes at `offset` (always within the current length).
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), Self::Error>;
    /// Writes `data` at `offset` (always within the current length).
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), Self::Error>;
    /// Grows or shrinks the document. Growing may fail if there is no room.
    fn set_len(&mut self, len: u64) -> Result<(), Self::Error>;
    /// Moves `len` bytes from `from` to `to` (ranges may overlap), using `scratch` as a
    /// buffer. The default implementation copies through `scratch` in chunks.
    fn move_within(&mut self, from: u64, to: u64, len: u64, scratch: &mut [u8]) -> Result<(), Self::Error> {
        let mut small = [0u8; 16];
        let buf: &mut [u8] = if scratch.is_empty() { &mut small } else { scratch };
        let chunk = buf.len() as u64;
        if to < from {
            let mut done = 0;
            while done < len {
                let b = &mut buf[..chunk.min(len - done) as usize];
                self.read_at(from + done, b)?;
                self.write_at(to + done, b)?;
                done += b.len() as u64;
            }
        } else if to > from {
            let mut left = len;
            while left > 0 {
                let n = chunk.min(left);
                left -= n;
                let b = &mut buf[..n as usize];
                self.read_at(from + left, b)?;
                self.write_at(to + left, b)?;
            }
        }
        Ok(())
    }
}

impl<T: Storage + ?Sized> Storage for &mut T {
    type Error = T::Error;
    fn len(&mut self) -> Result<u64, Self::Error> {
        (**self).len()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), Self::Error> {
        (**self).read_at(offset, buf)
    }
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), Self::Error> {
        (**self).write_at(offset, data)
    }
    fn set_len(&mut self, len: u64) -> Result<(), Self::Error> {
        (**self).set_len(len)
    }
    fn move_within(&mut self, from: u64, to: u64, len: u64, scratch: &mut [u8]) -> Result<(), Self::Error> {
        (**self).move_within(from, to, len, scratch)
    }
}

/// Error of [`MemStorage`]: the edit does not fit in the buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapacityError;

impl fmt::Display for CapacityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("buffer capacity exceeded")
    }
}

impl core::error::Error for CapacityError {}

/// A document in a RAM buffer: the first `len` bytes of `buf` hold the JSON, the rest is
/// room to grow.
#[derive(Debug)]
pub struct MemStorage<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> MemStorage<'a> {
    /// Wraps `buf`, whose first `len` bytes are the document.
    ///
    /// # Panics
    /// If `len > buf.len()`.
    pub fn new(buf: &'a mut [u8], len: usize) -> Self {
        assert!(len <= buf.len(), "document length exceeds buffer");
        Self { buf, len }
    }

    /// The document.
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Length of the document.
    pub fn doc_len(&self) -> usize {
        self.len
    }

    /// Size of the buffer.
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Returns the buffer and the document length.
    pub fn into_inner(self) -> (&'a mut [u8], usize) {
        (self.buf, self.len)
    }

    fn range(&self, offset: u64, n: usize) -> Result<core::ops::Range<usize>, CapacityError> {
        let start = usize::try_from(offset).map_err(|_| CapacityError)?;
        let end = start.checked_add(n).ok_or(CapacityError)?;
        if end > self.len { Err(CapacityError) } else { Ok(start..end) }
    }
}

impl Storage for MemStorage<'_> {
    type Error = CapacityError;
    fn len(&mut self) -> Result<u64, CapacityError> {
        Ok(self.len as u64)
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), CapacityError> {
        let r = self.range(offset, buf.len())?;
        buf.copy_from_slice(&self.buf[r]);
        Ok(())
    }
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), CapacityError> {
        let r = self.range(offset, data.len())?;
        self.buf[r].copy_from_slice(data);
        Ok(())
    }
    fn set_len(&mut self, len: u64) -> Result<(), CapacityError> {
        match usize::try_from(len) {
            Ok(l) if l <= self.buf.len() => {
                self.len = l;
                Ok(())
            }
            _ => Err(CapacityError),
        }
    }
    fn move_within(&mut self, from: u64, to: u64, len: u64, _: &mut [u8]) -> Result<(), CapacityError> {
        let n = usize::try_from(len).map_err(|_| CapacityError)?;
        let src = self.range(from, n)?;
        let dst = self.range(to, n)?;
        self.buf.copy_within(src, dst.start);
        Ok(())
    }
}

#[cfg(feature = "std")]
impl Storage for std::fs::File {
    type Error = std::io::Error;
    fn len(&mut self) -> std::io::Result<u64> {
        Ok(self.metadata()?.len())
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        self.seek(SeekFrom::Start(offset))?;
        self.read_exact(buf)
    }
    fn write_at(&mut self, offset: u64, data: &[u8]) -> std::io::Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        self.seek(SeekFrom::Start(offset))?;
        self.write_all(data)
    }
    fn set_len(&mut self, len: u64) -> std::io::Result<()> {
        std::fs::File::set_len(self, len)
    }
}

/// A [`Source`] reading a [`Storage`] through a caller-provided buffer.
#[derive(Debug)]
pub struct StorageSource<'a, St: ?Sized> {
    st: &'a mut St,
    buf: &'a mut [u8],
    start: usize,
    end: usize,
    /// Storage offset of `buf[end]`.
    pos: u64,
    len: u64,
}

impl<'a, St: Storage + ?Sized> StorageSource<'a, St> {
    /// Creates a source reading the storage from the beginning. `buf` must not be empty.
    pub fn new(st: &'a mut St, buf: &'a mut [u8]) -> Result<Self, St::Error> {
        Self::at(st, buf, 0)
    }

    /// Creates a source reading the storage from `offset`.
    pub fn at(st: &'a mut St, buf: &'a mut [u8], offset: u64) -> Result<Self, St::Error> {
        let len = st.len()?;
        Ok(Self { st, buf, start: 0, end: 0, pos: offset, len })
    }
}

impl<St: Storage + ?Sized> Source for StorageSource<'_, St> {
    type Error = St::Error;
    fn fill(&mut self) -> Result<&[u8], St::Error> {
        if self.start == self.end && self.pos < self.len {
            let n = (self.buf.len() as u64).min(self.len - self.pos) as usize;
            self.st.read_at(self.pos, &mut self.buf[..n])?;
            self.start = 0;
            self.end = n;
            self.pos += n as u64;
        }
        Ok(&self.buf[self.start..self.end])
    }
    fn consume(&mut self, n: usize) {
        self.start = (self.start + n).min(self.end);
    }
    fn offset(&self) -> u64 {
        self.pos - (self.end - self.start) as u64
    }
}

/// Kind of modification for [`Editor::edit`], [`plan`] and [`copy_edit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Replace an existing value. Nothing happens if the path does not exist.
    Replace,
    /// Replace the value if it exists, otherwise create it: add the member to its parent
    /// object, or append the element if the last segment is the array length or `-`.
    /// The parent must exist.
    Set,
    /// Like [`Op::Set`] for objects; in arrays, insert before the element at that index
    /// (shifting the following elements), or append for the array length or `-`.
    Insert,
    /// Append the value to the array at the path.
    Push,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind<'k> {
    Value,
    Remove,
    Member { lead: bool, key: Seg<'k> },
    Element { lead: bool, trail: bool },
}

/// A planned modification: bytes `start..end` of the document are replaced with new
/// content (a value, possibly with a member name and commas around it, or nothing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Patch<'k> {
    /// Offset of the first byte replaced.
    pub start: u64,
    /// Offset after the last byte replaced (equal to `start` for insertions).
    pub end: u64,
    kind: Kind<'k>,
}

impl<'k> Patch<'k> {
    /// A patch replacing a value at `span` (as returned by
    /// [`Parser::value_span`] or [`Node::span`](crate::Node::span)).
    pub fn replace(span: Span) -> Self {
        Self { start: span.start, end: span.end, kind: Kind::Value }
    }

    /// The bytes replaced by this patch.
    pub fn span(&self) -> Span {
        Span { start: self.start, end: self.end }
    }

    /// Whether this patch deletes bytes without writing anything.
    pub fn is_removal(&self) -> bool {
        self.kind == Kind::Remove
    }

    /// Length of the content this patch writes for `value`.
    pub fn content_len<V: ToJson + ?Sized>(&self, value: &V) -> u64 {
        encoded_len(&self.content(value))
    }

    /// Change of the document length when applied with `value`.
    pub fn delta<V: ToJson + ?Sized>(&self, value: &V) -> i64 {
        self.content_len(value) as i64 - (self.end - self.start) as i64
    }

    /// Writes the content this patch puts at `start` for `value`.
    pub fn write_content<W: Write, V: ToJson + ?Sized>(&self, out: W, value: &V) -> Result<(), W::Error> {
        self.content(value).to_json(&mut JsonWriter::new(out))
    }

    fn content<'v, V: ?Sized>(&self, value: &'v V) -> Content<'v, 'k, V> {
        Content { kind: self.kind, value }
    }
}

struct Content<'v, 'k, V: ?Sized> {
    kind: Kind<'k>,
    value: &'v V,
}

impl<V: ToJson + ?Sized> ToJson for Content<'_, '_, V> {
    fn to_json<W: Write>(&self, w: &mut JsonWriter<W>) -> Result<(), W::Error> {
        match self.kind {
            Kind::Value => w.value(self.value),
            Kind::Remove => Ok(()),
            Kind::Member { lead, key } => {
                if lead {
                    w.raw_fragment(b",")?;
                }
                write_key(w, key)?;
                w.value(self.value)
            }
            Kind::Element { lead, trail } => {
                if lead {
                    w.raw_fragment(b",")?;
                }
                w.value(self.value)?;
                if trail {
                    w.raw_fragment(b",")?;
                }
                Ok(())
            }
        }
    }
}

fn write_key<W: Write>(w: &mut JsonWriter<W>, key: Seg<'_>) -> Result<(), W::Error> {
    match key {
        Seg::Key(k) => w.key(k),
        Seg::Index(i) => {
            let mut buf = [0u8; 20];
            let digits = fmt_u64(i as u64, &mut buf);
            w.key(core::str::from_utf8(digits).unwrap_or_default())
        }
        Seg::Pointer(mut rest) => {
            w.begin_string()?;
            while let Some(i) = rest.find('~') {
                w.string_fragment(&rest[..i])?;
                let tail = &rest[i + 1..];
                match tail.as_bytes().first() {
                    Some(b'0') => {
                        w.string_fragment("~")?;
                        rest = &tail[1..];
                    }
                    Some(b'1') => {
                        w.string_fragment("/")?;
                        rest = &tail[1..];
                    }
                    _ => {
                        w.string_fragment("~")?;
                        rest = tail;
                    }
                }
            }
            w.string_fragment(rest)?;
            w.end_key()
        }
    }
}

/// Where an edit goes, found with the parser stopped exactly at `start`.
struct Target<'k> {
    start: u64,
    /// Whether the value at `start` is replaced (and must be skipped).
    replace: bool,
    kind: Kind<'k>,
}

/// Finds where `op` applies. On success the parser has consumed exactly up to `start`.
fn locate<'k, S: Source, const N: usize, P: Path + ?Sized>(
    p: &mut Parser<S, N>,
    op: Op,
    path: &'k P,
) -> Res<Option<Target<'k>>, S> {
    let value_here = |p: &mut Parser<S, N>| -> Res<Option<Target<'k>>, S> {
        Ok(Some(Target { start: p.offset(), replace: true, kind: Kind::Value }))
    };
    let n = path.segments().count();
    if op == Op::Push {
        if !p.seek(path)? || p.peek()? != Token::BeginArray {
            return Ok(None);
        }
        p.begin_array()?;
        let mut any = false;
        while p.peek()? != Token::EndArray {
            p.skip_value()?;
            any = true;
        }
        return Ok(Some(Target { start: p.offset(), replace: false, kind: Kind::Element { lead: any, trail: false } }));
    }
    if op == Op::Replace || n == 0 {
        return if p.seek(path)? { value_here(p) } else { Ok(None) };
    }
    let Some(last) = path.segments().nth(n - 1) else { return Ok(None) };
    if !p.seek_segs(path.segments().take(n - 1))? {
        return Ok(None);
    }
    match p.peek()? {
        Token::BeginObject => {
            p.begin_object()?;
            let mut any = false;
            loop {
                match p.peek()? {
                    Token::Key => {
                        if p.match_seg_key(&last)? {
                            p.peek()?;
                            return value_here(p);
                        }
                        p.skip_value()?;
                        any = true;
                    }
                    _ => {
                        return Ok(Some(Target {
                            start: p.offset(),
                            replace: false,
                            kind: Kind::Member { lead: any, key: last },
                        }));
                    }
                }
            }
        }
        Token::BeginArray => {
            let target = if last.is_end_marker() {
                None
            } else {
                match last.index() {
                    Some(i) => Some(i),
                    None => return Ok(None),
                }
            };
            p.begin_array()?;
            let mut i = 0;
            loop {
                if p.peek()? == Token::EndArray {
                    if target.is_none() || target == Some(i) {
                        let kind = Kind::Element { lead: i > 0, trail: false };
                        return Ok(Some(Target { start: p.offset(), replace: false, kind }));
                    }
                    return Ok(None);
                }
                if target == Some(i) {
                    if op == Op::Insert {
                        let kind = Kind::Element { lead: false, trail: true };
                        return Ok(Some(Target { start: p.offset(), replace: false, kind }));
                    }
                    return value_here(p);
                }
                p.skip_value()?;
                i += 1;
            }
        }
        _ => Ok(None),
    }
}

/// Computes the [`Patch`] for `op` at `path`, reading the document with `p` (positioned
/// before the root value). Returns `None` if the operation does not apply (path not
/// found, parent missing or not a container, index out of range...).
pub fn plan<'k, S: Source, const N: usize, P: Path + ?Sized>(
    p: &mut Parser<S, N>,
    op: Op,
    path: &'k P,
) -> Res<Option<Patch<'k>>, S> {
    let Some(t) = locate(p, op, path)? else { return Ok(None) };
    let end = if t.replace { p.value_span()?.end } else { t.start };
    Ok(Some(Patch { start: t.start, end, kind: t.kind }))
}

/// Computes the [`Patch`] removing the member or element at `path` (with the comma that
/// separates it from its neighbours). Returns `None` if it does not exist. The root
/// cannot be removed.
pub fn plan_remove<S: Source, const N: usize, P: Path + ?Sized>(
    p: &mut Parser<S, N>,
    path: &P,
) -> Res<Option<Patch<'static>>, S> {
    let n = path.segments().count();
    let Some(last) = n.checked_sub(1).and_then(|l| path.segments().nth(l)) else { return Ok(None) };
    if !p.seek_segs(path.segments().take(n - 1))? {
        return Ok(None);
    }
    let removal = |start, end| Ok(Some(Patch { start, end, kind: Kind::Remove }));
    let mut prev_end = None;
    match p.peek()? {
        Token::BeginObject => {
            p.begin_object()?;
            while p.peek()? == Token::Key {
                let start = p.offset();
                if p.match_seg_key(&last)? {
                    let value = p.value_span()?;
                    return match p.peek()? {
                        Token::Key => removal(start, p.offset()),
                        _ => removal(prev_end.unwrap_or(start), value.end),
                    };
                }
                p.skip_value()?;
                prev_end = Some(p.offset());
            }
            Ok(None)
        }
        Token::BeginArray => {
            let Some(target) = last.index() else { return Ok(None) };
            p.begin_array()?;
            let mut i = 0;
            while p.peek()? != Token::EndArray {
                let start = p.offset();
                if i == target {
                    let value = p.value_span()?;
                    return match p.peek()? {
                        Token::EndArray => removal(prev_end.unwrap_or(start), value.end),
                        _ => removal(start, p.offset()),
                    };
                }
                p.skip_value()?;
                prev_end = Some(p.offset());
                i += 1;
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

/// Error of [`StorageWriter`].
enum WriteError<E> {
    Io(E),
    Overflow,
}

/// Buffered writer into storage, limited to a range.
struct StorageWriter<'a, St: ?Sized> {
    st: &'a mut St,
    pos: u64,
    end: u64,
    buf: &'a mut [u8],
    n: usize,
}

impl<St: Storage + ?Sized> StorageWriter<'_, St> {
    fn flush_buf(&mut self) -> Result<(), St::Error> {
        if self.n > 0 {
            self.st.write_at(self.pos, &self.buf[..self.n])?;
            self.pos += self.n as u64;
            self.n = 0;
        }
        Ok(())
    }

    /// Flushes and pads the rest of the range with spaces.
    fn finish(&mut self) -> Result<(), St::Error> {
        self.flush_buf()?;
        while self.pos < self.end {
            const SPACES: &[u8; 16] = b"                ";
            let k = (self.end - self.pos).min(SPACES.len() as u64) as usize;
            self.st.write_at(self.pos, &SPACES[..k])?;
            self.pos += k as u64;
        }
        Ok(())
    }
}

impl<St: Storage + ?Sized> Write for StorageWriter<'_, St> {
    type Error = WriteError<St::Error>;
    fn write_all(&mut self, mut data: &[u8]) -> Result<(), Self::Error> {
        if self.pos + (self.n + data.len()) as u64 > self.end {
            return Err(WriteError::Overflow);
        }
        if self.buf.is_empty() {
            self.st.write_at(self.pos, data).map_err(WriteError::Io)?;
            self.pos += data.len() as u64;
            return Ok(());
        }
        while !data.is_empty() {
            let k = (self.buf.len() - self.n).min(data.len());
            self.buf[self.n..self.n + k].copy_from_slice(&data[..k]);
            self.n += k;
            data = &data[k..];
            if self.n == self.buf.len() {
                self.flush_buf().map_err(WriteError::Io)?;
            }
        }
        Ok(())
    }
}

/// Applies `patch` to the document in `st`, in place, writing `value` (ignored for
/// removals). Returns the span of the new content.
///
/// The document tail after the patch is moved once, through `scratch` (any size; larger
/// is faster for storage that does not implement [`Storage::move_within`] natively).
pub fn apply<St: Storage + ?Sized, V: ToJson + ?Sized>(
    st: &mut St,
    patch: &Patch<'_>,
    value: &V,
    scratch: &mut [u8],
) -> Result<Span, Error<St::Error>> {
    let content = patch.content(value);
    let new_len = encoded_len(&content);
    let total = st.len().map_err(Error::Io)?;
    let Patch { start, end, .. } = *patch;
    if start > end || end > total {
        return Err(Error::Json { kind: ErrorKind::InvalidState, offset: start });
    }
    let old_len = end - start;
    let new_end = start + new_len;
    let tail = total - end;
    if new_len > old_len {
        st.set_len(total + (new_len - old_len)).map_err(Error::Io)?;
        st.move_within(end, new_end, tail, scratch).map_err(Error::Io)?;
    } else if new_len < old_len {
        st.move_within(end, new_end, tail, scratch).map_err(Error::Io)?;
        st.set_len(total - (old_len - new_len)).map_err(Error::Io)?;
    }
    let mut w = StorageWriter { st, pos: start, end: new_end, buf: scratch, n: 0 };
    match content.to_json(&mut JsonWriter::new(&mut w)) {
        Ok(()) => w.finish().map_err(Error::Io)?,
        Err(WriteError::Io(e)) => return Err(Error::Io(e)),
        // `value` wrote more on the second call than when it was measured.
        Err(WriteError::Overflow) => return Err(Error::Json { kind: ErrorKind::InvalidState, offset: start }),
    }
    Ok(Span { start, end: new_end })
}

/// Copies bytes from `src` to `dst` (or drops them if `dst` is `None`) until offset
/// `until` (or the end of input if `None`).
fn pump<S: Source, W: Write>(src: &mut S, mut dst: Option<&mut W>, until: Option<u64>) -> PipeResult<(), S, W> {
    loop {
        let remaining = until.map(|u| u - src.offset());
        if remaining == Some(0) {
            return Ok(());
        }
        let buf = src.fill().map_err(|e| Error::Io(PipeError::Read(e)))?;
        if buf.is_empty() {
            return match until {
                None => Ok(()),
                Some(_) => Err(Error::Json { kind: ErrorKind::UnexpectedEof, offset: src.offset() }),
            };
        }
        let n = remaining.map_or(buf.len(), |r| buf.len().min(r.try_into().unwrap_or(usize::MAX)));
        if let Some(d) = dst.as_mut() {
            d.write_all(&buf[..n]).map_err(|e| Error::Io(PipeError::Write(e)))?;
        }
        src.consume(n);
    }
}

/// Copies the document from `src` to `dst`, applying `patch` on the way.
///
/// `src` must be positioned at or before `patch.start` (typically a fresh source over the
/// same data that was used to [`plan`] the patch).
pub fn apply_copy<S: Source, W: Write, V: ToJson + ?Sized>(
    mut src: S,
    mut dst: W,
    patch: &Patch<'_>,
    value: &V,
) -> PipeResult<(), S, W> {
    if src.offset() > patch.start || patch.start > patch.end {
        return Err(Error::Json { kind: ErrorKind::InvalidState, offset: src.offset() });
    }
    pump(&mut src, Some(&mut dst), Some(patch.start))?;
    patch.write_content(&mut dst, value).map_err(|e| Error::Io(PipeError::Write(e)))?;
    pump::<S, W>(&mut src, None, Some(patch.end))?;
    pump(&mut src, Some(&mut dst), None)?;
    dst.flush().map_err(|e| Error::Io(PipeError::Write(e)))
}

/// Copies the document from `src` to `dst` in a single pass, applying `op` at `path`.
///
/// Memory use is the parser (default 64 nesting levels) plus whatever buffer `src` has.
/// Returns whether the operation applied; if not, the output is an exact copy.
///
/// ```
/// use emjson::edit::{copy_edit, Op};
/// use emjson::io::{ReadSource, SliceWriter};
///
/// let input = br#"{"foo": {"bar": "hello world"}, "n": 1}"#;
/// let (mut rbuf, mut out) = ([0u8; 8], [0u8; 64]);
/// let src = ReadSource::new(&input[..], &mut rbuf); // any stream, tiny buffer
/// let mut dst = SliceWriter::new(&mut out);
/// assert!(copy_edit(src, &mut dst, Op::Replace, &["foo", "bar"], "bye").unwrap());
/// assert_eq!(dst.written(), br#"{"foo": {"bar": "bye"}, "n": 1}"#);
/// ```
pub fn copy_edit<S: Source, W: Write, P: Path + ?Sized, V: ToJson + ?Sized>(
    src: S,
    dst: W,
    op: Op,
    path: &P,
    value: &V,
) -> PipeResult<bool, S, W> {
    let mut p = Parser::new(Tee::new(src, dst));
    let target = locate(&mut p, op, path)?;
    let found = target.is_some();
    if let Some(t) = target {
        p.source_mut().set_enabled(false).map_err(Error::Io)?;
        if t.replace {
            p.skip_value()?;
        }
        p.source_mut().set_enabled(true).map_err(Error::Io)?;
        let w = p.source_mut().writer().map_err(Error::Io)?;
        let content = Content { kind: t.kind, value };
        content.to_json(&mut JsonWriter::new(w)).map_err(|e| Error::Io(PipeError::Write(e)))?;
    }
    p.source_mut().copy_rest().map_err(Error::Io)?;
    Ok(found)
}

/// Edits a JSON document in [`Storage`] in place.
///
/// ```
/// use emjson::edit::{Editor, MemStorage};
///
/// let mut buf = [0u8; 128];
/// let doc = br#"{"foo": {"bar": "hello world"}, "list": [1, 2]}"#;
/// buf[..doc.len()].copy_from_slice(doc);
///
/// let mut scratch = [0u8; 32];
/// let mut ed = Editor::new(MemStorage::new(&mut buf, doc.len()), &mut scratch);
/// ed.replace(&["foo", "bar"], "hi")?;
/// ed.set("/foo/baz", &[true, false])?;
/// ed.push("/list", &3)?;
/// ed.remove("/list/0")?;
/// assert_eq!(ed.storage().as_bytes(), br#"{"foo": {"bar": "hi","baz":[true,false]}, "list": [2,3]}"#);
/// # Ok::<(), emjson::Error<emjson::edit::CapacityError>>(())
/// ```
#[derive(Debug)]
pub struct Editor<'s, St, const N: usize = 8> {
    st: St,
    scratch: &'s mut [u8],
}

impl<'s, St: Storage> Editor<'s, St> {
    /// Creates an editor over `storage` with the default nesting depth (64). `scratch` is
    /// used to read and move data; it must not be empty.
    pub fn new(storage: St, scratch: &'s mut [u8]) -> Self {
        Self::with_stack(storage, scratch)
    }
}

impl<'s, St: Storage, const N: usize> Editor<'s, St, N> {
    /// Creates an editor whose parser has a nesting stack of `N` bytes (`8 * N` levels).
    pub fn with_stack(storage: St, scratch: &'s mut [u8]) -> Self {
        assert!(!scratch.is_empty(), "Editor needs a non-empty scratch buffer");
        Self { st: storage, scratch }
    }

    /// The storage.
    pub fn storage(&self) -> &St {
        &self.st
    }

    /// The storage.
    pub fn storage_mut(&mut self) -> &mut St {
        &mut self.st
    }

    /// Returns the storage.
    pub fn into_inner(self) -> St {
        self.st
    }

    /// Runs `f` with a parser positioned at the start of the document.
    pub fn parse<T, F>(&mut self, f: F) -> Result<T, Error<St::Error>>
    where
        F: FnOnce(&mut Parser<StorageSource<'_, St>, N>) -> Result<T, Error<St::Error>>,
    {
        let src = StorageSource::new(&mut self.st, &mut *self.scratch).map_err(Error::Io)?;
        f(&mut Parser::with_stack(src))
    }

    /// Returns the span of the value at `path`.
    pub fn locate<P: Path + ?Sized>(&mut self, path: &P) -> Result<Option<Span>, Error<St::Error>> {
        self.parse(|p| if p.seek(path)? { Ok(Some(p.value_span()?)) } else { Ok(None) })
    }

    /// Applies `op` at `path` with `value`. Returns whether it applied.
    pub fn edit<P: Path + ?Sized, V: ToJson + ?Sized>(
        &mut self,
        op: Op,
        path: &P,
        value: &V,
    ) -> Result<bool, Error<St::Error>> {
        let Some(patch) = self.parse(|p| plan(p, op, path))? else { return Ok(false) };
        self.apply(&patch, value)?;
        Ok(true)
    }

    /// Replaces the value at `path`. Returns `false` if it does not exist.
    pub fn replace<P: Path + ?Sized, V: ToJson + ?Sized>(
        &mut self,
        path: &P,
        value: &V,
    ) -> Result<bool, Error<St::Error>> {
        self.edit(Op::Replace, path, value)
    }

    /// Replaces or creates the value at `path` (see [`Op::Set`]).
    pub fn set<P: Path + ?Sized, V: ToJson + ?Sized>(&mut self, path: &P, value: &V) -> Result<bool, Error<St::Error>> {
        self.edit(Op::Set, path, value)
    }

    /// Inserts the value at `path` (see [`Op::Insert`]).
    pub fn insert<P: Path + ?Sized, V: ToJson + ?Sized>(
        &mut self,
        path: &P,
        value: &V,
    ) -> Result<bool, Error<St::Error>> {
        self.edit(Op::Insert, path, value)
    }

    /// Appends the value to the array at `path`.
    pub fn push<P: Path + ?Sized, V: ToJson + ?Sized>(
        &mut self,
        path: &P,
        value: &V,
    ) -> Result<bool, Error<St::Error>> {
        self.edit(Op::Push, path, value)
    }

    /// Removes the member or element at `path`. Returns `false` if it does not exist.
    pub fn remove<P: Path + ?Sized>(&mut self, path: &P) -> Result<bool, Error<St::Error>> {
        let Some(patch) = self.parse(|p| plan_remove(p, path))? else { return Ok(false) };
        self.apply(&patch, &())?;
        Ok(true)
    }

    /// Applies a patch (see [`apply`]).
    pub fn apply<V: ToJson + ?Sized>(&mut self, patch: &Patch<'_>, value: &V) -> Result<Span, Error<St::Error>> {
        apply(&mut self.st, patch, value, self.scratch)
    }

    /// Replaces the bytes at `span` (typically a value's span) with `value`.
    pub fn splice<V: ToJson + ?Sized>(&mut self, span: Span, value: &V) -> Result<Span, Error<St::Error>> {
        self.apply(&Patch::replace(span), value)
    }
}
