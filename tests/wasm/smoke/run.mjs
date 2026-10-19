// checks the smoke module: each row prints "ok   name = value" or exits 1.
import { readFileSync } from "node:fs";

const i = new WebAssembly.Instance(
  new WebAssembly.Module(readFileSync(process.argv[2])),
  {},
);
const e = i.exports;

const helper = (x) => (((x * 3 >>> 0) << 5 | (x * 3 >>> 0) >>> 27) >>> 0) ^ 0x1234;
const rows = [
  ["fib(20)", e.fib(20), 6765],
  ["sum_sq(100)", e.sum_sq(100), 338350n],
  ["hypot(3,4)", e.hypot(3, 4), 5],
  ["statics(6)", e.statics(6), 63],
  ["indirect(4)", e.indirect(4), 3 + helper(4)],
  ["indirect(5)", e.indirect(5), 6 + helper(5)],
  ["crosscall(21)", e.crosscall(21), helper(21) * 2],
];
let bad = 0;
for (const [name, got, want] of rows) {
  const ok = got === want;
  if (!ok) bad++;
  console.log(`${ok ? "ok  " : "FAIL"} ${name} = ${got}${ok ? "" : ` (want ${want})`}`);
}
if (bad) process.exit(1);
console.log("wasm smoke OK");
