use std::collections::HashSet;
fn main() {
    let a: HashSet<i32> = [1, 2, 3].into_iter().collect();
    let b: HashSet<i32> = [4, 2, 3, 4].into_iter().collect();
    let mut s = a.symmetric_difference(&b);
    println!("{:?} {:?} {:?}", s.next(), s.next(), s.next());
}
