use core::fmt;
use core::str::FromStr;

use crate::Span;
use crate::error::{Error, ErrorKind};
use crate::io::{SliceSource, Source};
use crate::number::fmt_usize;
use crate::path::{Path, Seg};
use crate::stack::BitStack;
use crate::string::{BufSink, KeyMatcher, NullSink};

/// Result type used by parser methods.
pub(crate) type Res<T, S> = Result<T, Error<<S as Source>::Error>>;

/// Kind of the next token, as returned by [`Parser::peek`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Token {
    /// `null`
    Null,
    /// `true` or `false`
    Bool,
    /// A number.
    Number,
    /// A string value.
    String,
    /// `{`
    BeginObject,
    /// `[`
    BeginArray,
    /// An object member name (a string followed by `:`).
    Key,
    /// `}`
    EndObject,
    /// `]`
    EndArray,
    /// End of input after the top-level value.
    Eof,
}

impl Token {
    /// Whether this token starts a value.
    pub fn is_value(self) -> bool {
        matches!(self, Self::Null | Self::Bool | Self::Number | Self::String | Self::BeginObject | Self::BeginArray)
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Null => "null",
            Self::Bool => "boolean",
            Self::Number => "number",
            Self::String => "string",
            Self::BeginObject => "object",
            Self::BeginArray => "array",
            Self::Key => "object key",
            Self::EndObject => "'}'",
            Self::EndArray => "']'",
            Self::Eof => "end of input",
        })
    }
}

/// A parse event, as returned by [`Parser::next_event`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event<'b> {
    /// `{`
    BeginObject,
    /// `}`
    EndObject,
    /// `[`
    BeginArray,
    /// `]`
    EndArray,
    /// An object member name.
    Key(&'b str),
    /// A string value.
    String(&'b str),
    /// A number, as its (validated) JSON text.
    Number(&'b str),
    /// `true` or `false`.
    Bool(bool),
    /// `null`.
    Null,
    /// End of input.
    Eof,
}

/// What the parser expects next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    /// Before the top-level value.
    TopValue,
    /// After the top-level value.
    TopDone,
    /// After `[`: value or `]`.
    ArrFirst,
    /// After `,` in an array: value.
    ArrValue,
    /// After an array element: `,` or `]`.
    ArrComma,
    /// After `{`: key or `}`.
    ObjFirst,
    /// After `,` in an object: key.
    ObjKey,
    /// After a key: `:`.
    ObjColon,
    /// After `:`: value.
    ObjValue,
    /// After a member value: `,` or `}`.
    ObjComma,
    /// Inside a string whose opening quote was consumed.
    InStr { key: bool },
}

/// A streaming JSON parser (pull parser / cursor) with bounded memory.
///
/// The parser reads from a [`Source`] and never buffers more than a few bytes itself.
/// Values are *lazy*: [`peek`](Parser::peek) tells what comes next without consuming it,
/// then the caller decides to read it ([`read_str`](Parser::read_str),
/// [`read_num`](Parser::read_num), [`begin_object`](Parser::begin_object), ...),
/// skip it ([`skip_value`](Parser::skip_value)), measure it
/// ([`value_span`](Parser::value_span)), or navigate into it
/// ([`seek`](Parser::seek)).
///
/// `N` is the size in bytes of the nesting stack; the maximum nesting depth is `8 * N`
/// (the default `N = 8` allows 64 levels). The parser itself is `N + 8` bytes plus the
/// source. Use [`Parser::new`] for the default depth or
/// `Parser::<_, N>::with_stack(src)` for another one.
///
/// ```
/// use emjson::{Parser, Token};
///
/// let json = br#"{"foo": {"bar": "hello world", "n": [1, 2, 3]}}"#;
/// let mut p = Parser::from_slice(json);
/// assert!(p.seek(&["foo", "bar"])?);
/// assert_eq!(p.peek()?, Token::String);
/// assert_eq!(p.offset(), 16); // exact position of the opening quote
/// let mut buf = [0u8; 32];
/// assert_eq!(p.read_str(&mut buf)?, "hello world");
/// # Ok::<(), emjson::Error<core::convert::Infallible>>(())
/// ```
///
/// After a JSON syntax error or an I/O error, the parser state is unspecified and the
/// parser should be discarded. Errors caused by the caller (type mismatch, buffer too
/// small, number out of range) leave it usable.
pub struct Parser<S, const N: usize = 8> {
    pub(crate) src: S,
    pub(crate) stack: BitStack<N>,
    pub(crate) state: State,
    multi: bool,
    pub(crate) stash: [u8; 4],
    pub(crate) stash_len: u8,
}

impl<S: fmt::Debug, const N: usize> fmt::Debug for Parser<S, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Parser")
            .field("src", &self.src)
            .field("depth", &self.stack.depth())
            .field("state", &self.state)
            .finish()
    }
}

