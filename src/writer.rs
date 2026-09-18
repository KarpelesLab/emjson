//! JSON encoding: [`JsonWriter`], the [`ToJson`] trait and [`encoded_len`].

use core::convert::Infallible;
use core::fmt;

use crate::io::{BufferFull, Counter, SliceWriter, Write};
use crate::number::{fmt_u32, fmt_u64, fmt_u128, fmt_usize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pos {
    /// Nothing written yet in the current container (or at top level).
    Start,
    /// After a key: the value follows directly.
    AfterKey,
    /// After a value: a separator is needed.
    AfterValue,
}

/// Streaming JSON encoder writing to any [`Write`]r, with no buffering.
///
/// Commas and colons are inserted automatically. The writer does not check that calls
/// are well nested (a key outside an object, for instance, produces invalid JSON).
///
/// ```
/// use emjson::JsonWriter;
/// use emjson::io::SliceWriter;
///
/// let mut buf = [0u8; 64];
/// let mut w = JsonWriter::new(SliceWriter::new(&mut buf));
/// w.begin_object()?;
/// w.member("name", "sensor \"A\"")?;
/// w.key("values")?;
/// w.value(&[1.5f32, -2.0])?;
/// w.member("ok", &true)?;
/// w.end_object()?;
/// assert_eq!(w.get_ref().written(), br#"{"name":"sensor \"A\"","values":[1.5,-2],"ok":true}"#);
/// # Ok::<(), emjson::io::BufferFull>(())
/// ```
#[derive(Debug)]
pub struct JsonWriter<W> {
    out: W,
    pos: Pos,
    depth: u32,
    indent: u8,
}

const SPACES: &[u8; 32] = b"                                ";

impl<W: Write> JsonWriter<W> {
    /// Creates a writer producing compact JSON.
    pub fn new(out: W) -> Self {
        Self { out, pos: Pos::Start, depth: 0, indent: 0 }
    }

    /// Creates a writer producing indented JSON (`indent` spaces per level).
    pub fn pretty(out: W, indent: u8) -> Self {
        Self { out, pos: Pos::Start, depth: 0, indent }
    }

    /// The output.
    pub fn get_ref(&self) -> &W {
        &self.out
    }

    /// The output. Writing to it directly may produce invalid JSON.
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.out
    }

    /// Returns the output.
    pub fn into_inner(self) -> W {
        self.out
    }

    /// Current nesting depth.
    pub fn depth(&self) -> usize {
        self.depth as usize
    }

    fn newline(&mut self) -> Result<(), W::Error> {
        if self.indent == 0 {
            return Ok(());
        }
        self.out.write_all(b"\n")?;
        let mut n = self.depth as usize * self.indent as usize;
        while n > 0 {
            let k = n.min(SPACES.len());
            self.out.write_all(&SPACES[..k])?;
            n -= k;
        }
        Ok(())
    }

    /// Writes what must precede a new key or value.
    fn separator(&mut self) -> Result<(), W::Error> {
        match self.pos {
            Pos::AfterKey => Ok(()),
            Pos::AfterValue if self.depth == 0 => self.out.write_all(b"\n"),
            Pos::AfterValue => {
                self.out.write_all(b",")?;
                self.newline()
            }
            Pos::Start if self.depth > 0 => self.newline(),
            Pos::Start => Ok(()),
        }
    }

    /// Writes `{`.
    pub fn begin_object(&mut self) -> Result<(), W::Error> {
        self.open(b"{")
    }

    /// Writes `[`.
    pub fn begin_array(&mut self) -> Result<(), W::Error> {
        self.open(b"[")
    }

    /// Writes `}`.
    pub fn end_object(&mut self) -> Result<(), W::Error> {
        self.close(b"}")
    }

    /// Writes `]`.
    pub fn end_array(&mut self) -> Result<(), W::Error> {
        self.close(b"]")
    }

    fn open(&mut self, b: &[u8]) -> Result<(), W::Error> {
        self.separator()?;
        self.out.write_all(b)?;
        self.depth += 1;
        self.pos = Pos::Start;
        Ok(())
    }

    fn close(&mut self, b: &[u8]) -> Result<(), W::Error> {
        self.depth = self.depth.saturating_sub(1);
        if self.pos != Pos::Start {
            self.newline()?;
        }
        self.out.write_all(b)?;
        self.pos = Pos::AfterValue;
        Ok(())
    }

    /// Writes an object key (and the following `:`).
    pub fn key(&mut self, key: &str) -> Result<(), W::Error> {
        self.begin_string()?;
        self.string_fragment(key)?;
        self.end_key()
    }

    /// Writes a key and a value.
    pub fn member<T: ToJson + ?Sized>(&mut self, key: &str, value: &T) -> Result<(), W::Error> {
        self.key(key)?;
        value.to_json(self)
    }

    /// Writes a value.
    pub fn value<T: ToJson + ?Sized>(&mut self, value: &T) -> Result<(), W::Error> {
        value.to_json(self)
    }

    /// Writes a string value.
    pub fn string(&mut self, s: &str) -> Result<(), W::Error> {
        self.begin_string()?;
        self.string_fragment(s)?;
        self.end_string()
    }

    /// Starts a string written in pieces with [`string_fragment`](JsonWriter::string_fragment)
    /// and terminated by [`end_string`](JsonWriter::end_string) (for a value) or
    /// [`end_key`](JsonWriter::end_key) (for an object key).
    pub fn begin_string(&mut self) -> Result<(), W::Error> {
        self.separator()?;
        self.out.write_all(b"\"")
    }

    /// Writes part of a string, escaping as needed.
    pub fn string_fragment(&mut self, s: &str) -> Result<(), W::Error> {
        let s = s.as_bytes();
        let mut start = 0;
        for (i, &b) in s.iter().enumerate() {
            let esc: &[u8] = match b {
                b'"' => b"\\\"",
                b'\\' => b"\\\\",
                b'\n' => b"\\n",
                b'\r' => b"\\r",
                b'\t' => b"\\t",
                0x08 => b"\\b",
                0x0c => b"\\f",
                0..=0x1f => b"",
                _ => continue,
            };
            if start < i {
                self.out.write_all(&s[start..i])?;
            }
            if esc.is_empty() {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                self.out.write_all(&[b'\\', b'u', b'0', b'0', HEX[(b >> 4) as usize], HEX[(b & 15) as usize]])?;
            } else {
                self.out.write_all(esc)?;
            }
            start = i + 1;
        }
        if start < s.len() {
            self.out.write_all(&s[start..])?;
        }
        Ok(())
    }

    /// Ends a string started with [`begin_string`](JsonWriter::begin_string) as a value.
    pub fn end_string(&mut self) -> Result<(), W::Error> {
        self.out.write_all(b"\"")?;
        self.pos = Pos::AfterValue;
        Ok(())
    }

    /// Ends a string started with [`begin_string`](JsonWriter::begin_string) as an object
    /// key, and writes the `:`.
    pub fn end_key(&mut self) -> Result<(), W::Error> {
        self.out.write_all(if self.indent > 0 { b"\": " } else { b"\":" })?;
        self.pos = Pos::AfterKey;
        Ok(())
    }

    /// Writes `null`.
    pub fn null(&mut self) -> Result<(), W::Error> {
        self.raw("null")
    }

    /// Writes `true` or `false`.
    pub fn bool(&mut self, v: bool) -> Result<(), W::Error> {
        self.raw(if v { "true" } else { "false" })
    }

    /// Writes an already encoded JSON value as is. The caller is responsible for its
    /// validity.
    pub fn raw(&mut self, json: &str) -> Result<(), W::Error> {
        self.begin_raw()?;
        self.raw_fragment(json.as_bytes())?;
        self.end_raw();
        Ok(())
    }

    pub(crate) fn begin_raw(&mut self) -> Result<(), W::Error> {
        self.separator()
    }

    pub(crate) fn raw_fragment(&mut self, b: &[u8]) -> Result<(), W::Error> {
        self.out.write_all(b)
    }

    pub(crate) fn end_raw(&mut self) {
        self.pos = Pos::AfterValue;
    }

    fn digits(&mut self, neg: bool, digits: &[u8]) -> Result<(), W::Error> {
        self.begin_raw()?;
        if neg {
            self.out.write_all(b"-")?;
        }
        self.out.write_all(digits)?;
        self.end_raw();
        Ok(())
    }

    /// Writes an unsigned integer.
    pub fn u32(&mut self, v: u32) -> Result<(), W::Error> {
        self.digits(false, fmt_u32(v, &mut [0; 10]))
    }

    /// Writes a signed integer.
    pub fn i32(&mut self, v: i32) -> Result<(), W::Error> {
        self.digits(v < 0, fmt_u32(v.unsigned_abs(), &mut [0; 10]))
    }

    /// Writes an unsigned integer.
    pub fn u64(&mut self, v: u64) -> Result<(), W::Error> {
        self.digits(false, fmt_u64(v, &mut [0; 20]))
    }

    /// Writes a signed integer.
    pub fn i64(&mut self, v: i64) -> Result<(), W::Error> {
        self.digits(v < 0, fmt_u64(v.unsigned_abs(), &mut [0; 20]))
    }

    fn usize(&mut self, v: usize, neg: bool) -> Result<(), W::Error> {
        self.digits(neg, fmt_usize(v, &mut [0; 20]))
    }

    fn u128(&mut self, v: u128, neg: bool) -> Result<(), W::Error> {
        self.digits(neg, fmt_u128(v, &mut [0; 40]))
    }

    /// Writes a float in its shortest round-trip form. NaN and infinities, which JSON
    /// cannot represent, are written as `null`.
    pub fn f64(&mut self, v: f64) -> Result<(), W::Error> {
        if !v.is_finite() {
            return self.null();
        }
        let a = v.abs();
        let sci = a != 0.0 && !(1e-5..1e16).contains(&a);
        self.float(format_args!("{v}"), format_args!("{v:e}"), sci)
    }

    /// Writes an `f32` in its shortest round-trip form (NaN and infinities as `null`).
    pub fn f32(&mut self, v: f32) -> Result<(), W::Error> {
        if !v.is_finite() {
            return self.null();
        }
        let a = v.abs();
        let sci = a != 0.0 && !(1e-5..1e16).contains(&a);
        self.float(format_args!("{v}"), format_args!("{v:e}"), sci)
    }

    fn float(&mut self, plain: fmt::Arguments<'_>, sci: fmt::Arguments<'_>, use_sci: bool) -> Result<(), W::Error> {
        struct Adapter<'a, W: Write> {
            out: &'a mut W,
            err: Option<W::Error>,
        }
        impl<W: Write> fmt::Write for Adapter<'_, W> {
            fn write_str(&mut self, s: &str) -> fmt::Result {
                self.out.write_all(s.as_bytes()).map_err(|e| {
                    self.err = Some(e);
                    fmt::Error
                })
            }
        }
        self.begin_raw()?;
        let mut a = Adapter { out: &mut self.out, err: None };
        let _ = fmt::write(&mut a, if use_sci { sci } else { plain });
        if let Some(e) = a.err {
            return Err(e);
        }
        self.end_raw();
        Ok(())
    }
}

