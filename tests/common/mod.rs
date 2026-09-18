#![allow(dead_code)]

use emjson::io::{Read, ReadSource};
use emjson::{Error, Event, Parser, Source};
use serde_json::{Map, Value};
use std::convert::Infallible;
use std::string::String;
use std::vec::Vec;

/// Small deterministic PRNG (xorshift64*).
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1)
    }
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    pub fn chance(&mut self, pct: usize) -> bool {
        self.below(100) < pct
    }
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// Reader that returns at most `max` bytes per call.
pub struct Trickle<'a> {
    pub data: &'a [u8],
    pub max: usize,
}

impl Read for Trickle<'_> {
    type Error = Infallible;
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Infallible> {
        let n = buf.len().min(self.max).min(self.data.len());
        buf[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}

/// Random JSON text generator (valid documents), with random whitespace, escapes and
/// number formats.
pub struct Gen<'r> {
    pub rng: &'r mut Rng,
    pub out: String,
    pub max_depth: usize,
    /// Probability (percent) of emitting whitespace at each opportunity.
    pub ws: usize,
    /// Use distinct keys in each object (so that seek results are unambiguous).
    pub unique_keys: bool,
}

const WORDS: &[&str] =
    &["a", "b", "foo", "bar", "baz", "key", "x", "value", "list", "n", "é", "日本", "~", "/", "a/b", "~1"];

impl<'r> Gen<'r> {
    pub fn new(rng: &'r mut Rng) -> Self {
        Gen { rng, out: String::new(), max_depth: 5, ws: 20, unique_keys: true }
    }

    fn ws(&mut self) {
        while self.rng.chance(self.ws) {
            let c = *self.rng.pick(&[' ', '\n', '\t', '\r', ' ', ' ']);
            self.out.push(c);
        }
    }

    pub fn doc(mut self) -> String {
        self.ws();
        self.value(0);
        self.ws();
        self.out
    }

    pub fn value(&mut self, depth: usize) {
        let kinds = if depth >= self.max_depth { 4 } else { 6 };
        match self.rng.below(kinds) {
            0 => self.number(),
            1 => {
                let s = self.random_string();
                self.string(&s)
            }
            2 => {
                let lit = *self.rng.pick::<&str>(&["true", "false"]);
                self.out.push_str(lit)
            }
            3 => self.out.push_str("null"),
            4 => {
                self.out.push('[');
                let n = self.rng.below(5);
                for i in 0..n {
                    if i > 0 {
                        self.ws();
                        self.out.push(',');
                    }
                    self.ws();
                    self.value(depth + 1);
                }
                self.ws();
                self.out.push(']');
            }
            _ => {
                self.out.push('{');
                let n = self.rng.below(5);
                let mut used: Vec<String> = Vec::new();
                for i in 0..n {
                    let mut key = self.random_key();
                    if self.unique_keys {
                        while used.contains(&key) {
                            key.push('_');
                        }
                        used.push(key.clone());
                    }
                    if i > 0 {
                        self.ws();
                        self.out.push(',');
                    }
                    self.ws();
                    self.string(&key);
                    self.ws();
                    self.out.push(':');
                    self.ws();
                    self.value(depth + 1);
                }
                self.ws();
                self.out.push('}');
            }
        }
    }

    pub fn random_key(&mut self) -> String {
        if self.rng.chance(70) { String::from(*self.rng.pick(WORDS)) } else { self.random_string() }
    }

    pub fn random_string(&mut self) -> String {
        let max = if self.rng.chance(10) { 200 } else { 12 };
        let n = self.rng.below(max);
        let mut s = String::new();
        for _ in 0..n {
            let c = match self.rng.below(10) {
                0 => char::from_u32(self.rng.below(0x20) as u32).unwrap(),
                1 => *self.rng.pick(&['"', '\\', '/', '~']),
                2 => *self.rng.pick(&['é', 'ß', '€', '日', '😀', '\u{10FFFF}', '\u{7FF}', '\u{800}', '\u{FFFD}']),
                3 => char::from_u32(0x80 + self.rng.below(0x700) as u32).unwrap(),
                _ => (b'a' + self.rng.below(26) as u8) as char,
            };
            s.push(c);
        }
        s
    }

    /// Writes `s` as a JSON string, with random choice of escapes.
    pub fn string(&mut self, s: &str) {
        self.out.push('"');
        for c in s.chars() {
            match c {
                '"' => self.out.push_str("\\\""),
                '\\' => self.out.push_str("\\\\"),
                '\n' if self.rng.chance(50) => self.out.push_str("\\n"),
                '\t' if self.rng.chance(50) => self.out.push_str("\\t"),
                '/' if self.rng.chance(50) => self.out.push_str("\\/"),
                c if (c as u32) < 0x20 || self.rng.chance(10) => {
                    let mut units = [0u16; 2];
                    for u in c.encode_utf16(&mut units) {
                        if self.rng.chance(50) {
                            self.out.push_str(&format!("\\u{:04x}", u));
                        } else {
                            self.out.push_str(&format!("\\u{:04X}", u));
                        }
                    }
                }
                c => self.out.push(c),
            }
        }
        self.out.push('"');
    }

