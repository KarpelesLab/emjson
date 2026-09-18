//! Whole-value helpers built on the parser and the writer.

use crate::Token;

/// Result of an operation reading from `S` and writing to `W`.
pub(crate) type PipeResult<T, S, W> = Result<T, Error<PipeError<<S as Source>::Error, <W as Write>::Error>>>;
use crate::error::{Error, ErrorKind};
use crate::io::{PipeError, Source, Write};
use crate::parser::Parser;
use crate::string::Sink;
use crate::writer::JsonWriter;

/// Checks that `src` contains exactly one well-formed JSON value (default nesting limit
/// of 64 levels), using no memory besides the parser.
pub fn validate<S: Source>(src: S) -> Result<(), Error<S::Error>> {
    let mut p = Parser::new(src);
    p.skip_value()?;
    p.finish()
}

/// Passes number text to a writer, remembering the first error.
struct NumberSink<'a, W: Write> {
    w: &'a mut JsonWriter<W>,
    err: Option<W::Error>,
}

impl<W: Write> Sink for NumberSink<'_, W> {
    fn room(&self) -> usize {
        usize::MAX
    }
    fn put(&mut self, bytes: &[u8]) {
        if self.err.is_none()
            && let Err(e) = self.w.raw_fragment(bytes)
        {
            self.err = Some(e);
        }
    }
}

/// Reads the next value from `p` and writes it to `w`, in constant memory (strings are
/// transferred in small chunks).
///
/// With a compact writer this minifies, with [`JsonWriter::pretty`] it pretty-prints. After
/// [`Parser::seek`], it extracts a sub-document.
pub fn copy_value<S: Source, W: Write, const N: usize>(
    p: &mut Parser<S, N>,
    w: &mut JsonWriter<W>,
) -> PipeResult<(), S, W> {
    let rd = |e: Error<S::Error>| e.map_io(PipeError::Read);
    let wr = |e: W::Error| Error::Io(PipeError::Write(e));
    let base = p.depth();
    let first = p.peek().map_err(rd)?;
    if !first.is_value() {
        return Err(Error::Json { kind: ErrorKind::UnexpectedToken(first), offset: p.offset() });
    }
    let mut buf = [0u8; 32];
    loop {
        match p.peek().map_err(rd)? {
            Token::BeginObject => {
                p.begin_object().map_err(rd)?;
                w.begin_object().map_err(wr)?;
            }
            Token::BeginArray => {
                p.begin_array().map_err(rd)?;
                w.begin_array().map_err(wr)?;
            }
            Token::EndObject => {
                p.end_object().map_err(rd)?;
                w.end_object().map_err(wr)?;
            }
            Token::EndArray => {
                p.end_array().map_err(rd)?;
                w.end_array().map_err(wr)?;
            }
            t @ (Token::Key | Token::String) => {
                w.begin_string().map_err(wr)?;
                let mut r = p.str_reader().map_err(rd)?;
                while let Some(chunk) = r.next_chunk(&mut buf).map_err(rd)? {
                    w.string_fragment(chunk).map_err(wr)?;
                }
                if t == Token::Key { w.end_key() } else { w.end_string() }.map_err(wr)?;
            }
            Token::Number => {
                w.begin_raw().map_err(wr)?;
                let mut sink = NumberSink { w, err: None };
                p.scan_number(&mut sink).map_err(rd)?;
                if let Some(e) = sink.err {
                    return Err(wr(e));
                }
                p.value_done();
                w.end_raw();
            }
            Token::Bool => {
                let b = p.read_bool().map_err(rd)?;
                w.bool(b).map_err(wr)?;
            }
            Token::Null => {
                p.read_null().map_err(rd)?;
                w.null().map_err(wr)?;
            }
            Token::Eof => return Err(Error::Json { kind: ErrorKind::UnexpectedEof, offset: p.offset() }),
        }
        if p.depth() == base {
            return Ok(());
        }
    }
}