impl<S: Source> Parser<S> {
    /// Creates a parser with the default nesting stack (64 levels).
    pub fn new(src: S) -> Self {
        Self::with_stack(src)
    }
}

impl<'a> Parser<SliceSource<'a>> {
    /// Creates a parser for a document in memory, with the default nesting stack.
    pub fn from_slice(data: &'a [u8]) -> Self {
        Self::new(SliceSource::new(data))
    }
}

#[inline]
fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\n' | b'\r' | b'\t')
}

#[inline]
fn value_token(b: u8) -> Option<Token> {
    Some(match b {
        b'{' => Token::BeginObject,
        b'[' => Token::BeginArray,
        b'"' => Token::String,
        b'-' | b'0'..=b'9' => Token::Number,
        b't' | b'f' => Token::Bool,
        b'n' => Token::Null,
        _ => return None,
    })
}

impl<S: Source, const N: usize> Parser<S, N> {
    /// Maximum nesting depth supported by this parser.
    pub const MAX_DEPTH: usize = BitStack::<N>::MAX;

    /// Creates a parser with a nesting stack of `N` bytes (`8 * N` levels).
    pub fn with_stack(src: S) -> Self {
        Self { src, stack: BitStack::new(), state: State::TopValue, multi: false, stash: [0; 4], stash_len: 0 }
    }

    /// Accepts a sequence of top-level values separated by whitespace (e.g. JSON Lines /
    /// NDJSON) instead of reporting [`ErrorKind::TrailingData`] after the first one.
    pub fn allow_multiple_values(mut self, allow: bool) -> Self {
        self.multi = allow;
        self
    }

    /// The source.
    pub fn source(&self) -> &S {
        &self.src
    }

    /// The source. Consuming bytes directly desynchronizes the parser.
    pub fn source_mut(&mut self) -> &mut S {
        &mut self.src
    }

    /// Returns the source.
    pub fn into_source(self) -> S {
        self.src
    }

    /// Byte offset of the next unconsumed byte. Right after [`peek`](Parser::peek) this is
    /// the offset of the first byte of the next token.
    #[inline]
    pub fn offset(&self) -> u64 {
        self.src.offset()
    }

    /// Current nesting depth (0 at top level).
    #[inline]
    pub fn depth(&self) -> usize {
        self.stack.depth()
    }

    #[cold]
    pub(crate) fn fail<T>(&self, kind: ErrorKind) -> Res<T, S> {
        Err(Error::Json { kind, offset: self.src.offset() })
    }

    #[cold]
    fn mismatch<T>(&self, expected: Token, found: Token) -> Res<T, S> {
        self.fail(ErrorKind::TypeMismatch { expected, found })
    }

    #[inline]
    pub(crate) fn peek_byte(&mut self) -> Res<Option<u8>, S> {
        match self.src.fill() {
            Ok(buf) => Ok(buf.first().copied()),
            Err(e) => Err(Error::Io(e)),
        }
    }

    /// Skips whitespace, returning the next byte (not consumed) or `None` at end of input.
    fn skip_ws(&mut self) -> Res<Option<u8>, S> {
        loop {
            let buf = self.src.fill().map_err(Error::Io)?;
            let Some(i) = buf.iter().position(|&b| !is_ws(b)) else {
                if buf.is_empty() {
                    return Ok(None);
                }
                let n = buf.len();
                self.src.consume(n);
                continue;
            };
            let b = buf[i];
            self.src.consume(i);
            return Ok(Some(b));
        }
    }

    /// Updates the state after a complete value.
    #[inline]
    pub(crate) fn value_done(&mut self) {
        self.state = match self.stack.top() {
            None => State::TopDone,
            Some(true) => State::ObjComma,
            Some(false) => State::ArrComma,
        };
    }

