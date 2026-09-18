//! Edits a large JSON file on disk with a few hundred bytes of buffers.
//!
//! Run with: `cargo run --release --example big_file --features std [size_mb]`

use std::fs::{File, OpenOptions};
use std::io::BufWriter;
use std::time::Instant;

use emjson::edit::{Editor, Op, copy_edit};
use emjson::io::{ReadSource, StdIo};
use emjson::{JsonWriter, Parser, validate};

const BUF: usize = 512;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let size_mb: u64 = std::env::args().nth(1).map_or(Ok(50), |s| s.parse())?;
    let dir = std::env::temp_dir().join(format!("emjson-example-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("big.json");

    // 1. Generate the file with the streaming writer.
    let t = Instant::now();
    {
        let mut w = JsonWriter::pretty(StdIo(BufWriter::new(File::create(&path)?)), 2);
        w.begin_object()?;
        w.key("records")?;
        w.begin_array()?;
        let mut i = 0u64;
        while w.get_ref().0.get_ref().metadata()?.len() < size_mb << 20 {
            for _ in 0..1000 {
                w.begin_object()?;
                w.member("id", &i)?;
                w.member("name", "sensor \"north\" \u{e9}")?;
                w.member("values", &[1.5, -2.25, 1e-9])?;
                w.member("active", &i.is_multiple_of(3))?;
                w.end_object()?;
                i += 1;
            }
        }
        w.end_array()?;
        w.key("config")?;
        w.begin_object()?;
        w.member("name", "device")?;
        w.member("version", &1)?;
        w.end_object()?;
        w.end_object()?;
        w.get_mut().0.get_mut().sync_all()?;
    }
    let len = std::fs::metadata(&path)?.len();
    println!("generated {:.1} MB in {:.2?}", len as f64 / 1e6, t.elapsed());

    // 2. Stream search: the value is at the very end of the file.
    let t = Instant::now();
    let mut buf = [0u8; BUF];
    let mut p = Parser::new(ReadSource::new(StdIo(File::open(&path)?), &mut buf));
    assert!(p.seek("/config/version")?);
    let offset = p.offset();
    let version: u32 = p.read_num()?;
    let el = t.elapsed();
    println!(
        "seek /config/version = {version} at offset {offset}: {el:.2?} ({:.0} MB/s), parser {} bytes + {BUF} byte buffer",
        len as f64 / 1e6 / el.as_secs_f64(),
        size_of::<Parser<emjson::SliceSource>>() - size_of::<emjson::SliceSource>(),
    );

    // 3. In-place edits through a 512-byte scratch buffer.
    let t = Instant::now();
    let file = OpenOptions::new().read(true).write(true).open(&path)?;
    let mut scratch = [0u8; BUF];
    let mut ed = Editor::new(file, &mut scratch);
    ed.replace("/config/name", "a much longer device name")?; // grows: moves nothing (at the end)
    ed.set("/config/serial", "SN-0001")?;
    ed.replace("/records/0/name", "first")?; // shrinks: moves the whole file tail
    let el = t.elapsed();
    println!("3 in-place edits (one moving ~{:.0} MB): {el:.2?}", len as f64 / 1e6);

    // 4. Single-pass copy with an edit, to a new file.
    let t = Instant::now();
    let copy = dir.join("copy.json");
    let mut buf = [0u8; BUF];
    let src = ReadSource::new(StdIo(File::open(&path)?), &mut buf);
    let dst = StdIo(BufWriter::with_capacity(BUF, File::create(&copy)?));
    copy_edit(src, dst, Op::Replace, "/config/version", &2)?;
    let el = t.elapsed();
    println!("copy with edit: {el:.2?} ({:.0} MB/s)", len as f64 / 1e6 / el.as_secs_f64());

    // 5. Check the results.
    let t = Instant::now();
    let mut buf = [0u8; BUF];
    validate(ReadSource::new(StdIo(File::open(&copy)?), &mut buf))?;
    let el = t.elapsed();
    println!("validate: {el:.2?} ({:.0} MB/s)", len as f64 / 1e6 / el.as_secs_f64());
    let mut buf = [0u8; BUF];
    let mut p = Parser::new(ReadSource::new(StdIo(File::open(&copy)?), &mut buf));
    assert!(p.seek(&["config"])?);
    let mut out = [0u8; 256];
    let mut w = JsonWriter::new(emjson::io::SliceWriter::new(&mut out));
    emjson::copy_value(&mut p, &mut w)?;
    println!("config: {}", std::str::from_utf8(w.get_ref().written())?);

    std::fs::remove_dir_all(&dir)?;
    Ok(())
}
