//! Minimal I/O abstractions: byte sources for the parser, byte sinks for the writer.
//!
//! The parser reads through a [`Source`], a `BufRead`-like trait that lets it look at
//! buffered bytes without consuming them. This is what makes byte-exact offsets
//! possible: after [`Parser::peek`](crate::Parser::peek), [`Source::offset`] is exactly the
//! position of the first byte of the next token.
//!
//! Provided sources:
//! - [`SliceSource`]: a JSON document in memory (zero copy).
//! - [`ReadSource`]: any [`Read`] stream, buffered through a caller-provided buffer.
//! - [`StorageSource`](crate::edit::StorageSource): random-access [`Storage`](crate::edit::Storage).
//! - [`Tee`]: wraps another source and copies every consumed byte to a [`Write`]r,
//!   which allows editing a stream while copying it.

use core::convert::Infallible;
use core::fmt;

/// A stream of bytes (like `std::io::Read`).
pub trait Read {
    /// Error type.
    type Error;
    /// Reads some bytes into `buf`, returning how many were read. `Ok(0)` means end of stream
    /// (when `buf` is not empty).
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error>;
}

impl<R: Read + ?Sized> Read for &mut R {
    type Error = R::Error;
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        (**self).read(buf)
    }
}

impl Read for &[u8] {
    type Error = Infallible;
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Infallible> {
        let n = buf.len().min(self.len());
        buf[..n].copy_from_slice(&self[..n]);
        *self = &self[n..];
        Ok(n)
    }
}

/// A byte sink (like `std::io::Write`).
pub trait Write {
    /// Error type.
    type Error;
    /// Writes all of `data`.
    fn write_all(&mut self, data: &[u8]) -> Result<(), Self::Error>;
    /// Flushes buffered data, if any.
    fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl<W: Write + ?Sized> Write for &mut W {
    type Error = W::Error;
    fn write_all(&mut self, data: &[u8]) -> Result<(), Self::Error> {
        (**self).write_all(data)
    }
    fn flush(&mut self) -> Result<(), Self::Error> {
        (**self).flush()
    }
}

/// A buffered byte source for the parser (like `std::io::BufRead`).
///
/// Contract: [`fill`](Source::fill) returns the currently buffered, unconsumed bytes. It must
/// only read more data when the buffer is empty, so that consecutive calls without
/// [`consume`](Source::consume) return the same bytes. An empty slice means end of input.
pub trait Source {
    /// Error type of the underlying I/O.
    type Error;
    /// Returns buffered bytes, reading more if none are buffered. Empty means end of input.
    fn fill(&mut self) -> Result<&[u8], Self::Error>;
    /// Marks `n` bytes (at most the length returned by `fill`) as consumed.
    fn consume(&mut self, n: usize);
    /// Offset of the next unconsumed byte, counted from the start of the input.
    fn offset(&self) -> u64;
}

impl<S: Source + ?Sized> Source for &mut S {
    type Error = S::Error;
    #[inline]
    fn fill(&mut self) -> Result<&[u8], Self::Error> {
        (**self).fill()
    }
    #[inline]
    fn consume(&mut self, n: usize) {
        (**self).consume(n)
    }
    #[inline]
    fn offset(&self) -> u64 {
        (**self).offset()
    }
}

/// A [`Source`] over a JSON document in memory.
#[derive(Debug, Clone)]
pub struct SliceSource<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> SliceSource<'a> {
    /// Creates a source reading `data` from the beginning.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// The whole input.
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    /// The unconsumed part of the input.
    pub fn remaining(&self) -> &'a [u8] {
        &self.data[self.pos..]
    }
}

impl Source for SliceSource<'_> {
    type Error = Infallible;
    #[inline]
    fn fill(&mut self) -> Result<&[u8], Infallible> {
        Ok(&self.data[self.pos..])
    }
    #[inline]
    fn consume(&mut self, n: usize) {
        self.pos = (self.pos + n).min(self.data.len());
    }
    #[inline]
    fn offset(&self) -> u64 {
        self.pos as u64
    }
}

/// A [`Source`] reading from a [`Read`] stream through a caller-provided buffer.
///
/// Any buffer size of at least one byte works; larger buffers mean fewer `read` calls.
#[derive(Debug)]
pub struct ReadSource<'b, R> {
    reader: R,
    buf: &'b mut [u8],
    start: usize,
    end: usize,
    offset: u64,
}

impl<'b, R: Read> ReadSource<'b, R> {
    /// Creates a buffered source. `buf` must not be empty.
    pub fn new(reader: R, buf: &'b mut [u8]) -> Self {
        assert!(!buf.is_empty(), "ReadSource needs a non-empty buffer");
        Self { reader, buf, start: 0, end: 0, offset: 0 }
    }