    /// Returns the kind of the next token without consuming it.
    ///
    /// Whitespace and separators (`,` and `:`) before it are consumed, so afterwards
    /// [`offset`](Parser::offset) is the exact position of the token. Calling `peek`
    /// again returns the same token. If a string was partially read with a
    /// [`StrReader`], the rest of it is skipped first.
    pub fn peek(&mut self) -> Res<Token, S> {
        if let State::InStr { .. } = self.state {
            self.finish_str()?;
        }
        loop {
            let Some(b) = self.skip_ws()? else {
                return if self.state == State::TopDone { Ok(Token::Eof) } else { self.fail(ErrorKind::UnexpectedEof) };
            };
            let tok = match self.state {
                State::TopValue | State::ArrValue | State::ObjValue => value_token(b),
                State::ArrFirst if b == b']' => Some(Token::EndArray),
                State::ArrFirst => value_token(b),
                State::ArrComma => match b {
                    b',' => {
                        self.src.consume(1);
                        self.state = State::ArrValue;
                        continue;
                    }
                    b']' => Some(Token::EndArray),
                    _ => None,
                },
                State::ObjFirst => match b {
                    b'"' => Some(Token::Key),
                    b'}' => Some(Token::EndObject),
                    _ => None,
                },
                State::ObjKey => (b == b'"').then_some(Token::Key),
                State::ObjComma => match b {
                    b',' => {
                        self.src.consume(1);
                        self.state = State::ObjKey;
                        continue;
                    }
                    b'}' => Some(Token::EndObject),
                    _ => None,
                },
                State::ObjColon => {
                    if b == b':' {
                        self.src.consume(1);
                        self.state = State::ObjValue;
                        continue;
                    }
                    None
                }
                State::TopDone => {
                    if self.multi {
                        self.state = State::TopValue;
                        continue;
                    }
                    return self.fail(ErrorKind::TrailingData);
                }
                State::InStr { .. } => None,
            };
            return match tok {
                Some(t) => Ok(t),
                None => self.fail(ErrorKind::UnexpectedByte(b)),
            };
        }
    }

    fn expect(&mut self, want: Token) -> Res<(), S> {
        let t = self.peek()?;
        if t == want { Ok(()) } else { self.mismatch(want, t) }
    }

    /// Whether the parser has a value pending as the value of an object member.
    #[inline]
    fn member_value_pending(&self) -> bool {
        self.state == State::ObjValue
    }

    /// Consumes `{` and enters the object.
    pub fn begin_object(&mut self) -> Res<(), S> {
        self.expect(Token::BeginObject)?;
        self.enter(true)
    }

    /// Consumes `[` and enters the array.
    pub fn begin_array(&mut self) -> Res<(), S> {
        self.expect(Token::BeginArray)?;
        self.enter(false)
    }

    fn enter(&mut self, object: bool) -> Res<(), S> {
        if !self.stack.push(object) {
            return self.fail(ErrorKind::DepthLimitExceeded);
        }
        self.src.consume(1);
        self.state = if object { State::ObjFirst } else { State::ArrFirst };
        Ok(())
    }

    fn leave(&mut self) {
        self.src.consume(1);
        self.stack.pop();
        self.value_done();
    }

    /// Skips the remaining members of the current object and consumes its `}`.
    pub fn end_object(&mut self) -> Res<(), S> {
        loop {
            match self.peek()? {
                Token::EndObject => {
                    self.leave();
                    return Ok(());
                }
                Token::Key => self.skip_key()?,
                t if t.is_value() && self.member_value_pending() => self.skip_value()?,
                t => return self.mismatch(Token::EndObject, t),
            }
        }
    }

    /// Skips the remaining elements of the current array and consumes its `]`.
    pub fn end_array(&mut self) -> Res<(), S> {
        loop {
            match self.peek()? {
                Token::EndArray => {
                    self.leave();
                    return Ok(());
                }
                t if t.is_value() && self.stack.top() == Some(false) => self.skip_value()?,
                t => return self.mismatch(Token::EndArray, t),
            }
        }
    }

    /// Leaves containers (skipping their remaining content) until [`depth`](Parser::depth)
    /// is `depth`.
    pub fn unwind(&mut self, depth: usize) -> Res<(), S> {
        while self.stack.depth() > depth {
            match self.stack.top() {
                Some(true) => self.end_object()?,
                _ => self.end_array()?,
            }
        }
        Ok(())
    }

