// Linked against tests/xfactsplugin (lib.rs) with `-Clto=thin`, as
// cargo's `lto = "thin"` does (nushell): cross-crate ThinLTO, which
// PLIRON_XFACTS_ALL used to refuse with a fatal error. It now declines
// ThinLTO support for such sessions, so rustc falls back to fat LTO.
fn main() {
    println!("{}", xfactsplugin::b::g(&[1, 2, 3, 40]));
}
