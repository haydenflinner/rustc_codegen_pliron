//! The differential checker must flag real behavior changes and stay quiet
//! on equivalent rewrites and on runs that hit undefined behavior.

use cranelift_codegen::ir::{Function, Value};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use rustc_hash::FxHashSet;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn cfg() -> Config {
    Config {
        trials: 32,
        ..Config::default()
    }
}

#[test]
fn equivalent_rewrite_passes() {
    let a = parse(
        "function %f(i64) -> i64 { block0(v0: i64): v9 = iconst.i64 2 v1 = iadd v0, v9 return v1 }",
    );
    let b = parse(
        "function %f(i64) -> i64 { block0(v0: i64): v9 = iconst.i64 1 v1 = iadd v0, v9 v2 = iadd v1, v9 return v2 }",
    );
    let s = check(&a, &b, &Facts::default(), &cfg()).unwrap();
    assert_eq!(s.compared, 32);
}

// r[verify verify.diffcheck.observe]
#[test]
// r[verify clif.diffcheck.observe]
fn wrong_constant_is_caught() {
    let a = parse(
        "function %f(i32) -> i32 { block0(v0: i32): v9 = iconst.i32 2 v1 = iadd v0, v9 return v1 }",
    );
    let b = parse(
        "function %f(i32) -> i32 { block0(v0: i32): v9 = iconst.i32 3 v1 = iadd v0, v9 return v1 }",
    );
    assert!(check(&a, &b, &Facts::default(), &cfg()).is_err());
}

#[test]
fn dropped_store_through_pointer_is_caught() {
    let a = parse("function %f(i64, i32) { block0(v0: i64, v1: i32): store v1, v0+8 return }");
    let b = parse("function %f(i64, i32) { block0(v0: i64, v1: i32): return }");
    let m = check(&a, &b, &Facts::default(), &cfg()).unwrap_err();
    assert!(m.to_string().contains("bytes written"), "{m}");
}

#[test]
fn load_forwarded_across_clobbering_call_is_caught() {
    // A callee that may write memory can change *p between the two loads.
    let a = parse(
        "function %f(i64) -> i32 {
            sig0 = (i64)
            fn0 = %g sig0
        block0(v0: i64):
            v1 = load.i32 v0
            call fn0(v0)
            v2 = load.i32 v0
            v3 = iadd v1, v2
            return v3
        }",
    );
    let b = parse(
        "function %f(i64) -> i32 {
            sig0 = (i64)
            fn0 = %g sig0
        block0(v0: i64):
            v1 = load.i32 v0
            call fn0(v0)
            v3 = iadd v1, v1
            return v3
        }",
    );
    assert!(check(&a, &b, &Facts::default(), &cfg()).is_err());
}

// r[verify verify.diffcheck.inconclusive]
#[test]
// r[verify clif.diffcheck.sound]
fn trapping_original_is_not_a_mismatch() {
    let a = parse("function %f(i32) -> i32 { block0(v0: i32): trap user1 }");
    let b = parse("function %f(i32) -> i32 { block0(v0: i32): v1 = iconst.i32 0 return v1 }");
    let s = check(&a, &b, &Facts::default(), &cfg()).unwrap();
    assert_eq!(s.compared, 0);
    assert_eq!(s.inconclusive, 32);
}

#[test]
fn loop_and_memcpy_libcall() {
    // Sum of n bytes, n clamped to 16; copies 8 bytes first via memcpy.
    let src = |k: i64| {
        format!(
            "function %f(i64, i64) -> i64 {{
                sig0 = (i64, i64, i64) system_v
                fn0 = %Memcpy sig0
            block0(v0: i64, v1: i64):
                v2 = iconst.i64 8
                v19 = iconst.i64 64
                v20 = iadd v0, v19
                call fn0(v20, v0, v2)
                v18 = iconst.i64 15
                v3 = band v1, v18
                v4 = iconst.i64 0
                jump block1(v4, v4)
            block1(v5: i64, v6: i64):
                v7 = icmp ult v5, v3
                brif v7, block2, block3
            block2:
                v8 = iadd v20, v5
                v9 = uload8.i64 v8
                v10 = iadd v6, v9
                v17 = iconst.i64 1
                v11 = iadd v5, v17
                jump block1(v11, v10)
            block3:
                v16 = iconst.i64 {k}
                v12 = iadd v6, v16
                return v12
            }}"
        )
    };
    let a = parse(&src(0));
    let s = check(&a, &parse(&src(0)), &Facts::default(), &cfg()).unwrap();
    assert!(s.compared >= 24, "{s:?}");
    assert!(check(&a, &parse(&src(1)), &Facts::default(), &cfg()).is_err());
}

