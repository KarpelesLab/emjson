mod common;

use common::*;
use emjson::edit::{CapacityError, Editor, MemStorage, Op, Storage, apply_copy, copy_edit, plan, plan_remove};
use emjson::io::{ReadSource, Write};
use emjson::{Error, Parser, RawJson, SliceSource, Span};
use serde_json::Value;
use std::convert::Infallible;

struct VecWriter(Vec<u8>);

impl Write for VecWriter {
    type Error = Infallible;
    fn write_all(&mut self, data: &[u8]) -> Result<(), Infallible> {
        self.0.extend_from_slice(data);
        Ok(())
    }
}

/// Storage using the default (chunked) `move_within`, like flash or a file would.
struct VecStorage(Vec<u8>);

impl Storage for VecStorage {
    type Error = Infallible;
    fn len(&mut self) -> Result<u64, Infallible> {
        Ok(self.0.len() as u64)
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), Infallible> {
        let o = offset as usize;
        buf.copy_from_slice(&self.0[o..o + buf.len()]);
        Ok(())
    }
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), Infallible> {
        let o = offset as usize;
        assert!(o + data.len() <= self.0.len(), "write outside the document");
        self.0[o..o + data.len()].copy_from_slice(data);
        Ok(())
    }
    fn set_len(&mut self, len: u64) -> Result<(), Infallible> {
        self.0.resize(len as usize, 0xAA);
        Ok(())
    }
}

/// All paths of a value as (pointer, parent pointer, last segment, is_index).
fn paths(v: &Value, ptr: &str, out: &mut Vec<(String, String, String, bool)>) {
    match v {
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                let p = format!("{ptr}/{i}");
                out.push((p.clone(), ptr.to_string(), i.to_string(), true));
                paths(x, &p, out);
            }
        }
        Value::Object(m) => {
            for (k, x) in m {
                let p = format!("{ptr}/{}", k.replace('~', "~0").replace('/', "~1"));
                out.push((p.clone(), ptr.to_string(), k.clone(), false));
                paths(x, &p, out);
            }
        }
        _ => {}
    }
}

/// Applies `op` with the in-place editor (two storage kinds, various scratch sizes), the
/// single-pass copy and the two-pass copy; checks they agree and returns the result.
fn edit_all_ways(doc: &str, op: Option<Op>, path: &str, new: &str, scratch_size: usize) -> Option<Vec<u8>> {
    let value = RawJson(new);
    // In place, MemStorage.
    let mut buf = vec![0u8; doc.len() + new.len() + 64];
    buf[..doc.len()].copy_from_slice(doc.as_bytes());
    let mut scratch = vec![0u8; scratch_size];
    let mut ed = Editor::new(MemStorage::new(&mut buf, doc.len()), &mut scratch);
    let applied = match op {
        Some(op) => ed.edit(op, path, &value).unwrap(),
        None => ed.remove(path).unwrap(),
    };
    let result = ed.storage().as_bytes().to_vec();
    if !applied {
        assert_eq!(result, doc.as_bytes());
    }

    // In place, chunked moves.
    let mut scratch = vec![0u8; scratch_size];
    let mut ed = Editor::new(VecStorage(doc.as_bytes().to_vec()), &mut scratch);
    let applied2 = match op {
        Some(op) => ed.edit(op, path, &value).unwrap(),
        None => ed.remove(path).unwrap(),
    };
    assert_eq!(applied, applied2);
    assert_eq!(ed.into_inner().0, result, "chunked storage differs for {op:?} {path}");

    // Two passes: plan on one source, copy from another.
    let mut p = Parser::from_slice(doc.as_bytes());
    let patch = match op {
        Some(op) => plan(&mut p, op, path).unwrap(),
        None => plan_remove(&mut p, path).unwrap(),
    };
    assert_eq!(patch.is_some(), applied);
    if let Some(patch) = patch {
        let mut out = VecWriter(Vec::new());
        apply_copy(SliceSource::new(doc.as_bytes()), &mut out, &patch, &value).unwrap();
        assert_eq!(out.0, result, "apply_copy differs for {op:?} {path}");
        assert_eq!(result.len() as i64 - doc.len() as i64, patch.delta(&value));
    }

    // Single pass through a tiny buffer.
    if let Some(op) = op {
        let mut rbuf = [0u8; 3];
        let mut out = VecWriter(Vec::new());
        let found = copy_edit(ReadSource::new(doc.as_bytes(), &mut rbuf), &mut out, op, path, &value).unwrap();
        assert_eq!(found, applied);
        assert_eq!(out.0, result, "copy_edit differs for {op:?} {path}");
    }
    applied.then_some(result)
}

