//! Assembles stdin and prints the `.text` bytes as hex. Used by the
//! differential test script to compare against GNU as.

use rsasm::arch;
use rsasm::assembler::{Assembler, Options};
use rsasm::section::SectionId;
use std::io::Read;

fn main() {
    let mut src = String::new();
    std::io::stdin()
        .read_to_string(&mut src)
        .expect("read stdin");

    let name = std::env::args().nth(1).unwrap_or_else(|| "x86-64".into());
    let Some(arch) = arch::lookup(&name) else {
        eprintln!("unknown architecture `{name}`");
        std::process::exit(2);
    };
    // An optional second argument names the dialect; otherwise the
    // architecture's usual one, as the command-line tool does.
    let dialect = match std::env::args().nth(2) {
        Some(d) => match rsasm::lexer::Dialect::from_name(&d) {
            Some(d) => d,
            None => {
                eprintln!("unknown dialect `{d}`");
                std::process::exit(2);
            }
        },
        None => arch.default_dialect(),
    };
    // A third argument, `bin`, assembles a flat image at address 0, for
    // comparison with a reference that writes one.
    let flat = std::env::args().nth(3).as_deref() == Some("bin");
    let mut asm = Assembler::new(
        arch,
        Options::new().with_dialect(dialect).with_relocatable(!flat),
    );
    asm.assemble_str("<stdin>", &src);
    let ok = asm.finish();
    if !ok || asm.diags().has_errors() {
        print!(
            "RSASM-ERROR: {}",
            asm.diags()
                .render(asm.source_map(), false)
                .replace('\n', " | ")
        );
        println!();
        std::process::exit(1);
    }
    let bytes = asm.section_bytes(SectionId(0));
    println!(
        "{}",
        bytes
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
}
