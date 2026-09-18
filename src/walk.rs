//! Callback-driven traversal with JSON Pointer paths.

use core::str::FromStr;

use crate::error::ErrorKind;
use crate::io::Source;
use crate::parser::{Parser, Res, State, StrReader};
use crate::string::Sink;
use crate::{Span, Token};

/// What [`Parser::walk`] should do after a callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Keep going (enter objects and arrays).
    Continue,
    /// Do not enter this object or array (it is skipped, without further callbacks).
    Skip,
    /// Stop the walk; the parser stays where it is.
    Stop,
}

/// A value (or the end of an object or array) visited by [`Parser::walk`].
///
/// For scalar values, the callback may read the value through the node. Values that are
/// not read are skipped automatically.
pub struct Node<'a, S: Source, const N: usize> {
    parser: &'a mut Parser<S, N>,
    path: &'a str,
    token: Token,
    offset: u64,
}

impl<S: Source, const N: usize> core::fmt::Debug for Node<'_, S, N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Node")
            .field("path", &self.path)
            .field("token", &self.token)
            .field("offset", &self.offset)
            .finish()
    }
}

impl<'a, S: Source, const N: usize> Node<'a, S, N> {
    /// JSON Pointer to this value, relative to where the walk started (`""` for the value
    /// the walk started on).
    pub fn path(&self) -> &'a str {
        self.path
    }

    /// Kind of value; [`Token::EndObject`] or [`Token::EndArray`] after the last member or
    /// element of a container.
    pub fn token(&self) -> Token {
        self.token
    }

    /// Byte offset of the value's first byte (or of the closing bracket).
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Nesting depth of the value, relative to the parser's top level.
    pub fn depth(&self) -> usize {
        match self.token {
            Token::EndObject | Token::EndArray => self.parser.depth() + 1,
            _ => self.parser.depth(),
        }
    }

    fn parser(&mut self) -> Res<&mut Parser<S, N>, S> {
        if !self.token.is_value() || self.parser.offset() != self.offset {
            return self.parser.fail(ErrorKind::InvalidState);
        }
        Ok(self.parser)
    }

    /// Reads a string value into `buf`.
    pub fn read_str<'b>(&mut self, buf: &'b mut [u8]) -> Res<&'b str, S> {
        self.parser()?.read_str(buf)
    }

    /// Reads a string value in chunks.
    pub fn str_reader(&mut self) -> Res<StrReader<'_, S, N>, S> {
        self.parser()?.str_reader()
    }

    /// Reads a number (see [`Parser::read_num`]).
    pub fn read_num<T: FromStr>(&mut self) -> Res<T, S> {
        self.parser()?.read_num()
    }

    /// Reads a number as JSON text.
    pub fn read_number_str<'b>(&mut self, buf: &'b mut [u8]) -> Res<&'b str, S> {
        self.parser()?.read_number_str(buf)
    }

    /// Reads a boolean.
    pub fn read_bool(&mut self) -> Res<bool, S> {
        self.parser()?.read_bool()
    }

    /// Skips the value and returns its span (works for objects and arrays too, which are
    /// then not entered).
    pub fn span(&mut self) -> Res<Span, S> {
        self.parser()?.value_span()
    }

    /// Gives full access to the parser to consume this value in any way (for example
    /// [`copy_value`](crate::copy_value)). If the value is only partially consumed, the
    /// walk skips the rest of it. Consuming more than this value confuses the walk.
    pub fn take_parser(&mut self) -> Res<&mut Parser<S, N>, S> {
        self.parser()
    }
}

/// Builds the path by appending escaped key bytes.
struct PathSink<'b> {
    buf: &'b mut [u8],
    len: usize,
    overflow: bool,
}

impl PathSink<'_> {
    fn push(&mut self, bytes: &[u8]) {
        match self.buf.get_mut(self.len..self.len + bytes.len()) {
            Some(dst) => {
                dst.copy_from_slice(bytes);
                self.len += bytes.len();
            }
            None => self.overflow = true,
        }
    }
}

impl Sink for PathSink<'_> {
    fn room(&self) -> usize {
        usize::MAX
    }
    fn put(&mut self, bytes: &[u8]) {
        let mut start = 0;
        for (i, &b) in bytes.iter().enumerate() {
            let esc: &[u8] = match b {
                b'~' => b"~0",
                b'/' => b"~1",
                _ => continue,
            };
            self.push(&bytes[start..i]);
            self.push(esc);
            start = i + 1;
        }
        self.push(&bytes[start..]);
    }
}

fn pop_segment(path: &[u8], len: usize) -> usize {
    path[..len].iter().rposition(|&b| b == b'/').unwrap_or(0)
}

