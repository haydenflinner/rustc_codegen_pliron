#![no_std]
#![no_main]

use core::panic::PanicInfo;

unsafe extern "C" {
    fn write(fd: i32, buf: *const u8, n: usize) -> isize;
    fn exit(code: i32) -> !;
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    unsafe { exit(101) }
}

struct Pair {
    a: u64,
    b: u32,
}

fn fib(n: u32) -> u64 {
    if n < 2 { n as u64 } else { fib(n - 1) + fib(n - 2) }
}

fn sum(xs: &[Pair]) -> u64 {
    let mut t = 0;
    for p in xs {
        t += p.a * p.b as u64;
    }
    t
}

fn print(s: &[u8]) {
    unsafe { write(1, s.as_ptr(), s.len()) };
}

fn print_num(mut n: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    if n == 0 {
        i -= 1;
        buf[i] = b'0';
    }
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    print(&buf[i..]);
}

#[unsafe(no_mangle)]
pub extern "C" fn main(_argc: i32, _argv: *const *const u8) -> i32 {
    print(b"hello from pliron+cranelift\n");
    print(b"fib(20) = ");
    print_num(fib(20));
    print(b"\n");
    let ps = [Pair { a: 3, b: 4 }, Pair { a: 5, b: 6 }];
    print(b"sum = ");
    print_num(sum(&ps));
    print(b"\n");
    let x: Option<u8> = if fib(3) == 2 { Some(7) } else { None };
    match x {
        Some(v) => print_num(v as u64),
        None => print(b"none"),
    }
    print(b"\n");
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_eh_personality() {}
