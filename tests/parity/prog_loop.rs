#![crate_type="lib"]
// regex 1.7 `dfa.rs:959`: the second loop of `Fsm::exec_byte` — walk the
// current NFA states and dispatch on each instruction's kind.
pub struct InstBytes { pub goto: usize, pub start: u8, pub end: u8 }
impl InstBytes { #[inline] pub fn matches(&self, byte: u8) -> bool { self.start <= byte && byte <= self.end } }
pub enum Inst {
    Match(usize),
    Save { goto: usize, slot: usize },
    Split { goto1: usize, goto2: usize },
    EmptyLook { goto: usize, look: u8 },
    Char { goto: usize, c: char },
    Ranges { goto: usize, ranges: Box<[(char, char)]> },
    Bytes(InstBytes),
}
extern "Rust" { fn follow(ip: u32, q: &mut Vec<usize>); }
#[no_mangle]
pub fn step(prog: &Vec<Inst>, qcur: &[usize], qnext: &mut Vec<usize>, b: u16) -> bool {
    let mut is_match = false;
    for &ip in qcur {
        match prog[ip as usize] {
            Inst::Char { .. } | Inst::Ranges { .. } => unreachable!(),
            Inst::Save { .. } | Inst::Split { .. } | Inst::EmptyLook { .. } => {}
            Inst::Match(_) => { is_match = true; break; }
            Inst::Bytes(ref inst) => {
                if b < 256 && inst.matches(b as u8) {
                    unsafe { follow(inst.goto as u32, qnext) };
                }
            }
        }
    }
    is_match
}