fn parse(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap_or_else(|e| panic!("invalid result {e}: {}", String::from_utf8_lossy(bytes)))
}

#[test]
fn random_edits() {
    let mut rng = Rng::new(5);
    let mut counts = [0usize; 5];
    for seed in 0..1200u64 {
        let doc = random_doc(seed);
        let value: Value = serde_json::from_str(&doc).unwrap();
        let new_doc = random_doc(seed + 100_000);
        let new = new_doc.trim_matches([' ', '\n', '\t', '\r']);
        let new_value: Value = serde_json::from_str(new).unwrap();
        let mut all = Vec::new();
        paths(&value, "", &mut all);
        let scratch = *rng.pick(&[1usize, 2, 5, 64]);

        // Replace the root.
        let out = edit_all_ways(&doc, Some(Op::Replace), "", new, scratch).unwrap();
        assert_eq!(parse(&out), new_value);

        for (ptr, parent, last, is_index) in all.iter().take(12) {
            // Replace.
            let out = edit_all_ways(&doc, Some(Op::Replace), ptr, new, scratch).unwrap();
            let mut expected = value.clone();
            *expected.pointer_mut(ptr).unwrap() = new_value.clone();
            assert_eq!(parse(&out), expected, "replace {ptr}\n{doc}");
            counts[0] += 1;

            // Set on an existing path is a replace.
            let out = edit_all_ways(&doc, Some(Op::Set), ptr, new, scratch).unwrap();
            assert_eq!(parse(&out), expected);

            // Remove.
            let out = edit_all_ways(&doc, None, ptr, "", scratch).unwrap();
            let mut expected = value.clone();
            match expected.pointer_mut(parent).unwrap() {
                Value::Array(a) => {
                    a.remove(last.parse::<usize>().unwrap());
                }
                Value::Object(m) => {
                    m.remove(last);
                }
                _ => unreachable!(),
            }
            assert!(!is_index || expected.pointer(parent).unwrap().is_array());
            assert_eq!(parse(&out), expected, "remove {ptr}\n{doc}");
            counts[1] += 1;
        }

        // Create members / elements in every container.
        let mut containers = vec![String::new()];
        containers.extend(all.iter().map(|(p, ..)| p.clone()));
        for ptr in containers.iter().take(8) {
            let target = value.pointer(ptr).unwrap();
            match target {
                Value::Object(m) => {
                    let key = "new~/key";
                    let new_ptr = format!("{ptr}/new~0~1key");
                    let out = edit_all_ways(&doc, Some(Op::Set), &new_ptr, new, scratch).unwrap();
                    let mut expected = value.clone();
                    expected.pointer_mut(ptr).unwrap().as_object_mut().unwrap().insert(key.into(), new_value.clone());
                    assert_eq!(parse(&out), expected, "set {new_ptr}\n{doc}");
                    assert!(!m.contains_key(key));
                    // Push only works on arrays.
                    assert!(edit_all_ways(&doc, Some(Op::Push), ptr, new, scratch).is_none());
                    counts[2] += 1;
                }
                Value::Array(a) => {
                    // Push, and set/insert at the end ("-" or the length).
                    let mut expected = value.clone();
                    expected.pointer_mut(ptr).unwrap().as_array_mut().unwrap().push(new_value.clone());
                    let pushed = edit_all_ways(&doc, Some(Op::Push), ptr, new, scratch).unwrap();
                    assert_eq!(parse(&pushed), expected, "push {ptr}\n{doc}");
                    for last in [String::from("-"), a.len().to_string()] {
                        for op in [Op::Set, Op::Insert] {
                            let out = edit_all_ways(&doc, Some(op), &format!("{ptr}/{last}"), new, scratch).unwrap();
                            assert_eq!(out, pushed);
                        }
                    }
                    // Out of range.
                    let beyond = format!("{ptr}/{}", a.len() + 1);
                    assert!(edit_all_ways(&doc, Some(Op::Set), &beyond, new, scratch).is_none());
                    // Insert before each element.
                    for i in 0..a.len() {
                        let out = edit_all_ways(&doc, Some(Op::Insert), &format!("{ptr}/{i}"), new, scratch).unwrap();
                        let mut expected = value.clone();
                        expected.pointer_mut(ptr).unwrap().as_array_mut().unwrap().insert(i, new_value.clone());
                        assert_eq!(parse(&out), expected, "insert {ptr}/{i}\n{doc}");
                        counts[3] += 1;
                    }
                }
                _ => {
                    // Not a container: nothing can be created below it.
                    assert!(edit_all_ways(&doc, Some(Op::Set), &format!("{ptr}/x"), new, scratch).is_none());
                    counts[4] += 1;
                }
            }
        }
        // Missing paths.
        if value.pointer("/zz/top").is_none() {
            assert!(edit_all_ways(&doc, Some(Op::Replace), "/zz/top", new, scratch).is_none());
            assert!(edit_all_ways(&doc, Some(Op::Set), "/zz/top", new, scratch).is_none());
            assert!(edit_all_ways(&doc, None, "/zz/top", "", scratch).is_none());
        }
        assert!(edit_all_ways(&doc, None, "", "", scratch).is_none(), "the root cannot be removed");
    }
    assert!(counts.iter().all(|&c| c > 300), "{counts:?}");
}