/// Types that can be encoded as JSON with a [`JsonWriter`].
///
/// ```
/// use emjson::{JsonWriter, ToJson, io::Write};
///
/// struct Reading { sensor: &'static str, celsius: f32 }
///
/// impl ToJson for Reading {
///     fn to_json<W: Write>(&self, w: &mut JsonWriter<W>) -> Result<(), W::Error> {
///         w.begin_object()?;
///         w.member("sensor", self.sensor)?;
///         w.member("celsius", &self.celsius)?;
///         w.end_object()
///     }
/// }
///
/// let r = Reading { sensor: "t1", celsius: 21.5 };
/// assert_eq!(emjson::encoded_len(&r), 30); // {"sensor":"t1","celsius":21.5}
/// ```
pub trait ToJson {
    /// Writes `self` as one JSON value.
    fn to_json<W: Write>(&self, w: &mut JsonWriter<W>) -> Result<(), W::Error>;
}

impl<T: ToJson + ?Sized> ToJson for &T {
    fn to_json<W: Write>(&self, w: &mut JsonWriter<W>) -> Result<(), W::Error> {
        (**self).to_json(w)
    }
}

impl<T: ToJson + ?Sized> ToJson for &mut T {
    fn to_json<W: Write>(&self, w: &mut JsonWriter<W>) -> Result<(), W::Error> {
        (**self).to_json(w)
    }
}

