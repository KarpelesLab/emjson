//! # emjson — embedded JSON
//!
//! Streaming JSON parsing, writing and editing for memory-constrained systems:
//! `no_std`, no allocator, no `unsafe`, and a memory footprint of a few hundred bytes
//! regardless of document size. Edit a multi-megabyte JSON file stored in flash with
//! a 256-byte buffer.
//!
//! ## Parsing
//!
//! [`Parser`] is a pull parser (a cursor) over any [`Source`]: a slice
//! ([`Parser::from_slice`]), a stream ([`io::ReadSource`]), or random-access
//! [`edit::Storage`]. Values are lazy: you [`peek`](Parser::peek) at the next one and
//! decide to read it, skip it, measure it, or descend into it.
//!
//! ```
//! use emjson::{Parser, Token};
//! use emjson::io::ReadSource;
//!
//! let stream: &[u8] = br#"{"foo": {"bar": "hello world"}}"#; // any `emjson::io::Read`
//! let mut buf = [0u8; 16];
//! let mut p = Parser::new(ReadSource::new(stream, &mut buf));
//!
//! // Stops right at the opening quote of "hello world"...
//! assert!(p.seek(&["foo", "bar"])?);
//! assert_eq!(p.peek()?, Token::String);
//! assert_eq!(p.offset(), 16);
//! // ...so the caller decides what to do with the value.
//! let mut s = [0u8; 32];
//! assert_eq!(p.read_str(&mut s)?, "hello world");
//! # Ok::<(), emjson::Error<core::convert::Infallible>>(())
//! ```
//!
//! Other ways to consume a document:
//! - [`Parser::walk`]: callbacks for every value, with its JSON Pointer path.
//! - [`Parser::next_event`]: classic pull events.
//! - Cursor methods: [`begin_object`](Parser::begin_object),
//!   [`find_key`](Parser::find_key), [`read_key`](Parser::read_key),
//!   [`has_next`](Parser::has_next), [`read_num`](Parser::read_num),
//!   [`str_reader`](Parser::str_reader) for strings larger than any buffer, ...
//! - [`Parser::skip_value`], [`Parser::value_span`] (exact byte range of a value),
//!   [`validate`], [`copy_value`] (minify, pretty-print, extract).
//!
//! ## Writing
//!
//! [`JsonWriter`] encodes to any [`io::Write`]; types implement [`ToJson`].
//! [`encoded_len`] measures an encoding without producing it.
//!
//! ## Editing
//!
//! The [`edit`] module replaces, sets, inserts, appends and removes values:
//! in place in any [`edit::Storage`] ([`edit::Editor`]: locate the value, measure the new
//! encoding, move the tail of the document once, write the new value), or while copying
//! a stream ([`edit::copy_edit`]).
#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs, missing_debug_implementations)]

#[cfg(any(feature = "std", test))]
extern crate std;

pub mod edit;
mod error;
pub mod io;
mod number;
mod parser;
pub mod path;
mod stack;
mod string;
mod util;
mod walk;
pub mod writer;

pub use error::{Error, ErrorKind};
pub use io::{SliceSource, Source};
pub use parser::{Event, Parser, StrReader, Token};
pub use path::{Path, Seg};
pub use util::{copy_value, validate};
pub use walk::{Flow, Node};
pub use writer::{JsonWriter, RawJson, ToJson, encoded_len, to_slice};

/// A byte range `start..end` in a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Span {
    /// Offset of the first byte.
    pub start: u64,
    /// Offset after the last byte.
    pub end: u64,
}

impl Span {
    /// Length in bytes.
    pub fn len(&self) -> u64 {
        self.end - self.start
    }

    /// Whether the span is empty.
    pub fn is_empty(&self) -> bool {
        self.end == self.start
    }

    /// The span as a `usize` range, for slicing (`None` if it does not fit).
    pub fn range(&self) -> Option<core::ops::Range<usize>> {
        Some(usize::try_from(self.start).ok()?..usize::try_from(self.end).ok()?)
    }
}

#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
