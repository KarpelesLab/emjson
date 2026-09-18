mod common;

use common::*;
use emjson::io::ReadSource;
use emjson::{ErrorKind, Event, Parser, SliceSource, Token, validate};
use serde_json::Value;

const VALID: &[&str] = &[
    "0",
    "-0",
    "1",
    "-1",
    "123456789012345678901234567890",
    "1.5",
    "-0.0e0",
    "1e10",
    "1E+10",
    "1e-10",
    "0.000001",
    "true",
    "false",
    "null",
    r#""""#,
    r#""abc""#,
    r#""\"\\\/\b\f\n\r\t""#,
    r#""\u0000\u001f\u00e9\u20AC\ud83d\ude00""#,
    "\"é日本😀\"",
    "[]",
    "{}",
    "[[]]",
    "[{}]",
    r#"{"a":{}}"#,
    " \t\n\r [ 1 , 2 ,\n3 ] \n",
    r#"{"a":1,"b":[true,false,null],"c":{"d":"e"}}"#,
    r#"{"":""}"#,
    r#"{"a":1,"a":2}"#,
    r#"[-1.5e-3, 0.5, 10, 0, -0]"#,
    r#"{"\u0061":"\u0062"}"#,
];

const INVALID: &[&str] = &[
    "",
    " ",
    "[",
    "]",
    "{",
    "}",
    "[1,]",
    "[,1]",
    "[1 2]",
    "{\"a\"}",
    "{\"a\":}",
    "{\"a\" 1}",
    "{\"a\":1,}",
    "{,}",
    "{1:2}",
    "{'a':1}",
    "[1}",
    "{\"a\":1]",
    "01",
    "-",
    "-01",
    "1.",
    ".1",
    "1e",
    "1e+",
    "+1",
    "0x10",
    "1.e3",
    "NaN",
    "Infinity",
    "tru",
    "truee",
    "nul",
    "True",
    "\"abc",
    "\"\\x\"",
    "\"\\u12\"",
    "\"\\u12g4\"",
    "\"\\ud800\"",
    "\"\\ud800\\u0041\"",
    "\"\\udc00\"",
    "\"\\ud800x\"",
    "\"\t\"",
    "\"\n\"",
    "[1] [2]",
    "1 2",
    "[\"a\":1]",
    "{\"a\":1 \"b\":2}",
    "[1,,2]",
    "\u{feff}[]",
];

fn serde_value(doc: &[u8]) -> Value {
    serde_json::from_slice(doc).unwrap_or_else(|e| panic!("serde rejects {:?}: {e}", String::from_utf8_lossy(doc)))
}

#[test]
fn valid_corpus() {
    for doc in VALID {
        let expected = serde_value(doc.as_bytes());
        for bufsize in [1, 2, 3, 5, 7, 64] {
            let v = parse_via_stream(doc.as_bytes(), bufsize).unwrap_or_else(|e| panic!("{doc:?}: {e}"));
            assert_eq!(v, expected, "{doc:?} bufsize {bufsize}");
        }
        validate(SliceSource::new(doc.as_bytes())).unwrap();
    }
}

#[test]
fn invalid_corpus() {
    for doc in INVALID {
        assert!(serde_json::from_str::<Value>(doc).is_err(), "serde accepts {doc:?}");
        assert!(validate(SliceSource::new(doc.as_bytes())).is_err(), "validate accepts {doc:?}");
        for bufsize in [1, 3, 64] {
            assert!(parse_via_stream(doc.as_bytes(), bufsize).is_err(), "parse accepts {doc:?}");
        }
    }
}

