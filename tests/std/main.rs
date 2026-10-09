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

    // Loop-versioning bounds-check hoisting: in-bounds fast paths must be
    // correct, and the slow path must panic at the exact OOB index.
    {
        #[inline(never)]
        fn copy1(a: &[u32], dst: &mut [u32], n: usize) {
            for i in 0..n {
                dst[i] = a[i] + 1;
            }
        }
        #[inline(never)]
        fn incl(a: &[u32], dst: &mut [u32], n: usize) {
            for i in 0..=n {
                dst[i] = a[i];
            }
        }
        #[inline(never)]
        fn strided(a: &[u32], dst: &mut [u32], n: usize) {
            for i in 0..n {
                dst[i * 2] = a[i];
            }
        }
        let a = vec![7u32; 100];
        let mut d = vec![0u32; 100];
        copy1(&a, &mut d, 50);
        assert_eq!(d[49], 8);
        assert_eq!(d[60], 0);
        copy1(&a, &mut d, 0);
        copy1(&a, &mut d, 100);
        assert_eq!(d[99], 8);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            copy1(&a, &mut d, 200)
        }));
        assert!(r.is_err());
        incl(&a, &mut d, 99);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            incl(&a, &mut d, 100)
        }));
        assert!(r.is_err());
        let mut d2 = vec![0u32; 100];
        strided(&a, &mut d2, 50);
        assert_eq!(d2[98], 7);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut d3 = vec![0u32; 100];
            strided(&a, &mut d3, 60)
        }));
        assert!(r.is_err());
        println!("bcheck ok");
    }
}
