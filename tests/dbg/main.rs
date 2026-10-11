fn main() {
    let x = 1.0f64;
    let b = x.to_bits();
    println!("bits={b:x}");
    let e = (b >> 52) & 0x7ff;
    println!("exp={e}");
}