/// Replaces the last segment (an array index) with the next index, or appends `/0`.
fn next_index(path: &mut [u8], len: usize, first: bool) -> Option<usize> {
    let (start, idx) = if first {
        (len, 0)
    } else {
        let slash = pop_segment(path, len);
        let digits = &path[slash + 1..len];
        let v = digits.iter().fold(0usize, |a, &d| a.wrapping_mul(10).wrapping_add((d - b'0') as usize));
        (slash, v + 1)
    };
    let mut tmp = [0u8; 20];
    let digits = crate::number::fmt_usize(idx, &mut tmp);
    let end = start + 1 + digits.len();
    let dst = path.get_mut(start..end)?;
    dst[0] = b'/';
    dst[1..].copy_from_slice(digits);
    Some(end)
}

impl<S: Source, const N: usize> Parser<S, N> {
    /// Walks the next value, calling `f` for every value in it (depth first, in document
    /// order) and at the end of every object and array.
    ///
    /// `path_buf` receives the JSON Pointer of the current value, available as
    /// [`Node::path`]; it must be large enough for the longest path in the document
    /// (otherwise [`ErrorKind::PathTooLong`]). Returns `Ok(false)` if the callback stopped
    /// the walk.
    ///
    /// ```
    /// use emjson::{Flow, Parser, Token};
    ///
    /// let json = br#"{"wifi": {"ssid": "home", "channel": 6}, "debug": true}"#;
    /// let mut p = Parser::from_slice(json);
    /// let (mut path, mut ssid_buf) = ([0u8; 64], [0u8; 32]);
    /// let (mut ssid_len, mut channel) = (0, 0u8);
    /// p.walk(&mut path, |node| {
    ///     match node.path() {
    ///         "/wifi/ssid" => ssid_len = node.read_str(&mut ssid_buf)?.len(),
    ///         "/wifi/channel" => channel = node.read_num()?,
    ///         _ => {}
    ///     }
    ///     Ok(Flow::Continue)
    /// })?;
    /// assert_eq!((&ssid_buf[..ssid_len], channel), (&b"home"[..], 6));
    /// # Ok::<(), emjson::Error<core::convert::Infallible>>(())
    /// ```
    pub fn walk<F>(&mut self, path_buf: &mut [u8], mut f: F) -> Res<bool, S>
    where
        F: FnMut(&mut Node<'_, S, N>) -> Res<Flow, S>,
    {
        let base = self.stack.depth();
        let mut len = 0usize;
        let first = self.peek()?;
        if !first.is_value() {
            return self.fail(ErrorKind::UnexpectedToken(first));
        }
        loop {
            let tok = self.peek()?;
            match tok {
                Token::Key => {
                    self.src.consume(1);
                    self.state = State::InStr { key: true };
                    let Some(slash) = path_buf.get_mut(len) else {
                        return self.fail(ErrorKind::PathTooLong);
                    };
                    *slash = b'/';
                    let mut sink = PathSink { buf: path_buf, len: len + 1, overflow: false };
                    self.decode(&mut sink)?;
                    self.state = State::ObjColon;
                    if sink.overflow {
                        return self.fail(ErrorKind::PathTooLong);
                    }
                    len = sink.len;
                    continue;
                }
                Token::EndObject | Token::EndArray => {
                    if tok == Token::EndArray && self.state == State::ArrComma {
                        len = pop_segment(path_buf, len);
                    }
                    let offset = self.offset();
                    self.src.consume(1);
                    self.stack.pop();
                    self.value_done();
                    let path = core::str::from_utf8(&path_buf[..len]).unwrap_or_default();
                    let mut node = Node { parser: self, path, token: tok, offset };
                    if f(&mut node)? == Flow::Stop {
                        return Ok(false);
                    }
                }
                Token::Eof => return self.fail(ErrorKind::UnexpectedToken(tok)),
                _ => {
                    if self.stack.depth() > base && self.stack.top() == Some(false) {
                        let first = self.state == State::ArrFirst;
                        match next_index(path_buf, len, first) {
                            Some(l) => len = l,
                            None => return self.fail(ErrorKind::PathTooLong),
                        }
                    }
                    let depth = self.stack.depth();
                    let offset = self.offset();
                    let path = core::str::from_utf8(&path_buf[..len]).unwrap_or_default();
                    let mut node = Node { parser: self, path, token: tok, offset };
                    let flow = f(&mut node)?;
                    if flow == Flow::Stop {
                        return Ok(false);
                    }
                    if self.offset() == offset {
                        match (flow, tok) {
                            (Flow::Continue, Token::BeginObject) => {
                                self.begin_object()?;
                                continue;
                            }
                            (Flow::Continue, Token::BeginArray) => {
                                self.begin_array()?;
                                continue;
                            }
                            _ => self.skip_value()?,
                        }
                    } else {
                        // Consumed (maybe partially) by the callback.
                        self.finish_str()?;
                        self.unwind(depth)?;
                    }
                }
            }
            // A value is complete.
            if self.stack.depth() == base {
                return Ok(true);
            }
            if self.stack.top() == Some(true) {
                len = pop_segment(path_buf, len);
            }
        }
    }
}
