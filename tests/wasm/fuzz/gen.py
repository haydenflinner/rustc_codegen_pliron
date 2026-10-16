#!/usr/bin/env python3
"""gen.py SEED COUNT OUT.rs — random loop kernels over static arrays for
differential testing of the wasm loop vectorizer against stock LLVM.

Every kernel is `extern "C" fn kN(n: u32, off: u32) -> u32`: it runs a loop of
`n` iterations starting `off` elements into the arrays and returns a checksum of
what it produced. The driver (fuzz.mjs) compares both builds for many (n, off)
and also hashes all arrays afterwards to catch stray writes."""
import random
import sys

seed, count, out = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
R = random.Random(seed)

INT = ["u8", "i8", "u16", "i16", "u32", "i32", "u64", "i64"]
FLT = ["f32", "f64"]
SIZE = 4200

lines = []
w = lines.append
w("#![no_std]")
w("#![allow(internal_features, static_mut_refs, unused, clippy::all, non_upper_case_globals, non_snake_case)]")
w("use core::hint::black_box;")
w("#[panic_handler] fn panic(_: &core::panic::PanicInfo) -> ! { core::arch::wasm32::unreachable() }")
# arrays: two inputs and two outputs per type
for t in INT + FLT:
    for nm in "ABOP":
        init = "0.0" if t in FLT else "0"
        w(f"static mut {nm}_{t}: [{t}; {SIZE}] = [{init}; {SIZE}];")


def bits(t):
    return int(t[1:])


def lit(t):
    if t in FLT:
        return f"{R.choice([0.5, 1.0, 2.0, -1.5, 3.25, 0.125, 100.0])}{t}"
    b = bits(t)
    signed = t[0] == "i"
    lim = (1 << (b - 1)) - 1 if signed else (1 << b) - 1
    if R.random() < 0.5:
        v = R.choice([0, 1, 2, 3, 7, 15, 31, 100])
    else:
        v = R.randrange(0, min(lim, 1 << 31) + 1)
    v = min(v, lim)
    if signed and R.random() < 0.3:
        v = -v
    return f"({v}{t})"


def expr(t, depth, vars_):
    """a random expression of type t"""
    if depth == 0 or R.random() < 0.2:
        if R.random() < 0.75:
            return R.choice(vars_)
        return lit(t)
    isf = t in FLT
    if isf:
        k = R.choice(["+", "-", "*", "min", "max", "abs", "neg", "sel"])
    else:
        k = R.choice(["+", "-", "*", "&", "|", "^", "shl", "shr", "min", "max", "sat", "sel",
                      "abd", "wadd"])
    a = expr(t, depth - 1, vars_)
    if k in ("abs", "neg"):
        if isf:
            return f"({a}).abs()" if k == "abs" else f"(-({a}))"
    b = expr(t, depth - 1, vars_)
    if isf:
        if k in "+-*":
            return f"(({a}) {k} ({b}))"
        if k == "min":
            return f"({a}).min({b})"
        if k == "max":
            return f"({a}).max({b})"
        if k == "sel":
            c = expr(t, depth - 1, vars_)
            return f"(if ({a}) < ({b}) {{ {c} }} else {{ {b} }})"
        return f"({a})"
    m = {"+": "wrapping_add", "-": "wrapping_sub", "*": "wrapping_mul"}
    if k in m:
        return f"({a}).{m[k]}({b})"
    if k in "&|^":
        return f"(({a}) {k} ({b}))"
    if k == "shl":
        return f"(({a}).wrapping_shl({R.randrange(0, bits(t))}))"
    if k == "shr":
        return f"(({a}) >> {R.randrange(0, bits(t))})"
    if k == "min":
        return f"({a}).min({b})"
    if k == "max":
        return f"({a}).max({b})"
    if k == "sat":
        return f"({a}).saturating_{R.choice(['add', 'sub'])}({b})"
    if k == "abd":
        return f"(({a}).wrapping_sub({b}))"
    if k == "wadd":
        return f"({a}).wrapping_add({lit(t)})"
    if k == "sel":
        c = expr(t, depth - 1, vars_)
        op = R.choice(["<", ">", "==", "!=", "<=", ">="])
        return f"(if ({a}) {op} ({b}) {{ {c} }} else {{ {b} }})"
    return a


