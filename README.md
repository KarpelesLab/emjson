# emjson

JSON for embedded systems: a streaming parser, writer and **in-place editor** that
works in a few hundred bytes of RAM, whatever the size of the document.

- `no_std`, no allocator, no `unsafe`, no required dependencies.
- Reads from memory, from any stream, or from random-access storage (flash, SD card,
  files), through a buffer *you* size — 16 bytes works, 512 is plenty.
- Byte-exact positions: find a value, get its exact span, and replace, insert or
  remove it in place, moving the rest of the document only once.
- Edit multi-megabyte files with a 512-byte buffer (see [`examples/big_file.rs`](examples/big_file.rs)).

## Memory and code size

On a 32-bit MCU (Cortex-M4, `opt-level = "s"`, LTO):

| Item                               | RAM                                   |
|------------------------------------|---------------------------------------|
| `Parser` (64 nesting levels)       | 20 bytes + its source                 |
| `ReadSource` (streams)             | 32 bytes + the buffer you give it     |
| `Editor` (in-place edits)          | 20 bytes + the scratch buffer you give it |
| Walk with paths                    | + a buffer for the longest JSON Pointer |

Nesting depth costs one *bit* per level: `Parser::<_, N>::with_stack(src)` uses `N`
bytes for `8 * N` levels.

| Features used                           | Flash    |
|-----------------------------------------|----------|
| `seek` + read string / integer          | ~9 KB    |
| `walk` with paths                       | ~8 KB    |
| `Editor` (replace / set / remove)       | ~16 KB   |
| all of the above                        | ~24 KB   |
| + `f32` parse and format (from `core`)  | +32 KB   |

## Finding a value in a stream

Ask for a path and the parser stops *right before* the value, at its exact offset.
You then decide what to do with it: read it, skip it, measure it, or descend further.

```rust
use emjson::{Parser, Token};
use emjson::io::ReadSource;

// Any `emjson::io::Read` works: UART, flash, a file... Here, a byte slice.
let stream: &[u8] = br#"{"foo": {"bar": "hello world"}}"#;
let mut buf = [0u8; 16];
let mut p = Parser::new(ReadSource::new(stream, &mut buf));

assert!(p.seek(&["foo", "bar"]).unwrap());   // or p.seek("/foo/bar") (JSON Pointer)
assert_eq!(p.peek().unwrap(), Token::String);
assert_eq!(p.offset(), 16);                  // position of the opening quote

let mut s = [0u8; 32];
assert_eq!(p.read_str(&mut s).unwrap(), "hello world");
```

Strings larger than any buffer can be read in chunks with `p.str_reader()`, and
`p.value_span()` skips a value and returns its exact byte range.

## Callbacks with paths

```rust
use emjson::{Flow, Parser};

let json = br#"{"wifi": {"ssid": "home", "channel": 6}, "debug": {"level": 3}}"#;
let mut p = Parser::from_slice(json);
let (mut path, mut ssid) = ([0u8; 64], [0u8; 32]);
let (mut ssid_len, mut channel) = (0, 0u8);

p.walk(&mut path, |node| {
    match node.path() {
        "/wifi/ssid" => ssid_len = node.read_str(&mut ssid)?.len(),
        "/wifi/channel" => channel = node.read_num()?,
        "/debug" => return Ok(Flow::Skip), // not entered
        _ => {}
    }
    Ok(Flow::Continue)
})
.unwrap();
assert_eq!((&ssid[..ssid_len], channel), (&b"home"[..], 6));
```

## Cursor API

```rust
let mut p = emjson::Parser::from_slice(br#"{"id": 7, "tags": ["a", "b"], "on": true}"#);
p.begin_object().unwrap();
while p.has_next().unwrap() {
    let mut key = [0u8; 16];
    match p.read_key(&mut key).unwrap() {
        "id" => assert_eq!(p.read_num::<u8>().unwrap(), 7),
        "on" => assert!(p.read_bool().unwrap()),
        _ => {} // unread values are skipped automatically
    }
}
p.end_object().unwrap();
p.finish().unwrap();
```

Also: `find_key`, `match_key` (compares a key without buffering it), `skip_value`,
`next_event` (classic pull events), `unwind`, and for in-memory documents zero-copy
`read_str_ref` and `raw_value`.

## Writing