    /// Whether the current object or array has another member/element (or, at top level,
    /// whether there is another value). An unread member value is skipped first.
    pub fn has_next(&mut self) -> Res<bool, S> {
        loop {
            match self.peek()? {
                Token::EndObject | Token::EndArray | Token::Eof => return Ok(false),
                Token::Key => return Ok(true),
                _ if self.member_value_pending() => self.skip_value()?,
                _ => return Ok(true),
            }
        }
    }

    /// Consumes the opening quote of the next key (skipping an unread member value first).
    pub(crate) fn open_key(&mut self) -> Res<(), S> {
        loop {
            match self.peek()? {
                Token::Key => {
                    self.src.consume(1);
                    self.state = State::InStr { key: true };
                    return Ok(());
                }
                t if t.is_value() && self.member_value_pending() => self.skip_value()?,
                t => return self.mismatch(Token::Key, t),
            }
        }
    }

    /// Reads the next object key into `buf`.
    ///
    /// If the previous member's value was not read, it is skipped. Fails with
    /// [`ErrorKind::BufferTooSmall`] if the key does not fit (the rest of the key is then
    /// skipped by the next operation).
    pub fn read_key<'b>(&mut self, buf: &'b mut [u8]) -> Res<&'b str, S> {
        self.open_key()?;
        self.read_open_str(buf)
    }

    /// Reads the next object key and compares it with `name`, without any buffer.
    pub fn match_key(&mut self, name: &str) -> Res<bool, S> {
        self.match_seg_key(&Seg::Key(name))
    }

    /// Skips the next object key.
    pub fn skip_key(&mut self) -> Res<(), S> {
        self.open_key()?;
        self.finish_str()
    }

    /// Advances inside the current object to the member named `name` and positions the
    /// parser at its value. Returns `false` (positioned at the closing `}`) if there is no
    /// such member among the remaining ones.
    pub fn find_key(&mut self, name: &str) -> Res<bool, S> {
        self.find_member(&Seg::Key(name))
    }

    pub(crate) fn match_seg_key(&mut self, seg: &Seg<'_>) -> Res<bool, S> {
        let mut digits = [0u8; 20];
        let (pat, escaped) = match *seg {
            Seg::Key(k) => (k.as_bytes(), false),
            Seg::Pointer(p) => (p.as_bytes(), true),
            Seg::Index(i) => (fmt_usize(i, &mut digits), false),
        };
        self.open_key()?;
        let mut m = KeyMatcher::new(pat, escaped);
        self.decode(&mut m)?;
        self.state = State::ObjColon;
        Ok(m.matched())
    }

    pub(crate) fn find_member(&mut self, seg: &Seg<'_>) -> Res<bool, S> {
        loop {
            match self.peek()? {
                Token::Key => {
                    if self.match_seg_key(seg)? {
                        self.peek()?;
                        return Ok(true);
                    }
                }
                Token::EndObject => return Ok(false),
                t if t.is_value() && self.member_value_pending() => self.skip_value()?,
                t => return self.mismatch(Token::Key, t),
            }
        }
    }

    /// Inside an array, skips to element `idx` (counting from the current position).
    pub(crate) fn nth_element(&mut self, idx: usize) -> Res<bool, S> {
        let mut i = 0;
        loop {
            match self.peek()? {
                Token::EndArray => return Ok(false),
                t if t.is_value() => {
                    if i == idx {
                        return Ok(true);
                    }
                    self.skip_value()?;
                    i += 1;
                }
                t => return self.mismatch(Token::EndArray, t),
            }
        }
    }

    /// Navigates from the next value to the value at `path`, and stops right before it:
    /// afterwards [`offset`](Parser::offset) is the position of its first byte and it can
    /// be read, skipped, measured or navigated further.
    ///
    /// Paths can be given as `&["foo", "bar"]`, as a JSON Pointer (`"/foo/bar/0"`), or as
    /// segments (`&[Seg::Key("foo"), Seg::Index(0)]`, see [`path!`](crate::path!)).
    /// On a stream, only data after the current position can be found.
    ///
    /// Returns `Ok(false)` if the path does not exist. The parser is then left where the
    /// search stopped (possibly inside containers, see [`depth`](Parser::depth) and
    /// [`unwind`](Parser::unwind)).
    pub fn seek<P: Path + ?Sized>(&mut self, path: &P) -> Res<bool, S> {
        self.seek_segs(path.segments())
    }

    pub(crate) fn seek_segs<'p>(&mut self, segs: impl Iterator<Item = Seg<'p>>) -> Res<bool, S> {
        for seg in segs {
            match self.peek()? {
                Token::BeginObject => {
                    self.begin_object()?;
                    if !self.find_member(&seg)? {
                        return Ok(false);
                    }
                }
                Token::BeginArray => {
                    let Some(i) = seg.index() else { return Ok(false) };
                    self.begin_array()?;
                    if !self.nth_element(i)? {
                        return Ok(false);
                    }
                }
                t if t.is_value() => return Ok(false),
                t => return self.fail(ErrorKind::UnexpectedToken(t)),
            }
        }
        match self.peek()? {
            t if t.is_value() => Ok(true),
            t => self.fail(ErrorKind::UnexpectedToken(t)),
        }
    }

    /// Reads a string value into `buf` and returns it.
    ///
    /// Fails with [`ErrorKind::BufferTooSmall`] if it does not fit; the parser is still
    /// usable and the rest of the string is skipped by the next operation. Use
    /// [`str_reader`](Parser::str_reader) to process long strings in chunks.
    pub fn read_str<'b>(&mut self, buf: &'b mut [u8]) -> Res<&'b str, S> {
        self.expect(Token::String)?;
        self.src.consume(1);
        self.state = State::InStr { key: false };
        self.read_open_str(buf)
    }

    fn read_open_str<'b>(&mut self, buf: &'b mut [u8]) -> Res<&'b str, S> {
        let key = matches!(self.state, State::InStr { key: true });
        let mut sink = BufSink::new(&mut *buf);
        if !self.decode(&mut sink)? {
            return self.fail(ErrorKind::BufferTooSmall);
        }
        let len = sink.len;
        self.str_done(key);
        let buf: &'b [u8] = buf;
        match core::str::from_utf8(&buf[..len]) {
            Ok(s) => Ok(s),
            Err(_) => self.fail(ErrorKind::InvalidUnicode),
        }
    }

    /// Starts reading the next string value (or object key) in chunks.
    pub fn str_reader(&mut self) -> Res<StrReader<'_, S, N>, S> {
        match self.peek()? {
            Token::String => self.state = State::InStr { key: false },
            Token::Key => self.state = State::InStr { key: true },
            t => return self.mismatch(Token::String, t),
        }
        self.src.consume(1);
        Ok(StrReader { parser: self, done: false })
    }

    pub(crate) fn str_done(&mut self, key: bool) {
        if key {
            self.state = State::ObjColon;
        } else {
            self.value_done();
        }
    }

    pub(crate) fn finish_str(&mut self) -> Res<(), S> {
        let State::InStr { key } = self.state else { return Ok(()) };
        self.decode(&mut NullSink)?;
        self.str_done(key);
        Ok(())
    }

    /// Reads a number as its JSON text into `buf`.
    pub fn read_number_str<'b>(&mut self, buf: &'b mut [u8]) -> Res<&'b str, S> {
        self.expect(Token::Number)?;
        let start = self.offset();
        let mut sink = BufSink::new(&mut *buf);
        self.scan_number(&mut sink)?;
        let (len, overflow) = (sink.len, sink.overflow);
        self.value_done();
        if overflow {
            return Err(Error::Json { kind: ErrorKind::BufferTooSmall, offset: start });
        }
        let buf: &'b [u8] = buf;
        match core::str::from_utf8(&buf[..len]) {
            Ok(s) => Ok(s),
            Err(_) => self.fail(ErrorKind::InvalidNumber),
        }
    }

    /// Reads a number and converts it with [`FromStr`] (any integer or float type).
    ///
    /// Integer types fail with [`ErrorKind::NumberOutOfRange`] if the number is out of
    /// range or has a fraction or exponent.
    ///
    /// ```
    /// let mut p = emjson::Parser::from_slice(b"[42, -1.5e3]");
    /// p.begin_array()?;
    /// assert_eq!(p.read_num::<u8>()?, 42);
    /// assert_eq!(p.read_num::<f32>()?, -1500.0);
    /// # Ok::<(), emjson::Error<core::convert::Infallible>>(())
    /// ```
    pub fn read_num<T: FromStr>(&mut self) -> Res<T, S> {
        let mut tmp = [0u8; 64];
        let start = {
            self.expect(Token::Number)?;
            self.offset()
        };
        let s = match self.read_number_str(&mut tmp) {
            Err(Error::Json { kind: ErrorKind::BufferTooSmall, offset }) => {
                return Err(Error::Json { kind: ErrorKind::NumberTooLong, offset });
            }
            r => r?,
        };
        s.parse().map_err(|_| Error::Json { kind: ErrorKind::NumberOutOfRange, offset: start })
    }

    /// Reads `true` or `false`.
    pub fn read_bool(&mut self) -> Res<bool, S> {
        self.expect(Token::Bool)?;
        Ok(self.literal()? == Some(true))
    }

    /// Reads `null`.
    pub fn read_null(&mut self) -> Res<(), S> {
        self.expect(Token::Null)?;
        self.literal()?;
        Ok(())
    }

    /// Consumes `true`, `false` or `null` (the parser is positioned on its first byte).
    fn literal(&mut self) -> Res<Option<bool>, S> {
        let (lit, v): (&[u8], _) = match self.peek_byte()? {
            Some(b't') => (b"true", Some(true)),
            Some(b'f') => (b"false", Some(false)),
            _ => (b"null", None),
        };
        for &c in lit {
            match self.peek_byte()? {
                Some(b) if b == c => self.src.consume(1),
                Some(b) => return self.fail(ErrorKind::UnexpectedByte(b)),
                None => return self.fail(ErrorKind::UnexpectedEof),
            }
        }
        self.value_done();
        Ok(v)
    }

    /// Skips the next value (validating it). If a string is being read with a
    /// [`StrReader`], skips the rest of that string instead.
    pub fn skip_value(&mut self) -> Res<(), S> {
        if let State::InStr { .. } = self.state {
            return self.finish_str();
        }
        let base = self.stack.depth();
        let mut t = self.peek()?;
        if !t.is_value() {
            return self.fail(ErrorKind::UnexpectedToken(t));
        }
        loop {
            match t {
                Token::BeginObject => self.enter(true)?,
                Token::BeginArray => self.enter(false)?,
                Token::EndObject | Token::EndArray => self.leave(),
                Token::Key => {
                    self.src.consume(1);
                    self.decode(&mut NullSink)?;
                    self.state = State::ObjColon;
                }
                Token::String => {
                    self.src.consume(1);
                    self.decode(&mut NullSink)?;
                    self.value_done();
                }
                Token::Number => {
                    self.scan_number(&mut NullSink)?;
                    self.value_done();
                }
                Token::Bool | Token::Null => {
                    self.literal()?;
                }
                Token::Eof => return self.fail(ErrorKind::UnexpectedEof),
            }
            if self.stack.depth() == base {
                return Ok(());
            }
            t = self.peek()?;
        }
    }

    /// Skips the next value and returns its position: `start` is the offset of its first
    /// byte and `end` the offset right after its last byte.
    ///
    /// `end - start` is the exact encoded length of the value, which is what is needed to
    /// replace it in place (see [`edit`](crate::edit)).
    pub fn value_span(&mut self) -> Res<Span, S> {
        let t = self.peek()?;
        if !t.is_value() {
            return self.fail(ErrorKind::UnexpectedToken(t));
        }
        let start = self.offset();
        self.skip_value()?;
        Ok(Span { start, end: self.offset() })
    }

    /// Reads the next token as an [`Event`]. Keys, strings and numbers are copied into
    /// `buf` (fails with [`ErrorKind::BufferTooSmall`] if they do not fit).
    pub fn next_event<'b>(&mut self, buf: &'b mut [u8]) -> Res<Event<'b>, S> {
        Ok(match self.peek()? {
            Token::BeginObject => {
                self.enter(true)?;
                Event::BeginObject
            }
            Token::BeginArray => {
                self.enter(false)?;
                Event::BeginArray
            }
            Token::EndObject => {
                self.leave();
                Event::EndObject
            }
            Token::EndArray => {
                self.leave();
                Event::EndArray
            }
            Token::Key => Event::Key(self.read_key(buf)?),
            Token::String => Event::String(self.read_str(buf)?),
            Token::Number => Event::Number(self.read_number_str(buf)?),
            Token::Bool => Event::Bool(self.literal()? == Some(true)),
            Token::Null => {
                self.literal()?;
                Event::Null
            }
            Token::Eof => Event::Eof,
        })
    }

    /// Checks that nothing but whitespace follows the top-level value.
    pub fn finish(&mut self) -> Res<(), S> {
        match self.peek()? {
            Token::Eof => Ok(()),
            _ if self.stack.depth() == 0 => self.fail(ErrorKind::TrailingData),
            t => self.fail(ErrorKind::UnexpectedToken(t)),
        }
    }
}