impl ToJson for str {
    fn to_json<W: Write>(&self, w: &mut JsonWriter<W>) -> Result<(), W::Error> {
        w.string(self)
    }
}

impl ToJson for char {
    fn to_json<W: Write>(&self, w: &mut JsonWriter<W>) -> Result<(), W::Error> {
        w.string(self.encode_utf8(&mut [0; 4]))
    }
}

impl ToJson for bool {
    fn to_json<W: Write>(&self, w: &mut JsonWriter<W>) -> Result<(), W::Error> {
        w.bool(*self)
    }
}

/// `()` is written as `null`.
impl ToJson for () {
    fn to_json<W: Write>(&self, w: &mut JsonWriter<W>) -> Result<(), W::Error> {
        w.null()
    }
}

macro_rules! impl_int {
    ($method:ident as $wide:ty: $($t:ty),*) => {$(
        impl ToJson for $t {
            fn to_json<W: Write>(&self, w: &mut JsonWriter<W>) -> Result<(), W::Error> {
                w.$method(*self as $wide)
            }
        }
    )*};
}

impl_int!(u32 as u32: u8, u16, u32);
impl_int!(i32 as i32: i8, i16, i32);
impl_int!(u64 as u64: u64);
impl_int!(i64 as i64: i64);

impl ToJson for usize {
    fn to_json<W: Write>(&self, w: &mut JsonWriter<W>) -> Result<(), W::Error> {
        w.usize(*self, false)
    }
}