#[test]
fn invalid_utf8() {
    let cases: &[&[u8]] = &[
        b"\"\xff\"",
        b"\"\xc0\x80\"",             // overlong
        b"\"\xe0\x80\x80\"",         // overlong
        b"\"\xed\xa0\x80\"",         // surrogate
        b"\"\xf4\x90\x80\x80\"",     // > U+10FFFF
        b"\"\xc3\"",                 // truncated
        b"\"\xe2\x82\"",             // truncated
        b"\"\x80\"",                 // lone continuation
        b"\"\xf8\x88\x80\x80\x80\"", // 5 bytes
    ];
    for doc in cases {
        assert!(serde_json::from_slice::<Value>(doc).is_err());
        let err = validate(SliceSource::new(doc)).unwrap_err();
        assert_eq!(err.kind(), Some(ErrorKind::InvalidUnicode), "{doc:?}");
    }
}

#[test]
fn error_offsets() {
    let check = |doc: &str, kind: ErrorKind, offset: u64| {
        let e = validate(SliceSource::new(doc.as_bytes())).unwrap_err();
        assert_eq!(e.kind(), Some(kind), "{doc:?}");
        assert_eq!(e.offset(), Some(offset), "{doc:?}");
    };
    check("[1,]", ErrorKind::UnexpectedByte(b']'), 3);
    check("{\"a\" 1}", ErrorKind::UnexpectedByte(b'1'), 5);
    check("[1] x", ErrorKind::TrailingData, 4);
    check("[1", ErrorKind::UnexpectedEof, 2);
    check("[\"a\tb\"]", ErrorKind::ControlCharacter, 3);
    check("[\"\\q\"]", ErrorKind::InvalidEscape, 3);
    check("[1.]", ErrorKind::InvalidNumber, 3);
    check("[tru]", ErrorKind::UnexpectedByte(b']'), 4);
}

#[test]
fn random_differential() {
    for seed in 0..3000 {
        let doc = random_doc(seed);
        let expected = serde_value(doc.as_bytes());
        for bufsize in [1, 3, 16, 4096] {
            let v = parse_via_stream(doc.as_bytes(), bufsize).unwrap_or_else(|e| panic!("seed {seed}: {e}\n{doc}"));
            assert_eq!(v, expected, "seed {seed} bufsize {bufsize}\n{doc}");
        }
        // A reader returning few bytes per call, with a larger buffer.
        let mut rbuf = [0u8; 64];
        let mut sbuf = vec![0u8; doc.len()];
        let mut p =
            Parser::new(ReadSource::new(Trickle { data: doc.as_bytes(), max: 1 + seed as usize % 5 }, &mut rbuf));
        assert_eq!(build(&mut p, &mut sbuf).unwrap(), expected);
        p.finish().unwrap();

        validate(SliceSource::new(doc.as_bytes())).unwrap();
        // The span of the root value is the document minus surrounding whitespace.
        let mut p = Parser::from_slice(doc.as_bytes());
        let raw = p.raw_value().unwrap();
        assert_eq!(raw, doc.trim_matches([' ', '\n', '\t', '\r']).as_bytes());
    }
}

/// Mutated documents: emjson must agree with serde_json on validity, and never panic.
#[test]
fn mutation_fuzz() {
    let alphabet = b"{}[],:\"\\ 0123456789.eE+-tfnulrsa/u\xc3\xa9\xff\x01";
    let mut rng = Rng::new(42);
    let mut checked = 0;
    for seed in 0..20000u64 {
        let mut doc = random_doc(seed % 500).into_bytes();
        for _ in 0..1 + rng.below(3) {
            let i = rng.below(doc.len() + 1);
            match rng.below(3) {
                0 if i < doc.len() => {
                    doc.remove(i);
                }
                1 if i < doc.len() => doc[i] = *rng.pick(alphabet),
                _ => doc.insert(i, *rng.pick(alphabet)),
            }
        }
        let ours = validate(SliceSource::new(&doc));
        let theirs = serde_json::from_slice::<Value>(&doc);
        match (&ours, &theirs) {
            (Ok(()), Ok(_)) | (Err(_), Err(_)) => checked += 1,
            (Ok(()), Err(e)) if e.to_string().contains("number out of range") => {}
            _ => panic!("disagreement on {:?}: emjson {ours:?}, serde {theirs:?}", String::from_utf8_lossy(&doc)),
        }
        // Exercise other entry points on invalid input: must not panic.
        let _ = parse_via_stream(&doc, 3);
        let mut path = [0u8; 256];
        let _ = Parser::from_slice(&doc).walk(&mut path, |_| Ok(emjson::Flow::Continue));
        let _ = Parser::from_slice(&doc).seek("/a/0/b");
    }
    assert!(checked > 19000);
}

