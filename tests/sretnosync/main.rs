// PLIRON_SROA_SRETNOSYNC: sroa splits a call's `sret` temporary into
// SSA slices and normally writes every slice back into the temporary
// before each call that fills it. With the knob the writeback is skipped
// when the call's sret argument is the whole temporary. Shapes: an
// interpreter-style loop whose step returns a large enum through sret
// (variants writing different byte ranges: a payload-free one, a scalar
// one and an 8-value array), a destination read between calls, a call
// that unwinds (an invoke keeps its writeback), and a value built in the
// temporary before it is reused as a call's destination.
use std::hint::black_box;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Val {
    Null,
    Int(i64),
    Float(f64),
}

enum Flow {
    Next,
    Call { target: u32, args: [Val; 8], n: u8 },
    Return(Val),
}

#[inline(never)]
fn step(i: u32, k: u32) -> Flow {
    match (i + k) % 7 {
        0 | 1 | 2 => Flow::Next,
        3 => Flow::Return(Val::Int(i as i64 * 3)),
        4 => Flow::Return(Val::Float(i as f64 * 0.5)),
        _ => {
            let mut args = [Val::Null; 8];
            for j in 0..(i % 8) as usize {
                args[j] = Val::Int((i as i64) << j);
            }
            Flow::Call { target: i ^ k, args, n: (i % 8) as u8 }
        }
    }
}

#[inline(never)]
fn may_panic(i: u32) -> Flow {
    if i == 13 {
        panic!("boom");
    }
    Flow::Return(Val::Int(i as i64))
}

fn mix(h: u64, v: u64) -> u64 {
    (h ^ v).wrapping_mul(0x100_0000_01b3)
}

fn main() {
    std::panic::set_hook(Box::new(|_| {}));
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    // 1. the dispatch loop: one destination, refilled every iteration
    let k = black_box(5);
    for i in 0..2000u32 {
        let r = step(i, k);
        match r {
            Flow::Next => h = mix(h, 1),
            Flow::Return(Val::Int(x)) => h = mix(h, x as u64),
            Flow::Return(Val::Float(f)) => h = mix(h, f.to_bits()),
            Flow::Return(Val::Null) => h = mix(h, 7),
            Flow::Call { target, args, n } => {
                h = mix(h, target as u64);
                for a in &args[..n as usize] {
                    if let Val::Int(x) = a {
                        h = mix(h, *x as u64);
                    }
                }
            }
        }
    }
    // 2. a value built in place, read, then the same local refilled by a call
    let mut f = Flow::Call { target: 9, args: [Val::Int(1); 8], n: 8 };
    if let Flow::Call { args, .. } = &f {
        h = mix(h, args.iter().filter(|a| **a == Val::Int(1)).count() as u64);
    }
    for i in 0..50u32 {
        f = step(black_box(i), k);
        if let Flow::Return(Val::Int(x)) = f {
            h = mix(h, x as u64);
        }
    }
    // 3. an unwinding call into a destination that holds a value
    let mut last = Flow::Return(Val::Int(-1));
    for i in 10..16u32 {
        match std::panic::catch_unwind(|| may_panic(black_box(i))) {
            Ok(v) => last = v,
            Err(_) => h = mix(h, 99),
        }
        if let Flow::Return(Val::Int(x)) = last {
            h = mix(h, x as u64);
        }
    }
    println!("{h}");
}