    /// Sets the offset reported for the next byte (use when the stream does not start at
    /// the beginning of the document, e.g. after seeking a file).
    pub fn with_offset(mut self, offset: u64) -> Self {
        self.offset = offset;
        self
    }

    /// The underlying reader.
    pub fn get_ref(&self) -> &R {
        &self.reader
    }

    /// The underlying reader. Reading from it directly skips the buffered bytes.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.reader
    }

    /// Bytes read from the reader but not consumed yet.
    pub fn buffered(&self) -> &[u8] {
        &self.buf[self.start..self.end]
    }

    /// Returns the reader, dropping buffered data.
    pub fn into_inner(self) -> R {
        self.reader
    }
}

impl<R: Read> Source for ReadSource<'_, R> {
    type Error = R::Error;
    #[inline]
    fn fill(&mut self) -> Result<&[u8], R::Error> {
        if self.start == self.end {
            let n = self.reader.read(self.buf)?;
            self.start = 0;
            self.end = n.min(self.buf.len());
        }
        Ok(&self.buf[self.start..self.end])
    }
    #[inline]
    fn consume(&mut self, n: usize) {
        let n = n.min(self.end - self.start);
        self.start += n;
        self.offset += n as u64;
    }
    #[inline]
    fn offset(&self) -> u64 {
        self.offset
    }
}

/// Error of an operation that reads from one side and writes to another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeError<R, W> {
    /// The source failed.
    Read(R),
    /// The sink failed.
    Write(W),
}

impl<R: fmt::Display, W: fmt::Display> fmt::Display for PipeError<R, W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(e) => write!(f, "read error: {e}"),
            Self::Write(e) => write!(f, "write error: {e}"),
        }
    }
}

impl<R, W> core::error::Error for PipeError<R, W>
where
    R: core::error::Error + 'static,
    W: core::error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Read(e) => Some(e),
            Self::Write(e) => Some(e),
        }
    }
}

/// A [`Source`] adapter that copies every consumed byte to a [`Write`]r.
///
/// Used to edit a stream while copying it with no extra buffer: parse up to the value to
/// change, [disable](Tee::set_enabled) copying, skip the old value, re-enable, write the
/// new value to the [writer](Tee::writer), then [copy the rest](Tee::copy_rest).
/// See [`edit::copy_edit`](crate::edit::copy_edit).
///
/// Consumed bytes are forwarded lazily (in whole chunks) when the inner source needs to
/// refill; call [`sync`](Tee::sync) to forward them immediately.
#[derive(Debug)]
pub struct Tee<S, W> {
    src: S,
    dst: W,
    pending: usize,
    enabled: bool,
}

impl<S: Source, W: Write> Tee<S, W> {
    /// Creates a tee with copying enabled.
    pub fn new(src: S, dst: W) -> Self {
        Self { src, dst, pending: 0, enabled: true }
    }

    /// Whether consumed bytes are currently copied to the writer.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Enables or disables copying. Bytes consumed so far are forwarded (or dropped)
    /// according to the previous setting first.
    pub fn set_enabled(&mut self, enabled: bool) -> Result<(), PipeError<S::Error, W::Error>> {
        self.sync()?;
        self.enabled = enabled;
        Ok(())
    }

    /// Forwards all consumed bytes to the writer now.
    pub fn sync(&mut self) -> Result<(), PipeError<S::Error, W::Error>> {
        if self.pending == 0 {
            return Ok(());
        }
        let buf = self.src.fill().map_err(PipeError::Read)?;
        let n = self.pending.min(buf.len());
        if self.enabled {
            self.dst.write_all(&buf[..n]).map_err(PipeError::Write)?;
        }
        self.src.consume(n);
        self.pending = 0;
        Ok(())
    }

    /// Returns the writer, after forwarding all consumed bytes, so data can be inserted
    /// at the current position.
    pub fn writer(&mut self) -> Result<&mut W, PipeError<S::Error, W::Error>> {
        self.sync()?;
        Ok(&mut self.dst)
    }

    /// Copies everything that is left in the source to the writer (regardless of the
    /// enabled flag) and flushes it. Returns the number of bytes copied.
    pub fn copy_rest(&mut self) -> Result<u64, PipeError<S::Error, W::Error>> {
        self.sync()?;
        let mut total = 0;
        loop {
            let buf = self.src.fill().map_err(PipeError::Read)?;
            if buf.is_empty() {
                break;
            }
            let n = buf.len();
            self.dst.write_all(buf).map_err(PipeError::Write)?;
            self.src.consume(n);
            total += n as u64;
        }
        self.dst.flush().map_err(PipeError::Write)?;
        Ok(total)
    }