def fold(t, arr):
    if t in FLT:
        return f"{arr}.iter().fold(0u32, |h, &x| h.wrapping_mul(31).wrapping_add(x.to_bits() as u32))"
    return f"{arr}.iter().fold(0u32, |h, &x| h.wrapping_mul(31).wrapping_add(x as u32))"


def kernel(i):
    t = R.choice(INT + FLT) if R.random() < 0.8 else R.choice(["u8", "u32", "f32"])
    form = R.choice(["map", "zipmap", "reduce", "inplace", "window", "carried", "count", "dist"])
    d = R.randrange(1, 4)
    A, B, O = f"A_{t}", f"B_{t}", f"O_{t}"
    body = []
    ret = "0"
    if form == "map":
        e = expr(t, d, ["x", "y"])
        body += [
            "let (a, b, o) = unsafe { (&A[off as usize..off as usize + n as usize], &B[off as usize..off as usize + n as usize], &mut O[off as usize..off as usize + n as usize]) };",
            "for i in 0..n as usize { let x = a[i]; let y = b[i]; o[i] = " + e + "; }",
            f"{fold(t, 'o')}",
        ]
    elif form == "zipmap":
        e = expr(t, d, ["x", "y"])
        body += [
            "let (a, b, o) = unsafe { (&A[off as usize..off as usize + n as usize], &B[off as usize..off as usize + n as usize], &mut O[off as usize..off as usize + n as usize]) };",
            "for ((o, &x), &y) in o.iter_mut().zip(a).zip(b) { *o = " + e + "; }",
            f"{fold(t, 'o')}",
        ]
    elif form == "inplace":
        e = expr(t, d, ["x", "k"])
        body += [
            "let a = unsafe { &mut A[off as usize..off as usize + n as usize] };",
            f"let k = black_box({lit(t)});",
            "for v in a.iter_mut() { let x = *v; *v = " + e + "; }",
            f"{fold(t, 'a')}",
        ]
    elif form == "reduce":
        red = R.choice(["add", "xor", "or", "and", "max", "min"] if t not in FLT else ["max", "min"])
        e = expr(t, d, ["x"])
        if t in FLT:
            init = "0.0" if red == "max" else "0.0"
        elif red == "and":
            init = f"!(0 as {t})"
        elif red == "min":
            init = f"{t}::MAX"
        else:
            init = f"(0 as {t})"
        opx = {
            "add": "acc.wrapping_add(v)", "xor": "acc ^ v", "or": "acc | v", "and": "acc & v",
            "max": "acc.max(v)", "min": "acc.min(v)",
        }[red]
        body += [
            "let a = unsafe { &A[off as usize..off as usize + n as usize] };",
            f"let mut acc: {t} = {init};",
            "for &x in a { let v = " + e + f"; acc = {opx}; }}",
            ("acc.to_bits() as u32" if t in FLT else "acc as u32"),
        ]
    elif form == "window":
        e = expr(t, d, ["x", "y", "z"])
        body += [
            "let (a, o) = unsafe { (&A[off as usize..off as usize + n as usize + 2], &mut O[off as usize..off as usize + n as usize]) };",
            "for i in 0..n as usize { let x = a[i]; let y = a[i + 1]; let z = a[i + 2]; o[i] = " + e + "; }",
            f"{fold(t, 'o')}",
        ]
    elif form == "carried":
        dist = R.choice([1, 2, 3, 5, 8, 17, 40])
        sign = R.choice(["fwd", "back"])
        e = expr(t, 1, ["x"])
        if sign == "fwd":
            body += [
                f"let a = unsafe {{ &mut A[off as usize..off as usize + n as usize + {dist}] }};",
                "for i in 0..n as usize { let x = a[i]; a[i + " + str(dist) + "] = " + e + "; }",
                f"{fold(t, 'a')}",
            ]
        else:
            body += [
                f"let a = unsafe {{ &mut A[off as usize..off as usize + n as usize + {dist}] }};",
                "for i in 0..n as usize { let x = a[i + " + str(dist) + "]; a[i] = " + e + "; }",
                f"{fold(t, 'a')}",
            ]
    elif form == "count":
        op = R.choice(["<", ">", "==", "!="])
        body += [
            "let a = unsafe { &A[off as usize..off as usize + n as usize] };",
            f"let t = black_box({lit(t)});",
            f"let mut c = 0u32; for &x in a {{ if x {op} t {{ c += 1; }} }}",
            "c",
        ]
    else:  # dist: two output streams, possibly overlapping with an input
        e1 = expr(t, d, ["x", "y"])
        body += [
            "let (a, b, o, p) = unsafe { (&A[off as usize..off as usize + n as usize], &B[off as usize..off as usize + n as usize], &mut O[off as usize..off as usize + n as usize], &mut P_" + t + "[off as usize..off as usize + n as usize]) };",
            "for i in 0..n as usize { let x = a[i]; let y = b[i]; o[i] = " + e1 + "; p[i] = x; }",
            f"{fold(t, 'o')} ^ {fold(t, 'p')}",
        ]
    src = [f"#[unsafe(no_mangle)] pub extern \"C\" fn k{i}(n: u32, off: u32) -> u32 {{"]
    src.append(f"    use core::hint::black_box; type T = {t};")
    src.append(f"    let n = n.min({SIZE - 64}); let off = off.min(30);")
    src.append(f"    let (A, B, O) = unsafe {{ (&mut A_{t}, &mut B_{t}, &mut O_{t}) }};")
    src.append("    unsafe {")
    for ln in body[:-1]:
        src.append("        " + ln)
    src.append("        " + body[-1])
    src.append("    }")
    src.append("}")
    return "\n".join(src)


