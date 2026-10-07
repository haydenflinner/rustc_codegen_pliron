#[derive(pm::Hi)] struct Foo;
fn main() { println!("{} {}", pm::twice!(21), Foo::hi()); }
