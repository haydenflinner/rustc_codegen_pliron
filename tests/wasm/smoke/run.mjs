// checks the smoke module: each row prints "ok   name = value" or exits 1.
import { readFileSync } from "node:fs";

const i = new WebAssembly.Instance(
  new WebAssembly.Module(readFileSync(process.argv[2])),
  {},
);
const e = i.exports;

const helper = (x) => (((x * 3 >>> 0) << 5 | (x * 3 >>> 0) >>> 27) >>> 0) ^ 0x1234;
// tape_hash over [Some(true), None, Some(false)]: m = [1, u64::MAX, 0].
const P = 0x0000_0100_0000_01B3n, MASK = (1n << 64n) - 1n;
let gh = 0xcbf2_9ce4_8422_2325n;
for (const m of [1n, MASK, 0n]) gh = ((gh ^ m) * P) & MASK;
const rows = [
  ["fib(20)", e.fib(20), 6765],
  ["sum_sq(100)", e.sum_sq(100), 338350n],
  ["hypot(3,4)", e.hypot(3, 4), 5],
  ["statics(6)", e.statics(6), 63],
  ["indirect(4)", e.indirect(4), 3 + helper(4)],
  ["indirect(5)", e.indirect(5), 6 + helper(5)],
  ["crosscall(21)", e.crosscall(21), helper(21) * 2],
  ["genes_hash()", e.genes_hash(), gh],
];
let bad = 0;
for (const [name, got, want] of rows) {
  const ok = got === want;
  if (!ok) bad++;
  console.log(`${ok ? "ok  " : "FAIL"} ${name} = ${got}${ok ? "" : ` (want ${want})`}`);
}
if (bad) process.exit(1);
console.log("wasm smoke OK");