# kernels borrow the static arrays mutably multiple times; build each with its
# own scope of raw pointers instead
def fixup(s):
    s = s.replace("let (A, B, O) = unsafe { (&mut A_", "let (A, B, O) = unsafe { (&mut A_")
    return s


for i in range(count):
    w(fixup(kernel(i)))

# init + hash
w("static mut SEED: u32 = 1;")
w("fn nx() -> u32 { unsafe { SEED = SEED.wrapping_mul(1664525).wrapping_add(1013904223); SEED } }")
w("#[unsafe(no_mangle)] pub extern \"C\" fn init(s: u32) {")
w("    unsafe { SEED = s;")
for t in INT:
    for nm in "ABOP":
        w(f"        for x in {nm}_{t}.iter_mut() {{ *x = (nx() >> {R.choice([0, 3, 9, 16])}) as {t}; }}")
for t in FLT:
    for nm in "ABOP":
        w(f"        for x in {nm}_{t}.iter_mut() {{ *x = ((nx() >> 8) as {t}) / 16777216.0 - 0.5; }}")
w("    }")
w("}")
w("#[unsafe(no_mangle)] pub extern \"C\" fn hash() -> u32 { let mut h = 0u32; unsafe {")
for t in INT + FLT:
    for nm in "ABOP":
        f = "x.to_bits() as u32" if t in FLT else "*x as u32"
        f = f.replace("*x", "*x") if t not in FLT else "x.to_bits() as u32"
        w(f"    for x in {nm}_{t}.iter() {{ h = h.wrapping_mul(31).wrapping_add({'x.to_bits() as u32' if t in FLT else '*x as u32'}); }}")
w("} h }")
w(f"#[unsafe(no_mangle)] pub extern \"C\" fn count() -> u32 {{ {count} }}")

open(out, "w").write("\n".join(lines) + "\n")
