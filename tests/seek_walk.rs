mod common;

use common::*;
use emjson::io::ReadSource;
use emjson::{ErrorKind, Flow, Parser, Seg, SliceSource, Token, path};
use serde_json::Value;

enum OwnedSeg {
    Key(String),
    Index(usize),
}

/// All paths in `v`, as (JSON Pointer, segments, value).
fn all_paths<'v>(
    v: &'v Value,
    ptr: String,
    segs: Vec<(String, bool)>,
    out: &mut Vec<(String, Vec<(String, bool)>, &'v Value)>,
) {
    out.push((ptr.clone(), segs.clone(), v));
    match v {
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                let mut s = segs.clone();
                s.push((i.to_string(), true));
                all_paths(x, format!("{ptr}/{i}"), s, out);
            }
        }
        Value::Object(m) => {
            for (k, x) in m {
                let mut s = segs.clone();
                s.push((k.clone(), false));
                all_paths(x, format!("{ptr}/{}", k.replace('~', "~0").replace('/', "~1")), s, out);
            }
        }
        _ => {}
    }
}

fn to_owned_segs(segs: &[(String, bool)]) -> Vec<OwnedSeg> {
    segs.iter()
        .map(|(s, idx)| if *idx { OwnedSeg::Index(s.parse().unwrap()) } else { OwnedSeg::Key(s.clone()) })
        .collect()
}

fn as_segs(o: &[OwnedSeg]) -> Vec<Seg<'_>> {
    o.iter()
        .map(|s| match s {
            OwnedSeg::Key(k) => Seg::Key(k),
            OwnedSeg::Index(i) => Seg::Index(*i),
        })
        .collect()
}

#[test]
fn user_example() {
    let json = br#"{"foo":{"bar":"hello world"}}"#;
    let mut rbuf = [0u8; 4];
    let mut p = Parser::new(ReadSource::new(&json[..], &mut rbuf));
    assert!(p.seek(&["foo", "bar"]).unwrap());
    assert_eq!(p.offset(), 14);
    assert_eq!(json[14], b'"');
    assert_eq!(p.peek().unwrap(), Token::String);
    let mut s = [0u8; 16];
    assert_eq!(p.read_str(&mut s).unwrap(), "hello world");
    p.end_object().unwrap();
    p.end_object().unwrap();
    p.finish().unwrap();
}

#[test]
fn seek_differential() {
    let mut checked = 0;
    for seed in 0..4000 {
        let doc = random_doc(seed);
        let value: Value = serde_json::from_str(&doc).unwrap();
        let mut paths = Vec::new();
        all_paths(&value, String::new(), Vec::new(), &mut paths);
        for (ptr, segs, expected) in &paths {
            // JSON Pointer string over a slice.
            let mut p = Parser::from_slice(doc.as_bytes());
            assert!(p.seek(ptr.as_str()).unwrap(), "seed {seed} {ptr:?}\n{doc}");
            let raw = p.raw_value().unwrap();
            assert_eq!(&serde_json::from_slice::<Value>(raw).unwrap(), *expected, "seed {seed} {ptr:?}");

            // Segments over a tiny-buffer stream; check the reported offset.
            let segs_str = segs;
            let owned = to_owned_segs(segs);
            let segs = as_segs(&owned);
            let mut rbuf = [0u8; 3];
            let mut p = Parser::new(ReadSource::new(doc.as_bytes(), &mut rbuf));
            assert!(p.seek(&segs[..]).unwrap());
            let start = p.offset() as usize;
            let span = p.value_span().unwrap();
            assert_eq!(span.start as usize, start);
            assert_eq!(&doc.as_bytes()[span.range().unwrap()], raw);

            // Names only (indexes as decimal strings).
            let names: Vec<&str> = segs_str.iter().map(|(s, _)| s.as_str()).collect();
            let mut p = Parser::from_slice(doc.as_bytes());
            assert!(p.seek(&names[..]).unwrap());
            assert_eq!(p.raw_value().unwrap(), raw);
            checked += 1;
        }
        // Paths that do not exist.
        for missing in ["/zzz", "/0/zzz", "/a/b/c/d/e/f/g", "/999"] {
            if value.pointer(missing).is_none() {
                assert!(!Parser::from_slice(doc.as_bytes()).seek(missing).unwrap());
            }
        }
    }
    assert!(checked > 8000, "{checked}");
}

