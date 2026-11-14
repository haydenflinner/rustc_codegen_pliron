//! PLIRON: regression test — same-join `CondBr` (`SelectJoin`) must
//! evaluate every param's select operands before writing any param's
//! local. The `cond` value is rematerialized once per `select`, and a
//! param's `local.set` can reuse the local that a later remat (or a
//! later `vt`/`vf` arg) still reads.

use waffle::*;

/// Build the `Option<T>::branch`-shaped body:
/// ```text
/// fn(i32 tag, i32 payload) -> (i32, i32)
/// block0(v0 tag, v1 payload):
///   cond = tag & 255
///   if cond -> join(0, payload)   // Some -> Continue(v)
///   else    -> join(1, 0)         // None -> Break(_)
/// join(v8, v9): return v8, v9
/// ```
/// `cond` lowers as a rematerialized `local.get tag; i32.and 255` per
/// `select`, so the first param's `local.set` must not reuse `tag`'s
/// local before the second `select` reads it.
fn branch_body(module: &mut Module) -> FunctionBody {
    let sig = module.signatures.push(SignatureData {
        params: vec![Type::I32, Type::I32],
        returns: vec![Type::I32, Type::I32],
    });
    let mut body = FunctionBody::new(module, sig);
    let entry = body.entry;
    let join = body.add_block();

    let (v0, v1) = (body.blocks[entry].params[0].1, body.blocks[entry].params[1].1);
    let c255 = body.add_op(entry, Operator::I32Const { value: 255 }, &[], &[Type::I32]);
    let cond = body.add_op(entry, Operator::I32And, &[v0, c255], &[Type::I32]);
    let c0 = body.add_op(entry, Operator::I32Const { value: 0 }, &[], &[Type::I32]);
    let c1 = body.add_op(entry, Operator::I32Const { value: 1 }, &[], &[Type::I32]);

    let v8 = body.add_blockparam(join, Type::I32);
    let v9 = body.add_blockparam(join, Type::I32);

    body.set_terminator(
        entry,
        Terminator::CondBr {
            cond,
            if_true: BlockTarget {
                block: join,
                args: vec![c0, v1],
            },
            if_false: BlockTarget {
                block: join,
                args: vec![c1, c0],
            },
        },
    );
    body.set_terminator(
        join,
        Terminator::Return {
            values: vec![v8, v9],
        },
    );
    body
}

/// Interpret a `wasm_encoder::Function`'s bytes for the small opcode
/// set this shape lowers to (locals decl, `local.get/set/tee`,
/// `i32.const`, `i32.and`, `select`, `br`, `block`, `end`, `return`).
fn eval(func: wasm_encoder::Function, args: &[i64]) -> Vec<i64> {
    let bytes = func.into_raw_body();
    let mut pos = 0usize;
    // Local declarations: u32 count of (count, valtype) groups.
    let n_groups = read_u32(&bytes, &mut pos) as usize;
    let mut locals: Vec<i64> = args.to_vec();
    for _ in 0..n_groups {
        let n = read_u32(&bytes, &mut pos);
        pos += 1; // valtype
        locals.extend(std::iter::repeat(0).take(n as usize));
    }
    let mut stack: Vec<i64> = vec![];
    while pos < bytes.len() {
        match bytes[pos] {
            0x0b => {
                // end
                pos += 1;
            }
            0x0f => {
                // return
                break;
            }
            0x02 => {
                // block: consume the blocktype byte
                pos += 2;
            }
            0x0c => {
                // br L: only depth-0 branches to a single enclosing
                // block appear in this shape; scan to the matching
                // `end` (one level of nesting past intervening
                // block/end pairs).
                pos += 1;
                let _l = read_u32(&bytes, &mut pos);
                let mut depth = 0i32;
                loop {
                    match bytes[pos] {
                        0x02 | 0x04 | 0x03 => depth += 1,
                        0x0b if depth == 0 => break,
                        0x0b => depth -= 1,
                        _ => {}
                    }
                    pos += 1;
                }
                pos += 1;
            }
            0x1b => {
                // select
                let c = stack.pop().unwrap();
                let v2 = stack.pop().unwrap();
                let v1 = stack.pop().unwrap();
                stack.push(if c != 0 { v1 } else { v2 });
                pos += 1;
            }
            0x20 => {
                pos += 1;
                let l = read_u32(&bytes, &mut pos) as usize;
                stack.push(locals[l]);
            }
            0x21 => {
                pos += 1;
                let l = read_u32(&bytes, &mut pos) as usize;
                locals[l] = stack.pop().unwrap();
            }
            0x22 => {
                pos += 1;
                let l = read_u32(&bytes, &mut pos) as usize;
                locals[l] = *stack.last().unwrap();
            }
            0x41 => {
                let (v, n) = read_i64(&bytes[pos + 1..]);
                stack.push(v);
                pos += 1 + n;
            }
            0x45 => {
                // i32.eqz
                let a = stack.pop().unwrap();
                stack.push((a == 0) as i64);
                pos += 1;
            }
            0x71 => {
                // i32.and
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(a & b);
                pos += 1;
            }
            op => panic!("eval: unhandled opcode {:#04x} at {}", op, pos),
        }
    }
    stack
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

fn read_i64(bytes: &[u8]) -> (i64, usize) {
    let mut v = 0i64;
    let mut shift = 0;
    let mut n = 0;
    loop {
        let b = bytes[n];
        n += 1;
        v |= ((b & 0x7f) as i64) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            if shift < 64 && b & 0x40 != 0 {
                v |= -1i64 << shift;
            }
            break;
        }
    }
    (v, n)
}

#[test]
fn select_join_reads_operands_before_writes() {
    let mut module = Module::empty();
    let body = branch_body(&mut module);
    body.validate().expect("body valid");
    let func = body.compile().expect("compiles");

    // Some(7): tag=1 -> Continue(7) = (0, 7)
    assert_eq!(eval(func, &[1, 7]), vec![0, 7]);
}
