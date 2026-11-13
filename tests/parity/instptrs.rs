#![crate_type="lib"]
// regex 1.7 `dfa.rs`: the first loop of `Fsm::exec_byte` — decode a
// state's delta-varint instruction pointers into a sparse set.
pub struct SparseSet { dense: Vec<usize>, sparse: Box<[usize]> }
impl SparseSet {
    pub fn insert(&mut self, value: usize) {
        let i = self.dense.len();
        assert!(i < self.dense.capacity());
        self.dense.push(value);
        self.sparse[value] = i;
    }
    pub fn clear(&mut self) { self.dense.clear(); }
}
fn read_varu32(data: &[u8]) -> (u32, usize) {
    let mut n: u32 = 0;
    let mut shift: u32 = 0;
    for (i, &b) in data.iter().enumerate() {
        if b < 0b1000_0000 {
            return (n | ((b as u32) << shift), i + 1);
        }
        n |= ((b as u32) & 0b0111_1111) << shift;
        shift += 7;
    }
    (0, 0)
}
fn read_vari32(data: &[u8]) -> (i32, usize) {
    let (un, i) = read_varu32(data);
    let mut n = (un >> 1) as i32;
    if un & 1 != 0 { n = !n; }
    (n, i)
}
struct InstPtrs<'a> { base: usize, data: &'a [u8] }
impl<'a> Iterator for InstPtrs<'a> {
    type Item = usize;
    fn next(&mut self) -> Option<usize> {
        if self.data.is_empty() { return None; }
        let (delta, nread) = read_vari32(self.data);
        let base = self.base as i32 + delta;
        self.data = &self.data[nread..];
        self.base = base as usize;
        Some(self.base)
    }
}
#[no_mangle]
pub fn fill(q: &mut SparseSet, data: &[u8]) {
    q.clear();
    for ip in (InstPtrs { base: 0, data: &data[1..] }) {
        q.insert(ip);
    }
}
