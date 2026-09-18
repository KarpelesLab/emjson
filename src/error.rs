use core::fmt;

use crate::Token;

/// What went wrong while processing JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The input ended in the middle of a JSON value.
    UnexpectedEof,
    /// A byte that is not valid at this position.
    UnexpectedByte(u8),
    /// Non-whitespace data follows the top-level value.
    TrailingData,
    /// An invalid escape sequence in a string.
    InvalidEscape,
    /// Invalid UTF-8, or a `\u` escape encoding an unpaired surrogate.
    InvalidUnicode,
    /// An unescaped control character (below U+0020) in a string.
    ControlCharacter,
    /// A malformed number.
    InvalidNumber,
    /// The document nests deeper than the parser's stack allows.
    DepthLimitExceeded,
    /// A caller-provided buffer is too small for the requested data.
    BufferTooSmall,
    /// A number is too long for the internal conversion buffer (64 bytes).
    NumberTooLong,
    /// A number cannot be represented in the requested type (out of range,
    /// or has a fraction/exponent when an integer was requested).
    NumberOutOfRange,
    /// A value of a different type was found.
    TypeMismatch {
        /// What the caller asked for.
        expected: Token,
        /// What the document contains.
        found: Token,
    },
    /// A value was expected, but something else was found.
    UnexpectedToken(Token),
    /// The operation is not valid in the current state.
    InvalidState,
    /// The path buffer given to [`Parser::walk`](crate::Parser::walk) is too small.
    PathTooLong,
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEof => f.write_str("unexpected end of input"),
            Self::UnexpectedByte(b) if b.is_ascii_graphic() => {
                write!(f, "unexpected character '{}'", *b as char)
            }
            Self::UnexpectedByte(b) => write!(f, "unexpected byte 0x{b:02x}"),
            Self::TrailingData => f.write_str("trailing data after JSON value"),
            Self::InvalidEscape => f.write_str("invalid escape sequence"),
            Self::InvalidUnicode => f.write_str("invalid unicode"),
            Self::ControlCharacter => f.write_str("control character in string"),
            Self::InvalidNumber => f.write_str("invalid number"),
            Self::DepthLimitExceeded => f.write_str("nesting depth limit exceeded"),
            Self::BufferTooSmall => f.write_str("buffer too small"),
            Self::NumberTooLong => f.write_str("number too long"),
            Self::NumberOutOfRange => f.write_str("number out of range for target type"),
            Self::TypeMismatch { expected, found } => write!(f, "expected {expected}, found {found}"),
            Self::UnexpectedToken(t) => write!(f, "expected a value, found {t}"),
            Self::InvalidState => f.write_str("operation not valid in current state"),
            Self::PathTooLong => f.write_str("path buffer too small"),
        }
    }
}

/// Error type of all fallible operations: either an I/O error from the
/// underlying source/sink/storage, or a JSON error at a given byte offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error<E> {
    /// Error reported by the underlying I/O.
    Io(E),
    /// Malformed JSON, or API misuse.
    Json {
        /// What went wrong.
        kind: ErrorKind,
        /// Byte offset in the input where it went wrong.
        offset: u64,
    },
}

impl<E> Error<E> {
    /// The JSON error kind, or `None` for I/O errors.
    pub fn kind(&self) -> Option<ErrorKind> {
        match self {
            Self::Json { kind, .. } => Some(*kind),
            Self::Io(_) => None,
        }
    }

    /// The byte offset of a JSON error, or `None` for I/O errors.
    pub fn offset(&self) -> Option<u64> {
        match self {
            Self::Json { offset, .. } => Some(*offset),
            Self::Io(_) => None,
        }
    }

    /// Converts the I/O error type.
    pub fn map_io<F>(self, f: impl FnOnce(E) -> F) -> Error<F> {
        match self {
            Self::Io(e) => Error::Io(f(e)),
            Self::Json { kind, offset } => Error::Json { kind, offset },
        }
    }
}

impl<E: fmt::Display> fmt::Display for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Json { kind, offset } => write!(f, "{kind} at offset {offset}"),
        }
    }
}

impl<E: core::error::Error + 'static> core::error::Error for Error<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Json { .. } => None,
        }
    }
}
