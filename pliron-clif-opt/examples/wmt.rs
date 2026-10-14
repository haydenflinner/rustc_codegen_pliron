use cranelift_codegen::ir::Function;
fn main() {
    let src = std::fs::read_to_string(std::env::args().nth(1).unwrap()).unwrap();
    let mut f: Function = cranelift_reader::parse_functions(&src).unwrap().remove(0);
    let (n, _) = pliron_clif_opt::clifpeep::webmerge(&mut f);
    eprintln!("merges={n}\n{}", f.display());
}