    pub fn number(&mut self) {
        let mut s = String::new();
        if self.rng.chance(30) {
            s.push('-');
        }
        if self.rng.chance(20) {
            s.push('0');
        } else {
            s.push((b'1' + self.rng.below(9) as u8) as char);
            let max = if self.rng.chance(10) { 25 } else { 5 };
            for _ in 0..self.rng.below(max) {
                s.push((b'0' + self.rng.below(10) as u8) as char);
            }
        }
        if self.rng.chance(30) {
            s.push('.');
            for _ in 0..1 + self.rng.below(6) {
                s.push((b'0' + self.rng.below(10) as u8) as char);
            }
        }
        if self.rng.chance(20) {
            s.push(*self.rng.pick(&['e', 'E']));
            if self.rng.chance(50) {
                s.push(*self.rng.pick(&['+', '-']));
            }
            for _ in 0..1 + self.rng.below(2) {
                s.push((b'0' + self.rng.below(10) as u8) as char);
            }
        }
        self.out.push_str(&s);
    }
}

pub fn random_doc(seed: u64) -> String {
    let mut rng = Rng::new(seed);
    Gen::new(&mut rng).doc()
}

/// Builds a serde Value from parser events.
pub fn build<S: Source>(p: &mut Parser<S>, buf: &mut [u8]) -> Result<Value, Error<S::Error>> {
    let o = own(p.next_event(buf)?);
    build_owned(o, p, buf)
}

fn build_from<S: Source>(ev: Event<'_>, p: &mut Parser<S>, buf: &mut [u8]) -> Result<Value, Error<S::Error>> {
    Ok(match ev {
        Event::Null => Value::Null,
        Event::Bool(b) => Value::Bool(b),
        Event::Number(n) => number(n),
        Event::String(s) => Value::String(s.into()),
        Event::BeginArray => {
            let mut v = Vec::new();
            loop {
                let ev = p.next_event(buf)?;
                if ev == Event::EndArray {
                    break;
                }
                // Copy the event out of buf before recursing.
                let owned = own(ev);
                v.push(build_owned(owned, p, buf)?);
            }
            Value::Array(v)
        }
        Event::BeginObject => {
            let mut m = Map::new();
            loop {
                let key = match p.next_event(buf)? {
                    Event::EndObject => break,
                    Event::Key(k) => String::from(k),
                    other => panic!("unexpected event {other:?}"),
                };
                let ev = own(p.next_event(buf)?);
                let v = build_owned(ev, p, buf)?;
                m.insert(key, v);
            }
            Value::Object(m)
        }
        other => panic!("unexpected event {other:?}"),
    })
}

/// Converts number text with serde; emjson accepts any syntactically valid number, serde
/// rejects those out of f64 range.
fn number(n: &str) -> Value {
    serde_json::from_str(n).unwrap_or_else(|e| {
        assert!(e.to_string().contains("out of range"), "number {n:?} accepted by emjson, serde: {e}");
        Value::Null
    })
}

pub enum Owned {
    Scalar(Value),
    BeginArray,
    BeginObject,
}

fn own(ev: Event<'_>) -> Owned {
    match ev {
        Event::BeginArray => Owned::BeginArray,
        Event::BeginObject => Owned::BeginObject,
        Event::Null => Owned::Scalar(Value::Null),
        Event::Bool(b) => Owned::Scalar(Value::Bool(b)),
        Event::Number(n) => Owned::Scalar(number(n)),
        Event::String(s) => Owned::Scalar(Value::String(s.into())),
        other => panic!("unexpected event {other:?}"),
    }
}

fn build_owned<S: Source>(o: Owned, p: &mut Parser<S>, buf: &mut [u8]) -> Result<Value, Error<S::Error>> {
    match o {
        Owned::Scalar(v) => Ok(v),
        Owned::BeginArray => build_from(Event::BeginArray, p, buf),
        Owned::BeginObject => build_from(Event::BeginObject, p, buf),
    }
}

/// Parses `doc` to a Value with emjson, reading through a buffer of `bufsize` bytes.
pub fn parse_via_stream(doc: &[u8], bufsize: usize) -> Result<Value, Error<Infallible>> {
    let mut rbuf = vec![0u8; bufsize];
    let mut sbuf = vec![0u8; doc.len() + 8];
    let mut p = Parser::new(ReadSource::new(doc, &mut rbuf));
    let v = build(&mut p, &mut sbuf)?;
    p.finish()?;
    Ok(v)
}