impl<'a, const N: usize> Parser<SliceSource<'a>, N> {
    /// Skips the next value and returns its raw JSON text (zero copy).
    pub fn raw_value(&mut self) -> Result<&'a [u8], Error<core::convert::Infallible>> {
        let span = self.value_span()?;
        Ok(&self.src.data()[span.start as usize..span.end as usize])
    }

    /// Reads a string value, borrowing it from the input when it contains no escape
    /// sequence, and decoding it into `scratch` otherwise.
    pub fn read_str_ref<'b>(&mut self, scratch: &'b mut [u8]) -> Result<&'b str, Error<core::convert::Infallible>>
    where
        'a: 'b,
    {
        self.expect(Token::String)?;
        let rest = self.src.remaining();
        let body = &rest[1..];
        if let Some(i) = body.iter().position(|&b| b == b'"' || b == b'\\' || b < 0x20)
            && body[i] == b'"'
            && let Ok(s) = core::str::from_utf8(&body[..i])
        {
            self.src.consume(i + 2);
            self.value_done();
            return Ok(s);
        }
        self.read_str(scratch)
    }
}

/// Reads a string value or key in chunks, for strings larger than any available buffer.
///
/// Obtained from [`Parser::str_reader`]. Dropping it before the end is fine: the parser
/// skips the rest of the string on its next operation.
///
/// ```
/// let mut p = emjson::Parser::from_slice(br#""a long string \u00e9""#);
/// let mut r = p.str_reader()?;
/// let mut buf = [0u8; 4];
/// let mut len = 0;
/// while let Some(chunk) = r.next_chunk(&mut buf)? {
///     len += chunk.len(); // each chunk is valid UTF-8
/// }
/// assert_eq!(len, 16);
/// # Ok::<(), emjson::Error<core::convert::Infallible>>(())
/// ```
pub struct StrReader<'p, S: Source, const N: usize = 8> {
    parser: &'p mut Parser<S, N>,
    done: bool,
}

