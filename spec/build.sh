#!/bin/sh
# Build the rustc_codegen_pliron specification.
#   ./build.sh          PDF -> spec/build/pliron-spec.pdf (must compile with no warnings)
#   ./build.sh html     also try the experimental Typst HTML export
#   ./build.sh png      also write PNG page previews to spec/build/png/
#   ./build.sh trace    run `tracey query status` + `validate` from the repo root
# Fonts come only from spec/fonts/ and the faces built into Typst (FONTS.md),
# so the PDF is the same on every machine.
set -eu
cd "$(dirname "$0")"
mkdir -p build
T="typst compile --ignore-system-fonts --font-path fonts"
$T main.typ build/pliron-spec.pdf
echo "wrote build/pliron-spec.pdf"
for a in "$@"; do
  case "$a" in
    html) $T --format html --features html main.typ build/pliron-spec.html 2>build/html.log \
            && echo "wrote build/pliron-spec.html (experimental; warnings in build/html.log)" ;;
    png)  rm -rf build/png && mkdir -p build/png && $T --format png --ppi 80 main.typ "build/png/p-{0p}.png" \
            && echo "wrote build/png/" ;;
    trace) (cd .. && tracey query status && tracey query validate) ;;
  esac
done