#[test]
fn self_recursion_runs_for_real() {
    // Recursive sum 0..=n (n clamped to 31) vs the same as a loop: equal.
    let rec = parse(
        "function %f(i64) -> i64 {
            sig0 = (i64) -> i64
            fn0 = %f sig0
        block0(v0: i64):
            v1 = iconst.i64 31
            v2 = band v0, v1
            brif v2, block1, block2
        block1:
            v3 = iconst.i64 1
            v4 = isub v2, v3
            v5 = call fn0(v4)
            v6 = iadd v2, v5
            return v6
        block2:
            return v2
        }",
    );
    let lp = parse(
        "function %f(i64) -> i64 {
        block0(v0: i64):
            v1 = iconst.i64 31
            v2 = band v0, v1
            v3 = iconst.i64 0
            jump block1(v2, v3)
        block1(v4: i64, v5: i64):
            brif v4, block2, block3
        block2:
            v6 = iadd v5, v4
            v7 = iconst.i64 1
            v8 = isub v4, v7
            jump block1(v8, v6)
        block3:
            return v5
        }",
    );
    let s = check(&rec, &lp, &Facts::default(), &cfg()).unwrap();
    assert!(s.compared >= 24, "{s:?}");
}

#[test]
// r[verify clif.vendor.interp]
fn vector_built_from_i128_matches_stack_roundtrip() {
    // vecslot's rewrite: spill bytes to a slot and reload as i64x2, versus
    // building the vector with iconcat + bitcast. Equal.
    let a = parse(
        "function %f(i64) {
            ss0 = explicit_slot 16, align = 8
        block0(v0: i64):
            v1 = stack_addr.i64 ss0
            v2 = iconst.i8 50
            store v2, v1
            v3 = iconst.i64 0
            store v3, v1+1
            v4 = iconst.i32 0
            store v4, v1+9
            v5 = iconst.i16 0
            store v5, v1+13
            v6 = iconst.i8 0
            store v6, v1+15
            v7 = load.i64x2 v1
            store v7, v0
            return
        }",
    );
    let b = parse(
        "function %f(i64) {
        block0(v0: i64):
            v1 = iconst.i64 50
            v2 = iconst.i64 0
            v3 = iconcat v1, v2
            v4 = bitcast.i64x2 little v3
            store v4, v0
            return
        }",
    );
    let s = check(&a, &b, &Facts::default(), &cfg()).unwrap();
    assert_eq!(s.compared, 32);
}

#[test]
fn saved_clif_round_trips_symbol_names() {
    let src = "function %f() -> i64 {
        gv0 = symbol colocated u1:5
    block0:
        v0 = symbol_value.i64 gv0
        return v0
    }";
    let f = parse(src);
    assert!(f.display().to_string().contains("userextname0"));
    let text = pliron_clif_opt::diffcheck::clif_text(&f);
    assert!(text.contains("symbol colocated u1:5"), "{text}");
    let g = parse(&text);
    assert_eq!(g.display().to_string(), f.display().to_string());
}

#[test]
fn zero_size_slot_address_is_valid() {
    // A ZST local's address is computed but never dereferenced.
    let a = parse("function %f() -> i64 { block0: v0 = iconst.i64 1 return v0 }");
    let b = parse(
        "function %f() -> i64 {
            ss0 = explicit_slot 0, align = 8
        block0:
            v1 = stack_addr.i64 ss0
            v2 = iconst.i64 1
            return v2
        }",
    );
    let s = check(&a, &b, &Facts::default(), &cfg()).unwrap();
    assert_eq!(s.compared, 32);
}

