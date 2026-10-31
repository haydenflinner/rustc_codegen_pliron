#import "pliron-spec.typ": *

#show: spec.with(title: "rustc_codegen_pliron — a specification")

#cover(
  title: [rustc_codegen_pliron],
  subtitle: [A rustc code generator with no LLVM: what it promises, how it keeps the promise, and where it differs],
  version: "0.1",
  date: "2026-10-30",
)

#how-to-read()

#pagebreak()
#outline(title: [Contents], depth: 2)

#include "chapters/00-vocabulary.typ"
#include "chapters/01-model.typ"
#include "chapters/02-frontend.typ"
#include "chapters/03-iropt.typ"
#include "chapters/04-clif.typ"
#include "chapters/05-native.typ"
#include "chapters/06-wasm.typ"
#include "chapters/07-parallel.typ"
#include "chapters/08-verification.typ"

#chapter("back", 0)
#heading(level: 1, numbering: none)[Where we differ from LLVM]
Every #text(font: sans, size: 0.8em, weight: 700, fill: llvm-color)[COMPARED WITH LLVM]
note in this specification, grouped by kind. A #emph[gap] is something stock
rustc + LLVM does that this backend does not do yet; #emph[by design] is a
deliberate difference; #emph[beyond LLVM] is something this backend does that
the stock toolchain does not.
#v(4pt)
#differences()

#pagebreak()
#heading(level: 1, numbering: none)[Properties and checks]
Every requirement that names the check holding it to account.
#v(4pt)
#checks-index()

#pagebreak()
#heading(level: 1, numbering: none)[Level map]
Every level 0 requirement with the requirements that refine it, level by level.
#v(4pt)
#level-map()

#pagebreak()
#heading(level: 1, numbering: none)[Index of requirements]
#req-index()

#pagebreak()
#heading(level: 1, numbering: none)[Glossary index]
#glossary-index()
