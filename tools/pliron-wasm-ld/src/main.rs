fn main() {
    if let Err(e) = pliron_wasm_ld::link(std::env::args().skip(1).collect()) {
        eprintln!("pliron-wasm-ld: {e}");
        std::process::exit(1);
    }
}
