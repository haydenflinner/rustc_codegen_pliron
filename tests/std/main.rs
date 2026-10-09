use std::collections::HashMap;

fn main() {
    println!("hello, std, from pliron+cranelift");
    let v: Vec<u32> = (1..=10).map(|x| x * x).collect();
    println!("squares sum = {}", v.iter().sum::<u32>());
    let mut m = HashMap::new();
    for w in "a b a c b a".split(' ') {
        *m.entry(w).or_insert(0) += 1;
    }
    let mut kv: Vec<_> = m.into_iter().collect();
    kv.sort();
    println!("{kv:?}");
    let s = format!("{:.3} {}", 1.5f64.sqrt(), String::from("owned"));
    println!("{s}");
    let r = std::panic::catch_unwind(|| 1 + 1);
    println!("catch_unwind ok = {:?}", r.is_ok());
    #[cfg(unix)]
    {
        // Foreign C-variadic call: float and sub-64-bit args go on the
        // stack on aarch64-darwin (8-byte slots), in regs/stack on SysV.
        unsafe extern "C" {
            fn snprintf(buf: *mut u8, n: usize, fmt: *const u8, ...) -> i32;
        }
        let mut buf = [0u8; 64];
        unsafe {
            snprintf(
                buf.as_mut_ptr(),
                buf.len(),
                b"%d|%f|%c|%ld\0".as_ptr(),
                42i32,
                2.5f64,
                65u8 as i32,
                -7i64,
            );
        }
        let s = std::ffi::CStr::from_bytes_until_nul(&buf).unwrap();
        println!("snprintf = {}", s.to_str().unwrap());
    }
}
