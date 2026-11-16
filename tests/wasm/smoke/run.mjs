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
// `ctors()` is the first export called: the three `.init_array`
// constructors must already have run, by priority (b.rs's 100, then a.rs's
// 200 and its default 65535), giving the digits 1, 2, 3.
const rows = [
  ["ctors() first", e.ctors(), 123],
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
// ops128: the same values through every 128-bit operation.
const M128 = (1n << 128n) - 1n;
const rotl = (x, n) => ((x << n) | (x >> (128n - n))) & M128;
const clz = (x) => { let n = 0n; for (let i = 127n; i >= 0n && !((x >> i) & 1n); i--) n++; return n; };
const ctz = (x) => { let n = 0n; for (let i = 0n; i < 128n && !((x >> i) & 1n); i++) n++; return n; };
const pop = (x) => { let n = 0n; for (let i = 0n; i < 128n; i++) n += (x >> i) & 1n; return n; };
const bswap = (x) => { let r = 0n; for (let i = 0n; i < 16n; i++) r = (r << 8n) | ((x >> (8n * i)) & 255n); return r; };
const s64 = (x) => BigInt.asIntN(64, x);
const OPS = [
  (a, b) => a + b, (a, b) => a - b, (a, b) => a & b, (a, b) => a | b, (a, b) => a ^ b,
  (a, b) => a << (b & 127n), (a, b) => a >> (b & 127n), (a, b) => sgn(a) >> (b & 127n),
  (a, b) => rotl(a, b & 127n), (a, b) => rotl(a, (128n - (b & 127n)) & 127n),
  (a) => clz(a), (a) => ctz(a), (a) => pop(a), (a) => -a, (a) => ~a,
  (a, b) => (a < b ? a : b), (a, b) => (a > b ? a : b),
  (a, b) => (sgn(a) < sgn(b) ? a : b), (a, b) => (sgn(a) > sgn(b) ? a : b),
  (a) => bswap(a), (a, b) => BigInt(a === b) | BigInt(a !== b) << 1n,
  (a, b) => a + b + (a & 1n), (a, b) => (a > b ? a - b : b - a), (a) => (sgn(a) < 0n ? -a : a),
  (a, b) => s64(a & M64) ^ ((b >> 64n) << 64n), (a, b) => (sgn(a) <= sgn(b) ? a ^ b : a - b),
];
let nops = 0;
for (const a of W) {
  for (const b of W) {
    OPS.forEach((f, op) => {
      const want = f(a, b) & M128;
      for (const half of [0, 1]) {
        const got = BigInt.asUintN(64, e.ops128(a & M64, a >> 64n, b & M64, b >> 64n, op, half));
        rows.push([`ops128 op ${op} half ${half} (${a}, ${b})`, got, half ? want >> 64n : want & M64]);
        nops++;
      }
    });
  }
}
// ... and they ran once, not on every export call.
rows.push(["ctors() last", e.ctors(), 123]);
rows.push(["ctor exports", Object.keys(e).filter((k) => /ctors$|^_initialize$/.test(k)).join(), "ctors"]);
let bad = 0;
for (const [name, got, want] of rows) {
  const ok = got === want;
  if (!ok) bad++;
  if (!ok || !/^(cmp|ops)128/.test(name)) {
    console.log(`${ok ? "ok  " : "FAIL"} ${name} = ${got}${ok ? "" : ` (want ${want})`}`);
  }
}
if (bad) process.exit(1);
console.log(`ok   cmp128: ${W.length * W.length} pairs`);
console.log(`ok   ops128: ${nops} results`);
console.log("wasm smoke OK");
