//! std on wasm32-wasip1 with tools/pliron-wasi-libc instead of wasi-libc.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};

fn check(name: &str, ok: bool) {
    println!("{} {name}", if ok { "ok  " } else { "FAIL" });
    if !ok {
        std::process::exit(1);
    }
}

fn main() {
    let dir = std::env::var("WASI_DIR").unwrap();
    let args: Vec<String> = std::env::args().collect();
    check("args", args == ["wasitest", "a1"]);
    check("env", std::env::var("HELLO").as_deref() == Ok("world"));
    let d = format!("{dir}/sub");
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(format!("{d}/nested")).unwrap();
    fs::write(format!("{d}/a.txt"), "hello wasi").unwrap();
    check(
        "read_to_string",
        fs::read_to_string(format!("{d}/a.txt")).unwrap() == "hello wasi",
    );
    let mut f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!("{d}/a.txt"))
        .unwrap();
    f.seek(SeekFrom::Start(6)).unwrap();
    f.write_all(b"WASI").unwrap();
    f.seek(SeekFrom::Start(0)).unwrap();
    let mut s = String::new();
    f.read_to_string(&mut s).unwrap();
    check("seek+write", s == "hello WASI");
    check("metadata", f.metadata().unwrap().len() == 10);
    drop(f);
    fs::rename(format!("{d}/a.txt"), format!("{d}/nested/b.txt")).unwrap();
    let mut names: Vec<String> = fs::read_dir(&d)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    check("read_dir+rename", names == ["nested"]);
    check("readdir align", readdir_aligned(&d));
    check(
        "exists",
        fs::metadata(format!("{d}/nested/b.txt")).unwrap().is_file(),
    );
    check("missing", fs::metadata(format!("{d}/nope")).is_err());
    fs::remove_dir_all(&d).unwrap();
    check("remove_dir_all", fs::metadata(&d).is_err());
    let t = std::time::Instant::now();
    std::thread::sleep(std::time::Duration::from_millis(5));
    check("sleep+instant", t.elapsed().as_millis() >= 5);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    check("systemtime", now.as_secs() > 1_700_000_000);
    let x = std::hint::black_box(1.0f64);
    check(
        "variadic",
        unsafe { vsum(3, 10i32, 20i64, 30.5f64) == 60.5 },
    );
    // Consecutive i32/u32 args: the wasm C ABI packs varargs in 4-byte
    // slots at natural alignment, not SysV 8-byte slots — a stride-8 bug
    // shows up as misaligned reads here. The i64 after three i32s
    // exercises >4-alignment padding (offset 16, not 12/24).
    check(
        "variadic slots",
        unsafe { vpack(6, 1i32, 2u32, 3i32, 4i64, 5i32, 6.5f64) == 21.5 },
    );
    check(
        "libm",
        (x.exp() - std::f64::consts::E).abs() < 1e-12 && (x.atan2(x) - std::f64::consts::FRAC_PI_4).abs() < 1e-12,
    );
    // wpeep regression: str::lt compares a prefix then falls back to the
    // length difference; a deleted `get l; set l` self-copy hid the write
    // carrying that difference out of the branch, and the copy-forward
    // pass overwrote it with the memcmp result (lt returned false).
    {
        let c = std::hint::black_box("cccc");
        let cc = std::hint::black_box("ccccc");
        check("str lt", c < cc && !(cc < c) && c != cc && cc > c);
        let b = std::hint::black_box(b"abcd".as_slice());
        let bb = std::hint::black_box(b"abcde".as_slice());
        check("slice lt", b < bb && bb > b);
    }
    // localify steal regression (pulldown-cmark miscompile): the
    // br_table below sends two edges to the loop-header block with
    // different arg tuples and a third to the `)` handler, which still
    // reads the pre-increment `i` on its exit edge. Letting the shared
    // `i+1` arg steal `i`'s local clobbers the value the `)` exit reads
    // (scan returned i+1 on break).
    check(
        "br_table edge args",
        scan(std::hint::black_box(b"(x))y".as_slice())) == (3, 0),
    );
    check(
        "br_table edge args 2",
        scan(std::hint::black_box(b"(x)".as_slice())) == (3, 0)
            && scan(std::hint::black_box(b"u) b".as_slice())) == (1, 0),
    );
    eprintln!("wasi std OK");
}

#[inline(never)]
fn scan(bytes: &[u8]) -> (usize, u32) {
    let mut i = 0usize;
    let mut nest = 0u32;
    while i < bytes.len() {
        match bytes[i] {
            0x0..=0x20 => break,
            b'(' => nest += 1,
            b')' => {
                if nest == 0 {
                    break;
                }
                nest -= 1;
            }
            b'\\' if i + 1 < bytes.len() && bytes[i + 1].is_ascii_punctuation() => i += 1,
            _ => {}
        }
        i += 1;
    }
    (i, nest)
}

unsafe extern "C" fn vsum(n: i32, mut ap: ...) -> f64 {
    let mut s = 0.0;
    for i in 0..n {
        s += unsafe {
            match i {
                0 => ap.next_arg::<i32>() as f64,
                1 => ap.next_arg::<i64>() as f64,
                _ => ap.next_arg::<f64>(),
            }
        };
    }
    s
}

unsafe extern "C" fn vpack(n: i32, mut ap: ...) -> f64 {
    let mut s = 0.0;
    for i in 0..n {
        s += unsafe {
            match i {
                0 => ap.next_arg::<i32>() as f64,
                1 => ap.next_arg::<u32>() as f64,
                2 | 4 => ap.next_arg::<i32>() as f64,
                3 => ap.next_arg::<i64>() as f64,
                _ => ap.next_arg::<f64>(),
            }
        };
    }
    s
}

// std dereferences the returned `dirent` directly, so it must be suitably aligned.
fn readdir_aligned(dir: &str) -> bool {
    use std::ffi::{CString, c_char, c_void};
    unsafe extern "C" {
        fn opendir(p: *const c_char) -> *mut c_void;
        fn readdir(d: *mut c_void) -> *mut u8;
        fn closedir(d: *mut c_void) -> i32;
    }
    let p = CString::new(dir).unwrap();
    unsafe {
        let d = opendir(p.as_ptr());
        if d.is_null() {
            return false;
        }
        let mut ok = true;
        loop {
            let e = readdir(d);
            if e.is_null() {
                break;
            }
            ok &= e as usize % 8 == 0;
        }
        closedir(d);
        ok
    }
}