    /// Returns the source and the writer. Call [`sync`](Tee::sync) first, or consumed
    /// bytes that were not forwarded yet are lost.
    pub fn into_parts(self) -> (S, W) {
        (self.src, self.dst)
    }
}

impl<S: Source, W: Write> Source for Tee<S, W> {
    type Error = PipeError<S::Error, W::Error>;

    fn fill(&mut self) -> Result<&[u8], Self::Error> {
        if self.pending > 0 {
            let len = self.src.fill().map_err(PipeError::Read)?.len();
            if self.pending >= len {
                self.sync()?;
            }
        }
        let p = self.pending;
        let buf = self.src.fill().map_err(PipeError::Read)?;
        Ok(&buf[p.min(buf.len())..])
    }

    #[inline]
    fn consume(&mut self, n: usize) {
        self.pending += n;
    }

    #[inline]
    fn offset(&self) -> u64 {
        self.src.offset() + self.pending as u64
    }
}

/// A [`Write`]r that only counts bytes. Used to measure encoded lengths.
#[derive(Debug, Clone, Copy, Default)]
pub struct Counter {
    count: u64,
}

impl Counter {
    /// Creates a counter at zero.
    pub const fn new() -> Self {
        Self { count: 0 }
    }
    /// Bytes written so far.
    pub fn count(&self) -> u64 {
        self.count
    }
}

impl Write for Counter {
    type Error = Infallible;
    #[inline]
    fn write_all(&mut self, data: &[u8]) -> Result<(), Infallible> {
        self.count += data.len() as u64;
        Ok(())
    }
}

/// Error returned by [`SliceWriter`] when the buffer is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferFull;

impl fmt::Display for BufferFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("buffer full")
    }
}

impl core::error::Error for BufferFull {}

/// A [`Write`]r into a byte slice.
#[derive(Debug)]
pub struct SliceWriter<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> SliceWriter<'a> {
    /// Creates a writer filling `buf` from the start.
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, len: 0 }
    }
    /// Number of bytes written.
    pub fn len(&self) -> usize {
        self.len
    }
    /// Whether nothing was written.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    /// The bytes written so far.
    pub fn written(&self) -> &[u8] {
        &self.buf[..self.len]
    }
    /// Returns the written part of the buffer.
    pub fn into_written(self) -> &'a mut [u8] {
        &mut self.buf[..self.len]
    }
}

impl Write for SliceWriter<'_> {
    type Error = BufferFull;
    fn write_all(&mut self, data: &[u8]) -> Result<(), BufferFull> {
        let end = self.len.checked_add(data.len()).ok_or(BufferFull)?;
        self.buf.get_mut(self.len..end).ok_or(BufferFull)?.copy_from_slice(data);
        self.len = end;
        Ok(())
    }
}

/// Adapter implementing this crate's [`Read`]/[`Write`] for `std::io` types.
#[cfg(feature = "std")]
#[derive(Debug)]
pub struct StdIo<T>(pub T);

#[cfg(feature = "std")]
impl<T: std::io::Read> Read for StdIo<T> {
    type Error = std::io::Error;
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
        loop {
            match self.0.read(buf) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                r => return r,
            }
        }
    }
}

#[cfg(feature = "std")]
impl<T: std::io::Write> Write for StdIo<T> {
    type Error = std::io::Error;
    fn write_all(&mut self, data: &[u8]) -> Result<(), std::io::Error> {
        self.0.write_all(data)
    }
    fn flush(&mut self) -> Result<(), std::io::Error> {
        self.0.flush()
    }
}

/// Adapter implementing this crate's [`Read`]/[`Write`] for `embedded-io` types.
#[cfg(feature = "embedded-io")]
#[derive(Debug)]
pub struct EmbeddedIo<T>(pub T);

#[cfg(feature = "embedded-io")]
impl<T: embedded_io::Read> Read for EmbeddedIo<T> {
    type Error = T::Error;
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, T::Error> {
        self.0.read(buf)
    }
}

#[cfg(feature = "embedded-io")]
impl<T: embedded_io::Write> Write for EmbeddedIo<T> {
    type Error = T::Error;
    fn write_all(&mut self, data: &[u8]) -> Result<(), T::Error> {
        self.0.write_all(data)
    }
    fn flush(&mut self) -> Result<(), T::Error> {
        self.0.flush()
    }
}
