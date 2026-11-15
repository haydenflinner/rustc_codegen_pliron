//! PLIRON: `PLIRON_WASM_COLDBR` keeps hot→cold CondBr edges bare.
//!
//! Without it, a single-pred Err tail is inlined into the `if` arm, so
//! the join's block-arg moves sit on the hot edge. With it, the tail is
//! a labeled merge and the arm is a bare `br`.

use std::sync::Mutex;
use waffle::*;

static ENV: Mutex<()> = Mutex::new(());

/// ```text
/// entry:
///   brif cond, err, ok
/// err:          // const-only → join(1, 0)
///   jump join(1, 0)
/// ok:
///   jump join(0, payload)
/// join(tag, val):
///   return tag, val
/// ```
fn err_tail_body(module: &mut Module) -> FunctionBody {
    let sig = module.signatures.push(SignatureData {
        params: vec![Type::I32, Type::I32],
        returns: vec![Type::I32, Type::I32],
    });
    let mut body = FunctionBody::new(module, sig);
    let entry = body.entry;
    let err = body.add_block();
    let ok = body.add_block();
    let join = body.add_block();

    let (cond, payload) = (
        body.blocks[entry].params[0].1,
        body.blocks[entry].params[1].1,
    );
    let c0 = body.add_op(entry, Operator::I32Const { value: 0 }, &[], &[Type::I32]);
    let c1 = body.add_op(entry, Operator::I32Const { value: 1 }, &[], &[Type::I32]);
    let (tag, val) = (
        body.add_blockparam(join, Type::I32),
        body.add_blockparam(join, Type::I32),
    );

    body.set_terminator(
        entry,
        Terminator::CondBr {
            cond,
            if_true: BlockTarget {
                block: err,
                args: vec![],
            },
            if_false: BlockTarget {
                block: ok,
                args: vec![],
            },
        },
    );
    body.set_terminator(
        err,
        Terminator::Br {
            target: BlockTarget {
                block: join,
                args: vec![c1, c0],
            },
        },
    );
    body.set_terminator(
        ok,
        Terminator::Br {
            target: BlockTarget {
                block: join,
                args: vec![c0, payload],
            },
        },
    );
    body.set_terminator(
        join,
        Terminator::Return {
            values: vec![tag, val],
        },
    );
    body
}

/// True if some `if` arm is only a `br` (no `local.set` / const in the arm).
fn has_bare_if_br_arm(bytes: &[u8]) -> bool {
    let mut p = 0usize;
    // skip locals decl
    let n_groups = read_u32(bytes, &mut p) as usize;
    for _ in 0..n_groups {
        let _ = read_u32(bytes, &mut p);
        p += 1;
    }
    scan_bare_arm(bytes, &mut p, 0)
}

fn scan_bare_arm(bytes: &[u8], p: &mut usize, depth: i32) -> bool {
    while *p < bytes.len() {
        let op = bytes[*p];
        *p += 1;
        match op {
            0x04 => {
                // if: then until else/end, else until end
                let _ = read_i33(bytes, p); // blocktype
                let then_s = *p;
                let mut d = 1i32;
                let mut else_at = None;
                while *p < bytes.len() && d > 0 {
                    let o = bytes[*p];
                    *p += 1;
                    match o {
                        0x02 | 0x03 | 0x04 => {
                            d += 1;
                            let _ = read_i33(bytes, p);
                        }
                        0x05 if d == 1 => else_at = Some(*p),
                        0x0b => d -= 1,
                        0x0c | 0x0d => {
                            let _ = read_u32(bytes, p);
                        }
                        0x20 | 0x21 | 0x22 => {
                            let _ = read_u32(bytes, p);
                        }
                        0x41 | 0x42 => {
                            let _ = read_i33(bytes, p);
                        }
                        _ => {}
                    }
                }
                let end = *p;
                let else_at = else_at.unwrap_or(end - 1);
                let then_arm = &bytes[then_s..else_at.saturating_sub(1).max(then_s)];
                // else starts after 0x05; end-1 is the final 0x0b of the if
                let else_arm = if else_at < end {
                    &bytes[else_at..end - 1]
                } else {
                    &[]
                };
                if arm_is_bare_br(then_arm) || arm_is_bare_br(else_arm) {
                    return true;
                }
                if scan_bare_arm(then_arm, &mut 0, depth + 1) || scan_bare_arm(else_arm, &mut 0, depth + 1)
                {
                    return true;
                }
            }
            0x02 | 0x03 => {
                let _ = read_i33(bytes, p);
                if scan_bare_arm(bytes, p, depth + 1) {
                    return true;
                }
            }
            0x0b => {
                if depth == 0 {
                    return false;
                }
                return false;
            }
            0x0c | 0x0d | 0x20 | 0x21 | 0x22 => {
                let _ = read_u32(bytes, p);
            }
            0x41 | 0x42 => {
                let _ = read_i33(bytes, p);
            }
            _ => {}
        }
    }
    false
}

fn arm_is_bare_br(arm: &[u8]) -> bool {
    // br N  → 0x0c <leb>
    if arm.is_empty() {
        return false;
    }
    let mut p = 0usize;
    if arm[p] != 0x0c {
        return false;
    }
    p += 1;
    let _ = read_u32(arm, &mut p);
    p == arm.len()
}

fn read_u32(bytes: &[u8], pos: &mut usize) -> u32 {
    let mut v = 0u32;
    let mut shift = 0;
    loop {
        let b = bytes[*pos];
        *pos += 1;
        v |= ((b & 0x7f) as u32) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    v
}

fn read_i33(bytes: &[u8], pos: &mut usize) -> i64 {
    let mut v = 0i64;
    let mut shift = 0;
    loop {
        let b = bytes[*pos];
        *pos += 1;
        v |= ((b & 0x7f) as i64) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            if shift < 64 && (b & 0x40) != 0 {
                v |= -1i64 << shift;
            }
            break;
        }
    }
    v
}

#[test]
fn coldbr_makes_err_arm_bare() {
    let _g = ENV.lock().unwrap();
    // WBRIF is default on and flattens the if this assertion looks for.
    std::env::set_var("PLIRON_WBRIF", "0");
    std::env::set_var("PLIRON_WASM_COLDBR", "1");
    let mut module = Module::empty();
    let body = err_tail_body(&mut module);
    body.validate().expect("valid");
    let bytes = body.compile().expect("compile").into_raw_body();
    std::env::remove_var("PLIRON_WASM_COLDBR");
    std::env::remove_var("PLIRON_WBRIF");
    assert!(
        has_bare_if_br_arm(&bytes),
        "expected a bare br if-arm under COLDBR; bytes={:02x?}",
        bytes
    );
}

#[test]
fn without_coldbr_err_arm_has_moves() {
    let _g = ENV.lock().unwrap();
    std::env::set_var("PLIRON_WBRIF", "0");
    std::env::remove_var("PLIRON_WASM_COLDBR");
    let mut module = Module::empty();
    let body = err_tail_body(&mut module);
    body.validate().expect("valid");
    let bytes = body.compile().expect("compile").into_raw_body();
    std::env::remove_var("PLIRON_WBRIF");
    assert!(
        !has_bare_if_br_arm(&bytes),
        "default stackify should inline the Err tail (moves in the if arm); bytes={:02x?}",
        bytes
    );
}
