#![crate_type="lib"]
// regex 1.7 `dfa.rs`, `Fsm::cached_state_key`: walk the sparse set's
// instruction pointers, skip the epsilon instructions, and append the
// others to the key as zigzag delta varints.
pub enum Inst { Match(usize), Save(usize), Split(usize), EmptyLook(usize), Char(usize), Ranges(usize), Bytes(usize) }
fn usize_to_u32(n: usize) -> u32 {
    if (n as u64) > (u32::MAX as u64) { panic!("BUG: {} is too big to fit into u32", n) }
    n as u32
}
fn write_varu32(data: &mut Vec<u8>, mut n: u32) {
    while n >= 0b1000_0000 {
        data.push((n as u8) | 0b1000_0000);
        n >>= 7;
    }
    data.push(n as u8);
}
fn write_vari32(data: &mut Vec<u8>, n: i32) {
    let mut un = (n as u32) << 1;
    if n < 0 { un = !un; }
    write_varu32(data, un)
}
fn push_inst_ptr(data: &mut Vec<u8>, prev: &mut u32, ip: u32) {
    let delta = (ip as i32) - (*prev as i32);
    write_vari32(data, delta);
    *prev = ip;
}
#[no_mangle]
pub fn state_key(q: &[usize], prog: &[Inst], insts: &mut Vec<u8>, flags: &mut u8, past_match: bool) {
    use Inst::*;
    insts.clear();
    insts.push(0);
    let mut prev = 0;
    for &ip in q {
        let ip = usize_to_u32(ip);
        match prog[ip as usize] {
            Char(_) | Ranges(_) => unreachable!(),
            Save(_) | Split(_) => {}
            Bytes(_) => push_inst_ptr(insts, &mut prev, ip),
            EmptyLook(_) => {
                *flags |= 2;
                push_inst_ptr(insts, &mut prev, ip)
            }
            Match(_) => {
                push_inst_ptr(insts, &mut prev, ip);
                if !past_match { break; }
            }
        }
    }
}
pub struct Cache { pub scratch: Vec<u8>, pub past_match: bool }
// The function as regex has it: the key is built in a scratch vector
// taken out of the cache and put back, and the result is an `Arc<[u8]>`.
#[no_mangle]
pub fn cached_key(cache: &mut Cache, q: &[usize], prog: &[Inst], flags: &mut u8) -> Option<std::sync::Arc<[u8]>> {
    use Inst::*;
    let mut insts = std::mem::replace(&mut cache.scratch, vec![]);
    insts.clear();
    insts.push(0);
    let mut prev = 0;
    for &ip in q {
        let ip = usize_to_u32(ip);
        match prog[ip as usize] {
            Char(_) | Ranges(_) => unreachable!(),
            Save(_) | Split(_) => {}
            Bytes(_) => push_inst_ptr(&mut insts, &mut prev, ip),
            EmptyLook(_) => {
                *flags |= 2;
                push_inst_ptr(&mut insts, &mut prev, ip)
            }
            Match(_) => {
                push_inst_ptr(&mut insts, &mut prev, ip);
                if !cache.past_match { break; }
            }
        }
    }
    let r = if insts.len() == 1 && *flags & 1 == 0 {
        None
    } else {
        insts[0] = *flags;
        Some(std::sync::Arc::from(&*insts))
    };
    cache.scratch = insts;
    r
}
#[no_mangle]
pub fn zigzag(n: i32) -> u32 {
    let mut un = (n as u32) << 1;
    if n < 0 { un = !un; }
    un
}