impl<S: Source, const N: usize> fmt::Debug for StrReader<'_, S, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StrReader").field("offset", &self.parser.offset()).field("done", &self.done).finish()
    }
}

impl<S: Source, const N: usize> StrReader<'_, S, N> {
    /// Decodes the next chunk into `buf`, returning `None` at the end of the string.
    ///
    /// Chunks never split a character; `buf` must have room for at least 4 bytes to be
    /// guaranteed to make progress.
    pub fn next_chunk<'b>(&mut self, buf: &'b mut [u8]) -> Res<Option<&'b str>, S> {
        if self.done {
            return Ok(None);
        }
        let State::InStr { key } = self.parser.state else {
            return self.parser.fail(ErrorKind::InvalidState);
        };
        let mut sink = BufSink::new(&mut *buf);
        let finished = self.parser.decode(&mut sink)?;
        let len = sink.len;
        if finished {
            self.done = true;
            self.parser.str_done(key);
        }
        if len == 0 {
            return if finished { Ok(None) } else { self.parser.fail(ErrorKind::BufferTooSmall) };
        }
        let buf: &'b [u8] = buf;
        match core::str::from_utf8(&buf[..len]) {
            Ok(s) => Ok(Some(s)),
            Err(_) => self.parser.fail(ErrorKind::InvalidUnicode),
        }
    }

    /// Whether the end of the string was reached.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Skips the rest of the string.
    pub fn skip_rest(self) -> Res<(), S> {
        if self.done { Ok(()) } else { self.parser.finish_str() }
    }
}