/// Random bytes must never cause a panic.
#[test]
fn garbage_does_not_panic() {
    let mut rng = Rng::new(7);
    for _ in 0..20000 {
        let len = rng.below(40);
        let doc: Vec<u8> = (0..len).map(|_| *rng.pick(b"{}[]:,\"\\ -01e.tnf\xe9\x80")).collect();
        let _ = validate(SliceSource::new(&doc));
        let _ = parse_via_stream(&doc, 2);
    }
}

#[test]
fn cursor_api() {
    let doc = br#"{"name": "dev", "id": 12, "tags": ["a", "b"], "cfg": {"x": 1, "y": [1, 2]}, "on": true, "z": null}"#;
    let mut p = Parser::from_slice(doc);
    let mut buf = [0u8; 16];
    p.begin_object().unwrap();
    assert!(p.has_next().unwrap());
    assert_eq!(p.read_key(&mut buf).unwrap(), "name");
    assert_eq!(p.read_str(&mut buf).unwrap(), "dev");
    assert!(p.match_key("id").unwrap());
    assert_eq!(p.read_num::<u32>().unwrap(), 12);
    // Skip "tags" entirely: read_key auto-skips the unread value.
    assert!(!p.match_key("nope").unwrap());
    assert_eq!(p.read_key(&mut buf).unwrap(), "cfg");
    p.begin_object().unwrap();
    assert!(p.find_key("y").unwrap());
    p.begin_array().unwrap();
    assert_eq!(p.read_num::<i8>().unwrap(), 1);
    p.end_array().unwrap(); // skips the remaining element
    p.end_object().unwrap();
    assert!(p.find_key("z").unwrap());
    assert_eq!(p.peek().unwrap(), Token::Null);
    p.read_null().unwrap();
    assert!(!p.has_next().unwrap());
    p.end_object().unwrap();
    assert_eq!(p.peek().unwrap(), Token::Eof);
    p.finish().unwrap();
}

