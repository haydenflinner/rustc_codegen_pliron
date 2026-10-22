// Fat LTO (-Clto) across two rlibs: main -> mid -> base, plus std.
// test.sh builds it with and without -Clto and diffs against expected.out.
use std::panic;
use std::sync::atomic::Ordering;

unsafe extern "C" {
    fn lto_base_answer(x: u32) -> u32;
}

fn main() {
    panic::set_hook(Box::new(|i| {
        let p = i.payload();
        let m = p
            .downcast_ref::<String>()
            .map(|s| s.as_str())
            .or_else(|| p.downcast_ref::<&str>().copied())
            .unwrap_or("?");
        println!("hook: {m}");
    }));

    // #[used] static initializer in an upstream crate ran before main.
    println!("ctor ran = {}", base::CTOR_RAN.load(Ordering::SeqCst));

    // Generics instantiated across the crate graph.
    println!("sums = {:?}", mid::sums());
    println!("total u16 = {}", base::total(&[60000u16, 6000, 600]));

    // Trait objects built in mid, vtables from base and mid.
    for s in mid::shapes() {
        println!("{} -> {}", s.name(), s.area());
    }

    // std containers through an upstream crate.
    let h = mid::histogram("the quick brown fox jumps over The lazy dog the end");
    println!("histogram = {h:?}");

    // #[no_mangle] extern "C" in an upstream crate.
    println!("answer = {}", unsafe { lto_base_answer(std::hint::black_box(12345)) });

    // A panic raised upstream unwinds into this crate.
    println!("checked(2) = {:?}", panic::catch_unwind(|| base::checked(2)));
    let r = panic::catch_unwind(|| base::checked(std::hint::black_box(7)));
    println!("checked(7) caught = {}", r.is_err());

    // A panic raised here unwinds through two upstream crates' frames
    // (drops run, the thread-local depth is restored).
    let r = panic::catch_unwind(|| {
        mid::nested(|d| {
            println!("depth inside = {} (arg {d})", base::depth());
            if std::hint::black_box(d) == 12 {
                panic!("main panic at {d}");
            }
            d
        })
    });
    println!("nested caught = {}, depth after = {}", r.is_err(), base::depth());
    println!("nested ok = {}", mid::nested(|d| d + 1));
}