```rust
use emjson::{JsonWriter, ToJson, io::{SliceWriter, Write}};

struct Reading { sensor: &'static str, celsius: f32 }

impl ToJson for Reading {
    fn to_json<W: Write>(&self, w: &mut JsonWriter<W>) -> Result<(), W::Error> {
        w.begin_object()?;
        w.member("sensor", self.sensor)?;
        w.member("celsius", &self.celsius)?;
        w.end_object()
    }
}

let r = Reading { sensor: "t1", celsius: 21.5 };
assert_eq!(emjson::encoded_len(&r), 30); // measure without writing
let mut buf = [0u8; 64];
assert_eq!(emjson::to_slice(&r, &mut buf).unwrap(), r#"{"sensor":"t1","celsius":21.5}"#);
```

`JsonWriter` writes to any `emjson::io::Write` without buffering, compact or
pretty-printed. `copy_value` streams a value from a parser to a writer (minify,
pretty-print, extract a sub-document) in constant memory.

## Editing in place

`Editor` works on any `Storage` (a RAM buffer with `MemStorage`, a file, raw flash —
implement four methods). Each edit locates the value, measures the new encoding, moves
the tail of the document once, and writes the new value directly.

```rust
use emjson::edit::{Editor, MemStorage};

let mut buf = [0u8; 128];
let doc = br#"{"foo": {"bar": "hello world"}, "list": [1, 2]}"#;
buf[..doc.len()].copy_from_slice(doc);

let mut scratch = [0u8; 32];
let mut ed = Editor::new(MemStorage::new(&mut buf, doc.len()), &mut scratch);
ed.replace(&["foo", "bar"], "hi").unwrap();
ed.set("/foo/baz", &[true, false]).unwrap();   // creates the member
ed.push("/list", &3).unwrap();
ed.remove("/list/0").unwrap();                 // with its comma
assert_eq!(
    ed.storage().as_bytes(),
    br#"{"foo": {"bar": "hi","baz":[true,false]}, "list": [2,3]}"#
);
```

The building blocks are public: `Parser::value_span` gives the exact range of a value,
`encoded_len` the size of its replacement, and `Editor::splice` / `edit::apply` perform
the move-and-write. `edit::plan` computes a `Patch` without applying it, e.g. to check
that it fits first.

## Editing while copying a stream

When the document cannot be modified in place (read-only source, or you want an atomic
file swap), `copy_edit` applies the change in a single pass while copying:

```rust
use emjson::edit::{copy_edit, Op};
use emjson::io::{ReadSource, SliceWriter};

let input = br#"{"foo": {"bar": "hello world"}, "n": 1}"#;
let (mut rbuf, mut out) = ([0u8; 8], [0u8; 64]);
let mut dst = SliceWriter::new(&mut out);
copy_edit(ReadSource::new(&input[..], &mut rbuf), &mut dst, Op::Replace, "/foo/bar", "bye").unwrap();
assert_eq!(dst.written(), br#"{"foo": {"bar": "bye"}, "n": 1}"#);
```

Removal needs to look ahead, so it is done in two passes: `plan_remove` on one read
of the source, then `apply_copy` on a second one.

## Paths

- `&["foo", "bar", "0"]` — member names (a numeric name also indexes arrays).
- `"/foo/bar/0"` — JSON Pointer (RFC 6901), with `~1` for `/` and `~0` for `~`.
- `emjson::path!["foo", 0]` / `&[Seg::Key("foo"), Seg::Index(0)]`.
- `-` designates the end of an array for `Op::Set` / `Op::Insert`.

## I/O

- `Source`: what the parser reads (like `BufRead`). Implemented by `SliceSource`,
  `ReadSource` (any `Read` + your buffer), `edit::StorageSource` and `io::Tee`.
- `Read` / `Write`: minimal traits; enable the `std` feature for `io::StdIo` adapters
  (and `Storage` for `std::fs::File`), or `embedded-io` for `io::EmbeddedIo`.

## Correctness

Strict RFC 8259: UTF-8 is validated, escapes (including surrogate pairs) are checked,
and number grammar is enforced. The test suite compares emjson against `serde_json`
on thousands of random documents (read through buffers as small as 1 byte),
mutation-fuzzes invalid input, and checks every edit operation against the same edit
done on a `serde_json::Value`.

## Minimum Rust version

1.89 (edition 2024).

## License

MIT