#[test]
fn end_object_skips_rest() {
    let mut p = Parser::from_slice(br#"[{"a": [1, {"b": 2}], "c": "d"}, 5]"#);
    p.begin_array().unwrap();
    p.begin_object().unwrap();
    p.end_object().unwrap();
    assert_eq!(p.read_num::<i32>().unwrap(), 5);
    p.end_array().unwrap();
    p.finish().unwrap();

    // Pending member value is skipped.
    let mut p = Parser::from_slice(br#"{"a": [1, 2], "b": 3}"#);
    p.begin_object().unwrap();
    assert!(p.match_key("a").unwrap());
    p.end_object().unwrap();
    p.finish().unwrap();
}

#[test]
fn type_mismatch_is_recoverable() {
    let mut p = Parser::from_slice(br#"["x", 1]"#);
    p.begin_array().unwrap();
    let e = p.read_num::<i32>().unwrap_err();
    assert_eq!(e.kind(), Some(ErrorKind::TypeMismatch { expected: Token::Number, found: Token::String }));
    let mut buf = [0u8; 4];
    assert_eq!(p.read_str(&mut buf).unwrap(), "x");
    assert_eq!(p.read_num::<i32>().unwrap(), 1);
}

#[test]
fn buffer_too_small_is_recoverable() {
    let mut p = Parser::from_slice(br#"["hello world", "ok", "\u00e9\u00e9"]"#);
    p.begin_array().unwrap();
    let mut small = [0u8; 4];
    assert_eq!(p.read_str(&mut small).unwrap_err().kind(), Some(ErrorKind::BufferTooSmall));
    assert_eq!(p.read_str(&mut small).unwrap(), "ok");
    // Exact fit, with multi-byte characters from escapes.
    assert_eq!(p.read_str(&mut small).unwrap(), "éé");
    p.end_array().unwrap();
    p.finish().unwrap();

    // Exact fit of a raw multi-byte character at the end of the buffer.
    let mut p = Parser::from_slice("\"abcé\"".as_bytes());
    let mut buf = [0u8; 5];
    assert_eq!(p.read_str(&mut buf).unwrap(), "abcé");
    let mut p = Parser::from_slice("\"abcé\"".as_bytes());
    let mut buf = [0u8; 4];
    assert!(p.read_str(&mut buf).is_err());
}

#[test]
fn numbers() {
    let mut p = Parser::from_slice(b"[255, 256, -129, 1.5, 1e2, 18446744073709551615, -9223372036854775808, 1e400]");
    p.begin_array().unwrap();
    assert_eq!(p.read_num::<u8>().unwrap(), 255);
    assert_eq!(p.read_num::<u8>().unwrap_err().kind(), Some(ErrorKind::NumberOutOfRange));
    assert_eq!(p.read_num::<i8>().unwrap_err().kind(), Some(ErrorKind::NumberOutOfRange));
    assert_eq!(p.read_num::<i32>().unwrap_err().kind(), Some(ErrorKind::NumberOutOfRange));
    assert_eq!(p.read_num::<f64>().unwrap(), 100.0);
    assert_eq!(p.read_num::<u64>().unwrap(), u64::MAX);
    assert_eq!(p.read_num::<i64>().unwrap(), i64::MIN);
    assert_eq!(p.read_num::<f64>().unwrap(), f64::INFINITY);
    p.end_array().unwrap();

    let long = format!("[{}]", "1".repeat(100));
    let mut p = Parser::from_slice(long.as_bytes());
    p.begin_array().unwrap();
    assert_eq!(p.read_num::<f64>().unwrap_err().kind(), Some(ErrorKind::NumberTooLong));
    p.end_array().unwrap();
    p.finish().unwrap();

    let mut buf = [0u8; 8];
    let mut p = Parser::from_slice(b"-1.25e+3");
    assert_eq!(p.read_number_str(&mut buf).unwrap(), "-1.25e+3");
}

#[test]
fn depth_limit() {
    let ok = "[".repeat(8) + &"]".repeat(8);
    let too_deep = "[".repeat(9) + &"]".repeat(9);
    let mut p = Parser::<_, 1>::with_stack(SliceSource::new(ok.as_bytes()));
    p.skip_value().unwrap();
    p.finish().unwrap();
    let mut p = Parser::<_, 1>::with_stack(SliceSource::new(too_deep.as_bytes()));
    assert_eq!(p.skip_value().unwrap_err().kind(), Some(ErrorKind::DepthLimitExceeded));
    assert_eq!(Parser::<SliceSource, 1>::MAX_DEPTH, 8);
    assert_eq!(Parser::<SliceSource>::MAX_DEPTH, 64);

    let deep = "[".repeat(64) + &"]".repeat(64);
    validate(SliceSource::new(deep.as_bytes())).unwrap();
    let deeper = "[".repeat(65) + &"]".repeat(65);
    assert!(validate(SliceSource::new(deeper.as_bytes())).is_err());
    // Scalars need no stack at all.
    let mut p = Parser::<_, 0>::with_stack(SliceSource::new(b"\"x\""));
    p.skip_value().unwrap();
    p.finish().unwrap();
}

#[test]
fn multiple_values() {
    let doc = b"1 {\"a\":2}\n[3]\n\"x\"  ";
    let mut p = Parser::from_slice(doc).allow_multiple_values(true);
    let mut n = 0;
    while p.has_next().unwrap() {
        p.skip_value().unwrap();
        n += 1;
    }
    assert_eq!(n, 4);
    assert_eq!(p.peek().unwrap(), Token::Eof);

    let mut p = Parser::from_slice(doc);
    p.skip_value().unwrap();
    assert_eq!(p.finish().unwrap_err().kind(), Some(ErrorKind::TrailingData));
}

#[test]
fn str_reader_chunks() {
    for seed in 0..300 {
        let mut rng = Rng::new(seed);
        let mut g = Gen::new(&mut rng);
        let s = g.random_string();
        g.string(&s);
        let doc = g.out;
        for chunk in [4, 5, 7, 64] {
            let mut rbuf = [0u8; 3];
            let mut p = Parser::new(ReadSource::new(doc.as_bytes(), &mut rbuf));
            let mut r = p.str_reader().unwrap();
            let mut out = String::new();
            let mut buf = vec![0u8; chunk];
            while let Some(c) = r.next_chunk(&mut buf).unwrap() {
                assert!(!c.is_empty() && c.len() <= chunk);
                out.push_str(c);
            }
            assert!(r.is_done());
            assert_eq!(out, s);
            p.finish().unwrap();
        }
    }
}

#[test]
fn str_reader_partial_then_continue() {
    let mut p = Parser::from_slice(br#"{"long key here": "long value here", "n": 7}"#);
    p.begin_object().unwrap();
    let mut buf = [0u8; 4];
    {
        let mut r = p.str_reader().unwrap(); // the key
        assert_eq!(r.next_chunk(&mut buf).unwrap(), Some("long"));
    }
    {
        let mut r = p.str_reader().unwrap(); // the value (rest of key skipped)
        assert_eq!(r.next_chunk(&mut buf).unwrap(), Some("long"));
    }
    assert!(p.match_key("n").unwrap());
    assert_eq!(p.read_num::<u8>().unwrap(), 7);
}

#[test]
fn read_str_ref_borrows() {
    let doc = br#"["plain", "esc\naped"]"#;
    let mut p = Parser::from_slice(doc);
    let mut scratch = [0u8; 16];
    p.begin_array().unwrap();
    let s = p.read_str_ref(&mut scratch).unwrap();
    assert_eq!(s, "plain");
    assert_eq!(s.as_ptr(), doc[2..].as_ptr());
    let s = p.read_str_ref(&mut scratch).unwrap();
    assert_eq!(s, "esc\naped");
    p.end_array().unwrap();
}

#[test]
fn events() {
    let mut p = Parser::from_slice(br#"{"a": [1, "x", true, null]}"#);
    let mut buf = [0u8; 8];
    let mut got = Vec::new();
    loop {
        let e = p.next_event(&mut buf).unwrap();
        got.push(format!("{e:?}"));
        if e == Event::Eof {
            break;
        }
    }
    assert_eq!(
        got,
        [
            "BeginObject",
            "Key(\"a\")",
            "BeginArray",
            "Number(\"1\")",
            "String(\"x\")",
            "Bool(true)",
            "Null",
            "EndArray",
            "EndObject",
            "Eof"
        ]
    );
}

#[test]
fn value_spans() {
    let doc = br#" { "a" : [ 1 , "two" ] , "b" : { } } "#;
    let mut p = Parser::from_slice(doc);
    p.begin_object().unwrap();
    assert!(p.find_key("a").unwrap());
    let span = p.value_span().unwrap();
    assert_eq!(&doc[span.range().unwrap()], br#"[ 1 , "two" ]"#);
    assert!(p.find_key("b").unwrap());
    let span = p.value_span().unwrap();
    assert_eq!(&doc[span.range().unwrap()], b"{ }");
    p.end_object().unwrap();
    p.finish().unwrap();
}

#[test]
fn parser_is_small() {
    let size = std::mem::size_of::<Parser<SliceSource<'static>>>();
    let src = std::mem::size_of::<SliceSource<'static>>();
    assert!(size - src <= 24, "parser overhead {} bytes", size - src);
}
