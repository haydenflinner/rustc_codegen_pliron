// rust-lang/rust tests/ui/abi/rust-tail-cc.rs (aarch64 revision), adapted.
// rustc passes the `extern "tail"` ABI's large aggregates OnStack
// (byval); Cranelift has no StructArgument on arm64 and panicked. The
// backend now passes them as plain pointers there (both ends are ours).

#![feature(rust_tail_cc)]
#![feature(explicit_tail_calls)]
#![allow(incomplete_features)]

// An exported `extern "tail"` fn is not internal: its convention is the
// source ABI's `CallConv::Tail`, not the platform default. Before
// `FuncInfo::tail_cc`, non-local tail fns silently got the platform ABI —
// callers agreed by luck, but `become`/`return_call` and fn pointers were
// off-convention.
#[no_mangle]
extern "tail" fn exported_bite(x: u64) -> u64 {
    x * 3 + 1
}

extern "tail" fn tail_become(x: u64) -> u64 {
    become exported_bite(x)
}

extern "tail" fn local_tail(x: u64) -> u64 {
    x + 7
}

struct CrateOf<'a> {
    mcintosh: f64,
    golden_delicious: u64,
    jonagold: Option<&'a u64>,
    rome: [u64; 12],
}

#[inline(never)]
extern "tail" fn oven_explosion() {
    panic!("bad time");
}

#[inline(never)]
fn bite_into(yummy: u64) -> u64 {
    // Against a stock (LLVM-built) std on Windows, `panic!` is a real SEH
    // throw the emulated-EH invoke path can't intercept — only exercise the
    // catch with a backend-built std (`-Zbuild-std`) or elsewhere.
    #[cfg(not(pliron_stock_seh))]
    {
        let did_it_actually = std::panic::catch_unwind(move || oven_explosion());
        assert!(did_it_actually.is_err());
    }
    #[cfg(pliron_stock_seh)]
    let _ = oven_explosion as extern "tail" fn();
    yummy - 25
}

#[inline(never)]
extern "tail" fn lotsa_apples(
    honeycrisp: u64,
    gala: u32,
    fuji: f64,
    granny_smith: &[u64],
    pink_lady: (),
    and_a: CrateOf<'static>,
    cosmic_crisp: u64,
    ambrosia: f64,
    winesap: &[u64],
) -> (u64, f64, u64, u64) {
    assert_eq!(honeycrisp, 220);
    assert_eq!(gala, 140);
    assert_eq!(fuji, 210.54201234);
    assert_eq!(granny_smith, &[180, 210]);
    assert_eq!(pink_lady, ());
    assert_eq!(and_a.mcintosh, 150.0);
    assert_eq!(and_a.golden_delicious, 185);
    assert_eq!(and_a.jonagold, None); // my scales can't weight these gargantuans.
    assert_eq!(and_a.rome, [180, 182, 184, 186, 188, 190, 192, 194, 196, 198, 200, 202]);
    assert_eq!(cosmic_crisp, 270);
    assert_eq!(ambrosia, 193.1);
    assert_eq!(winesap, &[]);
    (
        and_a.rome.iter().sum(),
        fuji + ambrosia,
        cosmic_crisp - honeycrisp,
        bite_into(and_a.golden_delicious),
    )
}

// r[verify front.abi.internal]
fn main() {
    let pie = lotsa_apples(
        220,
        140,
        210.54201234,
        &[180, 210],
        (),
        CrateOf {
            mcintosh: 150.0,
            golden_delicious: 185,
            jonagold: None,
            rome: [180, 182, 184, 186, 188, 190, 192, 194, 196, 198, 200, 202],
        },
        270,
        193.1,
        &[],
    );
    assert_eq!(pie, (2292, 403.64201234, 50, 160));
    assert_eq!(tail_become(10), 31);
    let p: extern "tail" fn(u64) -> u64 = local_tail;
    assert_eq!(p(5), 12);
    let q: extern "tail" fn(u64) -> u64 = exported_bite;
    assert_eq!(q(5), 16);
}
