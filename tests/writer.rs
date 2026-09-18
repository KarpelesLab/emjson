mod common;

use common::*;
use emjson::io::{ReadSource, SliceWriter, Write};
use emjson::writer::Object;
use emjson::{JsonWriter, Parser, RawJson, ToJson, copy_value, encoded_len, to_slice};
use serde_json::Value;
use std::convert::Infallible;

/// Collects output into a Vec.
struct VecWriter(Vec<u8>);

impl Write for VecWriter {
    type Error = Infallible;
    fn write_all(&mut self, data: &[u8]) -> Result<(), Infallible> {
        self.0.extend_from_slice(data);
        Ok(())
    }
}

fn encode<T: ToJson + ?Sized>(v: &T) -> String {
    let mut w = JsonWriter::new(VecWriter(Vec::new()));
    v.to_json(&mut w).unwrap();
    let out = String::from_utf8(w.into_inner().0).unwrap();
    assert_eq!(encoded_len(v), out.len() as u64);
    out
}

#[test]
fn strings_roundtrip() {
    let mut rng = Rng::new(1);
    for _ in 0..5000 {
        let s = Gen::new(&mut rng).random_string();
        let json = encode(s.as_str());
        assert_eq!(serde_json::from_str::<String>(&json).unwrap(), s);
        // Only necessary escapes are used: same as serde_json's output.
        assert_eq!(json, serde_json::to_string(&s).unwrap());
    }
}