#[test]
fn readme_flow() {
    // Locate, measure, move, overwrite: the manual version of Editor::replace.
    let doc = br#"{"config": {"name": "old", "items": [1, 2, 3]}, "tail": "unchanged"}"#;
    let mut buf = [0u8; 128];
    buf[..doc.len()].copy_from_slice(doc);
    let mut scratch = [0u8; 16];
    let mut ed = Editor::new(MemStorage::new(&mut buf, doc.len()), &mut scratch);

    let span = ed.locate("/config/name").unwrap().unwrap();
    assert_eq!(span, Span { start: 20, end: 25 });
    assert_eq!(emjson::encoded_len("a much longer name"), 20);
    let new_span = ed.splice(span, "a much longer name").unwrap();
    assert_eq!(new_span, Span { start: 20, end: 40 });
    assert_eq!(
        ed.storage().as_bytes(),
        br#"{"config": {"name": "a much longer name", "items": [1, 2, 3]}, "tail": "unchanged"}"#
    );
    assert!(ed.replace("/config/items", &[9u8; 0]).unwrap());
    assert!(ed.remove("/tail").unwrap());
    assert!(ed.set("/config/count", &0).unwrap());
    assert_eq!(ed.storage().as_bytes(), br#"{"config": {"name": "a much longer name", "items": [],"count":0}}"#);
    assert!(!ed.replace("/nope", &1).unwrap());
    assert!(!ed.remove("/config/nope").unwrap());
}

#[test]
fn removal_keeps_formatting() {
    let doc = "{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3\n}";
    let run = |path: &str| {
        let mut buf = doc.as_bytes().to_vec();
        let mut scratch = [0u8; 4];
        let mut ed = Editor::new(MemStorage::new(&mut buf, doc.len()), &mut scratch);
        assert!(ed.remove(path).unwrap());
        String::from_utf8(ed.storage().as_bytes().to_vec()).unwrap()
    };
    assert_eq!(run("/a"), "{\n  \"b\": 2,\n  \"c\": 3\n}");
    assert_eq!(run("/b"), "{\n  \"a\": 1,\n  \"c\": 3\n}");
    assert_eq!(run("/c"), "{\n  \"a\": 1,\n  \"b\": 2\n}");
    let doc = "[ 1 ]";
    let mut buf = doc.as_bytes().to_vec();
    let mut scratch = [0u8; 4];
    let mut ed = Editor::new(MemStorage::new(&mut buf, doc.len()), &mut scratch);
    assert!(ed.remove("/0").unwrap());
    assert_eq!(ed.storage().as_bytes(), b"[  ]");
}

#[test]
fn capacity_error_leaves_document_intact() {
    let doc = br#"{"a": "short"}"#;
    let mut buf = *doc;
    let mut scratch = [0u8; 8];
    let mut ed = Editor::new(MemStorage::new(&mut buf, doc.len()), &mut scratch);
    let err = ed.replace("/a", "much longer value").unwrap_err();
    assert_eq!(err, Error::Io(CapacityError));
    assert_eq!(ed.storage().as_bytes(), doc);
    // Shrinking works without spare room.
    assert!(ed.replace("/a", "s").unwrap());
    assert_eq!(ed.storage().as_bytes(), br#"{"a": "s"}"#);
}

#[test]
fn copy_edit_not_found_is_identity() {
    let doc = br#"{"a": [1, 2, {"b": null}]}"#;
    let mut rbuf = [0u8; 2];
    let mut out = VecWriter(Vec::new());
    assert!(!copy_edit(ReadSource::new(&doc[..], &mut rbuf), &mut out, Op::Replace, "/a/5", &1).unwrap());
    assert_eq!(out.0, doc);
}

#[test]
fn stream_edit_errors_on_invalid_json() {
    let doc = br#"{"a": [1, 2,, 3], "b": 1}"#;
    let mut out = VecWriter(Vec::new());
    assert!(copy_edit(SliceSource::new(doc), &mut out, Op::Replace, "/b", &2).is_err());
}

#[test]
fn deep_documents_use_wider_stacks() {
    let depth = 100;
    let doc = "[".repeat(depth) + "0" + &"]".repeat(depth);
    let path = "/0".repeat(depth);
    let mut buf = doc.clone().into_bytes();
    buf.resize(doc.len() + 8, 0);
    let mut scratch = [0u8; 8];
    let mut ed = Editor::new(MemStorage::new(&mut buf, doc.len()), &mut scratch);
    assert!(ed.replace(path.as_str(), &1).is_err()); // 64 levels by default
    let mut ed = Editor::<_, 16>::with_stack(MemStorage::new(&mut buf, doc.len()), &mut scratch);
    assert!(ed.replace(path.as_str(), &12345).unwrap());
    assert_eq!(ed.storage().as_bytes(), ("[".repeat(depth) + "12345" + &"]".repeat(depth)).as_bytes());
}

#[cfg(feature = "std")]
#[test]
fn file_storage() {
    use std::io::{Read, Seek, SeekFrom};
    let dir = std::env::temp_dir().join(format!("emjson-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("doc.json");
    std::fs::write(&path, br#"{"list": [1, 2, 3], "name": "file"}"#).unwrap();
    let file = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
    let mut scratch = [0u8; 5];
    let mut ed = Editor::new(file, &mut scratch);
    assert!(ed.replace("/name", "a longer name").unwrap());
    assert!(ed.remove("/list/0").unwrap());
    assert!(ed.push("/list", &4).unwrap());
    assert!(ed.replace("/name", "x").unwrap());
    let mut file = ed.into_inner();
    let mut s = String::new();
    file.seek(SeekFrom::Start(0)).unwrap();
    file.read_to_string(&mut s).unwrap();
    assert_eq!(s, r#"{"list": [2, 3,4], "name": "x"}"#);
    std::fs::remove_dir_all(&dir).unwrap();
}
