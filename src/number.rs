//! Number scanning (validation) and integer formatting.

use crate::error::{Error, ErrorKind};
use crate::io::Source;
use crate::parser::{Parser, Res};
use crate::string::Sink;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Num {
    Start,
    Minus,
    Zero,
    Int,
    Dot,
    Frac,
    Exp,
    ExpSign,
    ExpDigits,
}

impl Num {
    #[inline]
    fn step(self, b: u8) -> Option<Self> {
        use Num::*;
        Some(match (self, b) {
            (Start, b'-') => Minus,
            (Start | Minus, b'0') => Zero,
            (Start | Minus, b'1'..=b'9') => Int,
            (Int, b'0'..=b'9') => Int,
            (Zero | Int, b'.') => Dot,
            (Dot | Frac, b'0'..=b'9') => Frac,
            (Zero | Int | Frac, b'e' | b'E') => Exp,
            (Exp, b'+' | b'-') => ExpSign,
            (Exp | ExpSign | ExpDigits, b'0'..=b'9') => ExpDigits,
            _ => return None,
        })
    }

    fn is_final(self) -> bool {
        matches!(self, Num::Zero | Num::Int | Num::Frac | Num::ExpDigits)
    }
}

impl<S: Source, const N: usize> Parser<S, N> {
    /// Consumes and validates a number, passing its text to `sink`.
    pub(crate) fn scan_number<K: Sink>(&mut self, sink: &mut K) -> Res<(), S> {
        let mut st = Num::Start;
        loop {
            let buf = match self.src.fill() {
                Ok(b) => b,
                Err(e) => return Err(Error::Io(e)),
            };
            let mut i = 0;
            while let Some(&b) = buf.get(i) {
                match st.step(b) {
                    Some(next) => st = next,
                    None => break,
                }
                i += 1;
            }
            let more = i == buf.len() && i > 0;
            sink.put(&buf[..i]);
            self.src.consume(i);
            if !more {
                break;
            }
        }
        if st.is_final() { Ok(()) } else { self.fail(ErrorKind::InvalidNumber) }
    }
}

macro_rules! fmt_uint {
    ($name:ident, $t:ty) => {
        /// Formats `v` in decimal at the end of `buf` (which must be large enough),
        /// returning the digits.
        pub(crate) fn $name(mut v: $t, buf: &mut [u8]) -> &[u8] {
            let mut i = buf.len();
            loop {
                i -= 1;
                buf[i] = b'0' + (v % 10) as u8;
                v /= 10;
                if v == 0 {
                    return &buf[i..];
                }
            }
        }
    };
}

// Separate widths so that 32-bit targets only link 64-bit division when 64-bit values
// are actually formatted.
fmt_uint!(fmt_u32, u32);
fmt_uint!(fmt_u64, u64);
fmt_uint!(fmt_u128, u128);

/// Formats a `usize` with native-width arithmetic (`buf` must hold 20 bytes).
pub(crate) fn fmt_usize(v: usize, buf: &mut [u8]) -> &[u8] {
    #[cfg(target_pointer_width = "64")]
    return fmt_u64(v as u64, buf);
    #[cfg(not(target_pointer_width = "64"))]
    return fmt_u32(v as u32, buf);
}