#[test]
fn seek_details() {
    let doc = br#"{"a/b": {"~x": [10, 20, {"0": "zero"}]}, "list": [[1], [2, 3]], "": "empty"}"#;
    let get = |path: &str| -> Option<String> {
        let mut p = Parser::from_slice(doc);
        if p.seek(path).unwrap() { Some(String::from_utf8(p.raw_value().unwrap().to_vec()).unwrap()) } else { None }
    };
    assert_eq!(get("/a~1b/~0x/1").as_deref(), Some("20"));
    assert_eq!(get("/a~1b/~0x/2/0").as_deref(), Some("\"zero\""));
    assert_eq!(get("/list/1/1").as_deref(), Some("3"));
    assert_eq!(get("/").as_deref(), Some("\"empty\""));
    assert_eq!(get("").as_deref(), Some(std::str::from_utf8(doc).unwrap()));
    assert_eq!(get("/list/01"), None); // leading zero is not an index
    assert_eq!(get("/list/-"), None);
    assert_eq!(get("/list/2"), None);
    assert_eq!(get("/list/x"), None);
    assert_eq!(get("/a~1b/~0x/0/deeper"), None);

    let mut p = Parser::from_slice(doc);
    assert!(p.seek(path!["a/b", "~x", 2, "0"]).unwrap());
    // Index segment matching an object member named "0".
    let mut p = Parser::from_slice(doc);
    assert!(p.seek(path!["a/b", "~x", 2, 0]).unwrap());
    let mut buf = [0u8; 8];
    assert_eq!(p.read_str(&mut buf).unwrap(), "zero");
    let mut p = Parser::from_slice(doc);
    assert!(p.seek(&["list", "1", "0"]).unwrap());
    assert_eq!(p.read_num::<i32>().unwrap(), 2);
    // Parser can continue after reading: close containers and finish.
    p.unwind(0).unwrap();
    p.finish().unwrap();
}

#[test]
fn seek_relative() {
    let doc = br#"{"a": {"x": 1, "y": 2}, "b": {"x": 3}}"#;
    let mut p = Parser::from_slice(doc);
    p.begin_object().unwrap();
    assert!(p.find_key("a").unwrap());
    assert!(p.seek(&["y"]).unwrap());
    assert_eq!(p.read_num::<i32>().unwrap(), 2);
    assert_eq!(p.depth(), 2);
    p.unwind(1).unwrap();
    assert!(p.find_key("b").unwrap());
    assert!(p.seek(&["x"]).unwrap());
    assert_eq!(p.read_num::<i32>().unwrap(), 3);
    p.unwind(0).unwrap();
    p.finish().unwrap();
}

#[test]
fn walk_differential() {
    for seed in 0..1500 {
        let doc = random_doc(seed);
        let value: Value = serde_json::from_str(&doc).unwrap();
        let mut paths = Vec::new();
        all_paths(&value, String::new(), Vec::new(), &mut paths);

        let mut rbuf = [0u8; 5];
        let mut p = Parser::new(ReadSource::new(doc.as_bytes(), &mut rbuf));
        let mut path_buf = [0u8; 4096];
        let mut seen: Vec<String> = Vec::new();
        let mut ends = 0;
        let mut sbuf = vec![0u8; doc.len()];
        let done = p
            .walk(&mut path_buf, |node| {
                let path = node.path().to_string();
                let expected = value.pointer(&path).unwrap_or_else(|| panic!("seed {seed}: no {path:?}"));
                match node.token() {
                    Token::EndObject | Token::EndArray => {
                        ends += 1;
                        assert!(expected.is_object() || expected.is_array());
                        return Ok(Flow::Continue);
                    }
                    Token::String => {
                        assert_eq!(node.read_str(&mut sbuf)?, expected.as_str().unwrap());
                    }
                    Token::Number => {
                        let n: Value = serde_json::from_str(node.read_number_str(&mut sbuf)?).unwrap();
                        assert_eq!(&n, expected);
                    }
                    Token::Bool => assert_eq!(Some(node.read_bool()?), expected.as_bool()),
                    Token::Null => assert!(expected.is_null()), // left unread: skipped by the walk
                    Token::BeginObject => assert!(expected.is_object()),
                    Token::BeginArray => assert!(expected.is_array()),
                    _ => unreachable!(),
                }
                seen.push(path);
                Ok(Flow::Continue)
            })
            .unwrap();
        assert!(done);
        p.finish().unwrap();
        let mut expected: Vec<String> = paths.into_iter().map(|(p, _, _)| p).collect();
        let containers = value_containers(&value);
        expected.sort();
        seen.sort();
        assert_eq!(seen, expected, "seed {seed}\n{doc}");
        assert_eq!(ends, containers);
    }
}

