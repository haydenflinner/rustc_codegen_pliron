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
}
