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
        "libm",
        (x.exp() - std::f64::consts::E).abs() < 1e-12 && (x.atan2(x) - std::f64::consts::FRAC_PI_4).abs() < 1e-12,
    );
    eprintln!("wasi std OK");
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
