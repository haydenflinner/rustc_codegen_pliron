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
// cmp128: every pair of a few 128-bit values, against BigInt.
const M64 = (1n << 64n) - 1n;
const W = [0n, 1n, 127n, 128n, M64, 1n << 64n, (1n << 64n) + 5n, (1n << 127n) - 1n, 1n << 127n, (1n << 128n) - 1n];
const sgn = (x) => (x >> 127n ? x - (1n << 128n) : x);
for (const a of W) {
  for (const b of W) {
    const [sa, sb] = [sgn(a), sgn(b)];
    const want = (a < b) | (a <= b) << 1 | (a > b) << 2 | (a >= b) << 3
      | (sa < sb) << 4 | (sa <= sb) << 5 | (sa > sb) << 6 | (sa >= sb) << 7;
    rows.push([`cmp128(${a}, ${b})`, e.cmp128(a & M64, a >> 64n, b & M64, b >> 64n), want]);
  }
}
let bad = 0;
for (const [name, got, want] of rows) {
  const ok = got === want;
  if (!ok) bad++;
  if (!ok || !name.startsWith("cmp128")) {
    console.log(`${ok ? "ok  " : "FAIL"} ${name} = ${got}${ok ? "" : ` (want ${want})`}`);
  }
}
if (bad) process.exit(1);
console.log(`ok   cmp128: ${W.length * W.length} pairs`);
console.log("wasm smoke OK");
