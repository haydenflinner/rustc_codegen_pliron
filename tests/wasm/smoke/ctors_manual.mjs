// The smoke module linked with `--export __wasm_call_ctors`: the embedder
// runs the constructors, so no export may run them on its own.
import { readFileSync } from "node:fs";

const i = new WebAssembly.Instance(
  new WebAssembly.Module(readFileSync(process.argv[2])),
  {},
);
const e = i.exports;
const rows = [
  ["fib(10) before", e.fib(10), 55],
  ["ctors() before", e.ctors(), 0],
  ["__wasm_call_ctors()", e.__wasm_call_ctors(), undefined],
  ["ctors() after", e.ctors(), 123],
  ["ctors() again", e.ctors(), 123],
];
let bad = 0;
for (const [name, got, want] of rows) {
  const ok = got === want;
  if (!ok) bad++;
  console.log(`${ok ? "ok  " : "FAIL"} ${name} = ${got}${ok ? "" : ` (want ${want})`}`);
}
if (bad) process.exit(1);
console.log("wasm smoke (exported __wasm_call_ctors) OK");