fn value_containers(v: &Value) -> usize {
    match v {
        Value::Array(a) => 1 + a.iter().map(value_containers).sum::<usize>(),
        Value::Object(m) => 1 + m.values().map(value_containers).sum::<usize>(),
        _ => 0,
    }
}

#[test]
fn walk_skip_stop_and_spans() {
    let doc = br#"{"skip": {"deep": [1, 2]}, "arr": [10, [20, 21], 30], "k": "v", "after": 1}"#;
    let mut p = Parser::from_slice(doc);
    let mut buf = [0u8; 64];
    let mut log = Vec::new();
    let finished = p
        .walk(&mut buf, |node| {
            log.push(format!("{} {:?}", node.path(), node.token()));
            Ok(match node.path() {
                "/skip" => Flow::Skip,
                "/k" => Flow::Stop,
                "/arr/1" => {
                    let span = node.span()?;
                    assert_eq!(&doc[span.range().unwrap()], b"[20, 21]");
                    Flow::Continue
                }
                _ => Flow::Continue,
            })
        })
        .unwrap();
    assert!(!finished);
    assert_eq!(
        log,
        [
            " BeginObject",
            "/skip BeginObject",
            "/arr BeginArray",
            "/arr/0 Number",
            "/arr/1 BeginArray",
            "/arr/2 Number",
            "/arr EndArray",
            "/k String",
        ]
    );
    // Stopped right before "v": continue with the cursor API.
    assert_eq!(p.read_str(&mut buf).unwrap(), "v");
    assert!(p.find_key("after").unwrap());
}

#[test]
fn walk_take_parser_and_relative_start() {
    let doc = br#"{"cfg": {"list": [1, 2, 3], "name": "x"}, "other": 5}"#;
    let mut p = Parser::from_slice(doc);
    assert!(p.seek("/cfg").unwrap());
    let mut buf = [0u8; 32];
    let mut sum = 0;
    let mut paths = Vec::new();
    p.walk(&mut buf, |node| {
        paths.push(node.path().to_string());
        if node.path() == "/list" {
            // Consume the whole array with the cursor API.
            let p = node.take_parser()?;
            p.begin_array()?;
            while p.has_next()? {
                sum += p.read_num::<i32>()?;
            }
            p.end_array()?;
        }
        Ok(Flow::Continue)
    })
    .unwrap();
    assert_eq!(sum, 6);
    assert_eq!(paths, ["", "/list", "/name", ""]);
    assert!(p.find_key("other").unwrap());
    assert_eq!(p.read_num::<i32>().unwrap(), 5);

    // A partially consumed value is completed by the walk.
    let mut p = Parser::from_slice(doc);
    let mut n = 0;
    p.walk(&mut buf, |node| {
        if node.path() == "/cfg" {
            node.take_parser()?.begin_object()?;
        }
        n += 1;
        Ok(Flow::Continue)
    })
    .unwrap();
    assert_eq!(n, 4); // root, /cfg, /other, end of root
    p.finish().unwrap();
}

#[test]
fn walk_path_too_long() {
    let doc = br#"{"a": {"bbbbbbbbbbbbbbbbbbbb": 1}}"#;
    let mut p = Parser::from_slice(doc);
    let mut buf = [0u8; 8];
    let err = p.walk(&mut buf, |_| Ok(Flow::Continue)).unwrap_err();
    assert_eq!(err.kind(), Some(ErrorKind::PathTooLong));

    let doc = format!("[{}]", "0,".repeat(10) + "[1]");
    let mut p = Parser::from_slice(doc.as_bytes());
    let mut buf = [0u8; 3];
    let err = p.walk(&mut buf, |_| Ok(Flow::Continue)).unwrap_err();
    assert_eq!(err.kind(), Some(ErrorKind::PathTooLong));
}

#[test]
fn walk_node_misuse() {
    let mut p = Parser::from_slice(b"[1, 2]");
    let mut buf = [0u8; 8];
    let err = p
        .walk(&mut buf, |node| {
            if node.token() == Token::EndArray {
                node.read_num::<i32>()?;
            }
            Ok(Flow::Continue)
        })
        .unwrap_err();
    assert_eq!(err.kind(), Some(ErrorKind::InvalidState));

    let mut p = Parser::<SliceSource>::from_slice(b"[1, 2]");
    let err = p
        .walk(&mut buf, |node| {
            if node.token() == Token::Number {
                node.read_num::<i32>()?;
                node.read_num::<i32>()?; // already consumed
            }
            Ok(Flow::Continue)
        })
        .unwrap_err();
    assert_eq!(err.kind(), Some(ErrorKind::InvalidState));
}