impl ToJson for isize {
    fn to_json<W: Write>(&self, w: &mut JsonWriter<W>) -> Result<(), W::Error> {
        w.usize(self.unsigned_abs(), *self < 0)
    }
}

impl ToJson for u128 {
    fn to_json<W: Write>(&self, w: &mut JsonWriter<W>) -> Result<(), W::Error> {
        w.u128(*self, false)
    }
}

impl ToJson for i128 {
    fn to_json<W: Write>(&self, w: &mut JsonWriter<W>) -> Result<(), W::Error> {
        w.u128(self.unsigned_abs(), *self < 0)
    }
}

impl ToJson for f64 {
    fn to_json<W: Write>(&self, w: &mut JsonWriter<W>) -> Result<(), W::Error> {
        w.f64(*self)
    }
}

impl ToJson for f32 {
    fn to_json<W: Write>(&self, w: &mut JsonWriter<W>) -> Result<(), W::Error> {
        w.f32(*self)
    }
}

/// `None` is written as `null`.
impl<T: ToJson> ToJson for Option<T> {
    fn to_json<W: Write>(&self, w: &mut JsonWriter<W>) -> Result<(), W::Error> {
        match self {
            Some(v) => v.to_json(w),
            None => w.null(),
        }
    }
}

impl<T: ToJson> ToJson for [T] {
    fn to_json<W: Write>(&self, w: &mut JsonWriter<W>) -> Result<(), W::Error> {
        w.begin_array()?;
        for v in self {
            v.to_json(w)?;
        }
        w.end_array()
    }
}

impl<T: ToJson, const M: usize> ToJson for [T; M] {
    fn to_json<W: Write>(&self, w: &mut JsonWriter<W>) -> Result<(), W::Error> {
        self[..].to_json(w)
    }
}

/// Object members given as `(key, value)` pairs.
///
/// ```
/// use emjson::writer::Object;
/// let o = Object(&[("a", 1), ("b", 2)]);
/// let mut buf = [0u8; 16];
/// assert_eq!(emjson::to_slice(&o, &mut buf).unwrap(), r#"{"a":1,"b":2}"#);
/// ```
#[derive(Debug, Clone, Copy)]
pub struct Object<'a, T>(pub &'a [(&'a str, T)]);

impl<T: ToJson> ToJson for Object<'_, T> {
    fn to_json<W: Write>(&self, w: &mut JsonWriter<W>) -> Result<(), W::Error> {
        w.begin_object()?;
        for (k, v) in self.0 {
            w.member(k, v)?;
        }
        w.end_object()
    }
}

/// Already encoded JSON, written as is (the caller is responsible for its validity).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawJson<'a>(pub &'a str);

impl ToJson for RawJson<'_> {
    fn to_json<W: Write>(&self, w: &mut JsonWriter<W>) -> Result<(), W::Error> {
        w.raw(self.0)
    }
}

/// Returns the length in bytes of the compact JSON encoding of `value`, without
/// writing it anywhere.
pub fn encoded_len<T: ToJson + ?Sized>(value: &T) -> u64 {
    let mut w = JsonWriter::new(Counter::new());
    let r: Result<(), Infallible> = value.to_json(&mut w);
    match r {
        Ok(()) => w.into_inner().count(),
    }
}

/// Encodes `value` as compact JSON into `buf`.
pub fn to_slice<'b, T: ToJson + ?Sized>(value: &T, buf: &'b mut [u8]) -> Result<&'b str, BufferFull> {
    let mut w = JsonWriter::new(SliceWriter::new(buf));
    value.to_json(&mut w)?;
    let out: &'b [u8] = w.into_inner().into_written();
    core::str::from_utf8(out).map_err(|_| BufferFull)
}
