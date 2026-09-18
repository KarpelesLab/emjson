//! Paths to values inside a document.
//!
//! A path is a sequence of [`Seg`]ments. Anything implementing [`Path`] can be used:
//!
//! - `&["foo", "bar"]`: member names (a name made of digits also indexes arrays).
//! - `"/foo/bar/0"`: a JSON Pointer ([RFC 6901](https://www.rfc-editor.org/rfc/rfc6901)),
//!   with `~1` for `/` and `~0` for `~` in names.
//! - `&[Seg::Key("foo"), Seg::Index(0)]`, or the [`path!`](crate::path!) macro.
//!
//! In an array, the segment `-` designates the position after the last element
//! (for [`Op::Set`](crate::edit::Op::Set) and [`Op::Insert`](crate::edit::Op::Insert)).

/// One step of a [`Path`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Seg<'a> {
    /// A member name. In an array, a name made only of decimal digits is used as an index.
    Key(&'a str),
    /// An array index. In an object, matches the member whose name is this number.
    Index(usize),
    /// A JSON Pointer reference token, still escaped (`~0` = `~`, `~1` = `/`).
    /// Otherwise like [`Seg::Key`].
    Pointer(&'a str),
}

impl<'a> From<&'a str> for Seg<'a> {
    fn from(s: &'a str) -> Self {
        Seg::Key(s)
    }
}

impl From<usize> for Seg<'_> {
    fn from(i: usize) -> Self {
        Seg::Index(i)
    }
}

fn parse_index(s: &[u8]) -> Option<usize> {
    if s.is_empty() || (s.len() > 1 && s[0] == b'0') {
        return None;
    }
    s.iter().try_fold(0usize, |acc, &b| {
        if b.is_ascii_digit() { acc.checked_mul(10)?.checked_add((b - b'0') as usize) } else { None }
    })
}

impl Seg<'_> {
    /// The array index this segment designates, if any.
    pub fn index(&self) -> Option<usize> {
        match *self {
            Seg::Index(i) => Some(i),
            Seg::Key(s) | Seg::Pointer(s) => parse_index(s.as_bytes()),
        }
    }

    /// Whether this is the `-` segment (past the end of an array).
    pub fn is_end_marker(&self) -> bool {
        matches!(self, Seg::Key("-") | Seg::Pointer("-"))
    }
}

/// A path to a value: a sequence of [`Seg`]ments.
pub trait Path {
    /// Iterator over the segments.
    type Iter<'a>: Iterator<Item = Seg<'a>>
    where
        Self: 'a;
    /// Returns the segments, from the root.
    fn segments(&self) -> Self::Iter<'_>;
}

impl<P: Path + ?Sized> Path for &P {
    type Iter<'a>
        = P::Iter<'a>
    where
        Self: 'a;
    fn segments(&self) -> Self::Iter<'_> {
        (**self).segments()
    }
}

/// Iterator over a slice of [`Seg`].
#[derive(Debug, Clone)]
pub struct SegIter<'a, 's>(core::slice::Iter<'a, Seg<'s>>);

impl<'a, 's: 'a> Iterator for SegIter<'a, 's> {
    type Item = Seg<'a>;
    fn next(&mut self) -> Option<Seg<'a>> {
        self.0.next().copied()
    }
}

impl<'s> Path for [Seg<'s>] {
    type Iter<'a>
        = SegIter<'a, 's>
    where
        Self: 'a;
    fn segments(&self) -> SegIter<'_, 's> {
        SegIter(self.iter())
    }
}

impl<'s, const M: usize> Path for [Seg<'s>; M] {
    type Iter<'a>
        = SegIter<'a, 's>
    where
        Self: 'a;
    fn segments(&self) -> SegIter<'_, 's> {
        SegIter(self.iter())
    }
}

/// Iterator over a slice of names.
#[derive(Debug, Clone)]
pub struct KeyIter<'a, 's>(core::slice::Iter<'a, &'s str>);

impl<'a, 's: 'a> Iterator for KeyIter<'a, 's> {
    type Item = Seg<'a>;
    fn next(&mut self) -> Option<Seg<'a>> {
        self.0.next().map(|k| Seg::Key(k))
    }
}

impl<'s> Path for [&'s str] {
    type Iter<'a>
        = KeyIter<'a, 's>
    where
        Self: 'a;
    fn segments(&self) -> KeyIter<'_, 's> {
        KeyIter(self.iter())
    }
}

impl<'s, const M: usize> Path for [&'s str; M] {
    type Iter<'a>
        = KeyIter<'a, 's>
    where
        Self: 'a;
    fn segments(&self) -> KeyIter<'_, 's> {
        KeyIter(self.iter())
    }
}

/// Iterator over the reference tokens of a JSON Pointer.
#[derive(Debug, Clone)]
pub struct PointerIter<'a> {
    rest: Option<&'a str>,
}

impl<'a> PointerIter<'a> {
    /// Splits a JSON Pointer. The empty string is the root; a missing leading `/` is
    /// tolerated.
    pub fn new(pointer: &'a str) -> Self {
        let rest = if pointer.is_empty() { None } else { Some(pointer.strip_prefix('/').unwrap_or(pointer)) };
        Self { rest }
    }
}

impl<'a> Iterator for PointerIter<'a> {
    type Item = Seg<'a>;
    fn next(&mut self) -> Option<Seg<'a>> {
        let r = self.rest?;
        match r.find('/') {
            Some(i) => {
                self.rest = Some(&r[i + 1..]);
                Some(Seg::Pointer(&r[..i]))
            }
            None => {
                self.rest = None;
                Some(Seg::Pointer(r))
            }
        }
    }
}

/// A string is a JSON Pointer.
impl Path for str {
    type Iter<'a> = PointerIter<'a>;
    fn segments(&self) -> PointerIter<'_> {
        PointerIter::new(self)
    }
}

/// Builds a path (`&[Seg]`) from names and indexes.
///
/// ```
/// let mut p = emjson::Parser::from_slice(br#"{"a": [10, {"b": 20}]}"#);
/// assert!(p.seek(emjson::path!["a", 1, "b"])?);
/// assert_eq!(p.read_num::<i32>()?, 20);
/// # Ok::<(), emjson::Error<core::convert::Infallible>>(())
/// ```
#[macro_export]
macro_rules! path {
    ($($seg:expr),* $(,)?) => {
        &[$($crate::Seg::from($seg)),*] as &[$crate::Seg<'_>]
    };
}