#[test]
fn scalars() {
    assert_eq!(encode(&true), "true");
    assert_eq!(encode(&()), "null");
    assert_eq!(encode(&None::<u8>), "null");
    assert_eq!(encode(&Some(5u8)), "5");
    assert_eq!(encode(&'"'), r#""\"""#);
    assert_eq!(encode(&0u64), "0");
    assert_eq!(encode(&u64::MAX), "18446744073709551615");
    assert_eq!(encode(&i64::MIN), "-9223372036854775808");
    assert_eq!(encode(&i128::MIN), "-170141183460469231731687303715884105728");
    assert_eq!(encode(&u128::MAX), "340282366920938463463374607431768211455");
    assert_eq!(encode(&-7i8), "-7");
    assert_eq!(encode(&1.5f64), "1.5");
    assert_eq!(encode(&-0.0f64), "-0");
    assert_eq!(encode(&1e300f64), "1e300");
    assert_eq!(encode(&1.5e-7f64), "1.5e-7");
    assert_eq!(encode(&123456.0f64), "123456");
    assert_eq!(encode(&f64::NAN), "null");
    assert_eq!(encode(&f32::INFINITY), "null");
    assert_eq!(encode(&0.1f32), "0.1");
    assert_eq!(encode("\u{1}\u{1f}\u{7f}"), "\"\\u0001\\u001f\u{7f}\"");
    assert_eq!(encode(&[1u8, 2, 3]), "[1,2,3]");
    assert_eq!(encode(&[[0u8; 0]; 2]), "[[],[]]");
    assert_eq!(encode(&Object(&[("a", 1), ("b\"", 2)])), r#"{"a":1,"b\"":2}"#);
    assert_eq!(encode(&RawJson(r#"{"x":[1]}"#)), r#"{"x":[1]}"#);
}

#[test]
fn floats_roundtrip() {
    let mut rng = Rng::new(99);
    for i in 0..20000 {
        let v = if i % 2 == 0 { f64::from_bits(rng.next()) } else { (rng.next() as i64 as f64) / 1000.0 };
        if !v.is_finite() {
            continue;
        }
        let json = encode(&v);
        let mut p = Parser::from_slice(json.as_bytes());
        let back: f64 = p.read_num().unwrap();
        assert_eq!(back.to_bits(), v.to_bits(), "{v:?} -> {json}");
        let f = f32::from_bits(rng.next() as u32);
        if f.is_finite() {
            let json = encode(&f);
            let back: f32 = Parser::from_slice(json.as_bytes()).read_num().unwrap();
            assert_eq!(back.to_bits(), f.to_bits(), "{f:?} -> {json}");
        }
    }
}

#[test]
fn pretty() {
    let mut buf = [0u8; 256];
    let mut w = JsonWriter::pretty(SliceWriter::new(&mut buf), 2);
    w.begin_object().unwrap();
    w.member("a", &1).unwrap();
    w.key("b").unwrap();
    w.begin_array().unwrap();
    w.value(&true).unwrap();
    w.begin_object().unwrap();
    w.end_object().unwrap();
    w.begin_array().unwrap();
    w.end_array().unwrap();
    w.end_array().unwrap();
    w.end_object().unwrap();
    let out = std::str::from_utf8(w.get_ref().written()).unwrap();
    assert_eq!(out, "{\n  \"a\": 1,\n  \"b\": [\n    true,\n    {},\n    []\n  ]\n}");
}

#[test]
fn top_level_sequence_is_ndjson() {
    let mut buf = [0u8; 32];
    let mut w = JsonWriter::new(SliceWriter::new(&mut buf));
    w.value(&1).unwrap();
    w.value(&[2]).unwrap();
    w.string("x").unwrap();
    assert_eq!(w.get_ref().written(), b"1\n[2]\n\"x\"");
}

#[test]
fn slice_writer_overflow() {
    let mut buf = [0u8; 4];
    assert!(to_slice(&[1, 2, 3], &mut buf).is_err());
    let mut buf = [0u8; 7];
    assert_eq!(to_slice(&[1, 2, 3], &mut buf).unwrap(), "[1,2,3]");
}

#[test]
fn copy_value_minify_and_pretty() {
    for seed in 0..2000 {
        let doc = random_doc(seed);
        let expected: Value = serde_json::from_str(&doc).unwrap();
        for indent in [0u8, 1, 4] {
            let mut rbuf = [0u8; 7];
            let mut p = Parser::new(ReadSource::new(doc.as_bytes(), &mut rbuf));
            let mut w = if indent == 0 {
                JsonWriter::new(VecWriter(Vec::new()))
            } else {
                JsonWriter::pretty(VecWriter(Vec::new()), indent)
            };
            copy_value(&mut p, &mut w).unwrap();
            p.finish().unwrap();
            let out = w.into_inner().0;
            let v: Value =
                serde_json::from_slice(&out).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out)));
            assert_eq!(v, expected, "seed {seed}");
            if indent == 0 {
                // Minified: equal to serde's compact output modulo key order, so compare lengths.
                assert_eq!(out.len(), serde_compact_len(&doc), "seed {seed}");
            }
        }
    }
}

/// Expected length of the minified document: serde's compact output (same string
/// escaping; key order does not matter for the length), except that copy_value keeps
/// number text as is while serde reformats numbers.
fn serde_compact_len(doc: &str) -> usize {
    let v: Value = serde_json::from_str(doc).unwrap();
    let mut len = serde_json::to_string(&v).unwrap().len() as isize;
    let mut p = Parser::from_slice(doc.as_bytes());
    let (mut path, mut buf) = ([0u8; 4096], [0u8; 128]);
    p.walk(&mut path, |node| {
        if node.token() == emjson::Token::Number {
            let text = node.read_number_str(&mut buf)?;
            let n: Value = serde_json::from_str(text).unwrap();
            len += text.len() as isize - serde_json::to_string(&n).unwrap().len() as isize;
        }
        Ok(emjson::Flow::Continue)
    })
    .unwrap();
    len as usize
}

#[test]
fn copy_value_extracts_subdocument() {
    let doc = br#"{"a": {"b": [1, {"c": "d"}]}, "e": 2}"#;
    let mut p = Parser::from_slice(doc);
    assert!(p.seek("/a/b").unwrap());
    let mut out = [0u8; 64];
    let mut w = JsonWriter::new(SliceWriter::new(&mut out));
    copy_value(&mut p, &mut w).unwrap();
    assert_eq!(w.get_ref().written(), br#"[1,{"c":"d"}]"#);
    // Still inside "a": leave it to reach its sibling.
    assert_eq!(p.depth(), 2);
    p.unwind(1).unwrap();
    assert!(p.find_key("e").unwrap());
}