#[test]
fn nonnull_load_fact_is_honored() {
    // `v1` comes from a `!nonnull` load, so folding `v1 != 0` to true is
    // allowed; runs where the load yields 0 are undefined, not mismatches.
    let a = parse(
        "function %f(i64) -> i8 {
        block0(v0: i64):
            v1 = load.i64 notrap v0
            v2 = iconst.i64 0
            v3 = icmp ne v1, v2
            return v3
        }",
    );
    let b = parse(
        "function %f(i64) -> i8 {
        block0(v0: i64):
            v1 = load.i64 notrap v0
            v3 = iconst.i8 1
            return v3
        }",
    );
    // Without the fact, a zero in memory exposes the fold.
    let cfg = Config {
        trials: 64,
        ..Config::default()
    };
    assert!(check(&a, &b, &Facts::default(), &cfg).is_err());
    let nn: FxHashSet<Value> = [Value::from_u32(1)].into_iter().collect();
    let facts = Facts {
        nonnull: Some(&nn),
        ..Facts::default()
    };
    check(&a, &b, &facts, &cfg).unwrap();
}

// r[verify verify.diffcheck.inconclusive]
#[test]
fn uninit_slot_bytes_may_reshuffle() {
    // slotmerge folds an output slot onto a live one: bytes the original
    // left unwritten read back as the slot-mate's real data instead of
    // the interpreter's zero fill. Legal — those bytes were never
    // defined — so the differing lanes are excused, not a mismatch.
    let a = parse(
        "function %f() -> i64 {
            ss0 = explicit_slot 8, align = 8
            ss1 = explicit_slot 16, align = 8
        block0:
            v1 = stack_addr.i64 ss0
            v2 = stack_addr.i64 ss1
            v5 = iconst.i16 0x6261
            store v5, v2+6
            v8 = load.i8 v2
            v3 = iconst.i32 0x785c
            store v3, v1
            v6 = iconst.i16 0x30
            store v6, v1+4
            v4 = load.i64 v1
            return v4
        }",
    );
    // The merged body: ss0's accesses land on ss1, so the load's top two
    // bytes observe the table bytes (0x6162), not uninit.
    let b = parse(
        "function %f() -> i64 {
            ss1 = explicit_slot 16, align = 8
        block0:
            v2 = stack_addr.i64 ss1
            v5 = iconst.i16 0x6261
            store v5, v2+6
            v8 = load.i8 v2
            v3 = iconst.i32 0x785c
            store v3, v2
            v6 = iconst.i16 0x30
            store v6, v2+4
            v4 = load.i64 v2
            return v4
        }",
    );
    let s = check(&a, &b, &Facts::default(), &cfg()).unwrap();
    assert_eq!(s.inconclusive, 32, "{s:?}");

    // The same rewrite with a wrong *written* byte is still a miscompile:
    // determined bytes must survive any transform.
    let c = parse(
        "function %f() -> i64 {
            ss1 = explicit_slot 16, align = 8
        block0:
            v2 = stack_addr.i64 ss1
            v5 = iconst.i16 0x6261
            store v5, v2+6
            v8 = load.i8 v2
            v3 = iconst.i32 0x785d
            store v3, v2
            v6 = iconst.i16 0x30
            store v6, v2+4
            v4 = load.i64 v2
            return v4
        }",
    );
    assert!(check(&a, &c, &Facts::default(), &cfg()).is_err());
}

#[test]
fn noalias_param_memory_is_not_reachable_from_other_memory() {
    // `v1` is noalias, so a pointer loaded from `v0`'s memory can't point
    // into it: forwarding `*v1` across a call given that pointer is fine.
    let src = |reload: bool| {
        format!(
            "function %f(i64, i64) -> i64 {{
                sig0 = (i64)
                fn0 = %g sig0
            block0(v0: i64, v1: i64):
                v2 = load.i64 notrap v1
                v3 = load.i64 notrap v0
                call fn0(v3)
                {}
                v5 = iadd v2, v4
                return v5
            }}",
            if reload {
                "v4 = load.i64 notrap v1"
            } else {
                "v4 -> v2"
            }
        )
    };
    let (a, b) = (parse(&src(true)), parse(&src(false)));
    // The clobber needs the seeded pointer to land on `*v1` exactly: rare.
    let cfg = Config {
        trials: 2000,
        ..Config::default()
    };
    assert!(check(&a, &b, &Facts::default(), &cfg).is_err());
    let na: FxHashSet<Value> = [Value::from_u32(1)].into_iter().collect();
    let facts = Facts {
        noalias: Some(&na),
        ..Facts::default()
    };
    check(&a, &b, &facts, &cfg).unwrap();
}
