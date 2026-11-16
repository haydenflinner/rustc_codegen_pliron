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
// fops64 / fops32: float operations against JS (IEEE double; f32 through
// Math.fround, exact for one correctly rounded operation). Any NaN equals
// any NaN; min/max of equal operands (the two zeros) and copysign from a
// NaN are left out, Rust does not pin those.
const FV = [0, -0, 1, -1, 0.5, 1.5, 2.5, -2.5, 0.1, 1 / 3, 123456789.125, 1e-310, 1e308, -1e308,
  2 ** 31, -(2 ** 31) - 1, 2 ** 32, 2 ** 63, -(2 ** 63), 2 ** 64, Infinity, -Infinity, NaN];
const sat = (x, lo, hi) => (Number.isNaN(x) ? 0n : x <= Number(lo) ? lo : x >= Number(hi) ? hi : BigInt(Math.trunc(x)));
const neg0 = (x) => Object.is(x, -0) || x < 0;
const FOPS = [
  (a, b) => a + b, (a, b) => a - b, (a, b) => a * b, (a, b) => a / b,
  (a) => Math.sqrt(a), (a) => Math.abs(a), (a) => -a,
  (a) => Math.floor(a), (a) => Math.ceil(a), (a) => Math.trunc(a),
  (a, b) => (Number.isNaN(b) ? undefined : neg0(b) ? -Math.abs(a) : Math.abs(a)),
  (a, b) => (Number.isNaN(a) ? b : Number.isNaN(b) ? a : a === b ? undefined : Math.min(a, b)),
  (a, b) => (Number.isNaN(a) ? b : Number.isNaN(b) ? a : a === b ? undefined : Math.max(a, b)),
  (a, b) => (a < b) | (a <= b) << 1 | (a > b) << 2 | (a >= b) << 3 | (a === b) << 4 | (a !== b) << 5,
  (a) => Number(sat(a, -(2n ** 31n), 2n ** 31n - 1n)), (a) => Number(sat(a, 0n, 2n ** 32n - 1n)),
  (a) => Number(sat(a, -(2n ** 63n), 2n ** 63n - 1n)), (a) => Number(sat(a, 0n, 2n ** 64n - 1n)),
];
const same = (x, y) => Object.is(x, y) || (Number.isNaN(x) && Number.isNaN(y));
let nf = 0;
for (const a of FV) {
  for (const b of FV) {
    FOPS.forEach((f, op) => {
      const w64 = f(a, b);
      if (w64 !== undefined) {
        rows.push([`fops64 op ${op} (${a}, ${b}) = ${e.fops64(a, b, op)} want ${w64}`, same(e.fops64(a, b, op), w64), true]);
        nf++;
      }
      const [a32, b32] = [Math.fround(a), Math.fround(b)];
      const w = f(a32, b32);
      if (w !== undefined) {
        const w32 = Math.fround(w);
        rows.push([`fops32 op ${op} (${a32}, ${b32}) = ${e.fops32(a32, b32, op)} want ${w32}`, same(e.fops32(a32, b32, op), w32), true]);
        nf++;
      }
    });
    rows.push([`fops64 op 18 (${a})`, same(e.fops64(a, b, 18), Math.fround(a)), true]);
    rows.push([`fops32 op 18 (${a})`, same(e.fops32(Math.fround(a), 0, 18), Math.fround(Math.fround(a) * 0.5)), true]);
    nf += 2;
  }
}
let bad = 0;
for (const [name, got, want] of rows) {
  const ok = got === want;
  if (!ok) bad++;
  if (!ok || !/^((cmp|ops)128|fops)/.test(name)) {
    console.log(`${ok ? "ok  " : "FAIL"} ${name} = ${got}${ok ? "" : ` (want ${want})`}`);
  }
}
if (bad) process.exit(1);
console.log(`ok   cmp128: ${W.length * W.length} pairs`);
console.log(`ok   ops128: ${nops} results`);
console.log(`ok   fops64/fops32: ${nf} results`);
console.log("wasm smoke OK");
