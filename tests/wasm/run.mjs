// node run.mjs lib.wasm — instantiate and check every export against expected values.
import { readFileSync } from "node:fs";
const { instance } = await WebAssembly.instantiate(readFileSync(process.argv[2]), {});
const e = instance.exports;
const checks = [
  ["fib(20)", () => e.fib(20), 6765],
  ["sum_squares(100)", () => e.sum_squares(100), 338350n],
  ["wide_mul", () => e.wide_mul(0xdeadbeefcafebaben, 0x123456789abcdefn), 0xfd5bdeeeb2a01dn ^ 0x7eb689f4ea447d62n],
  ["byte_ops(0x1f7)", () => e.byte_ops(0x1f7), 0xbffd07],
  ["hypot(3,4)", () => e.hypot(3, 4), 5],
  ["bump", () => (e.bump(2), e.bump(3)), 5],
  ["table_sum", () => e.table_sum(), 14],
  ["sort_check(7)", () => e.sort_check(7) > 0, true],
  ["indirect(4)", () => e.indirect(4), 5],
  ["indirect(5)", () => e.indirect(5), 15],
  ["fmt_len(42)", () => e.fmt_len(42), "x=42 f=6.000".length],
];
let bad = 0;
for (const [n, f, want] of checks) {
  let got;
  try { got = f(); } catch (err) { got = `trap: ${err.message}`; }
  const ok = got === want;
  bad += !ok;
  console.log(`${ok ? "ok  " : "FAIL"} ${n} = ${got}${ok ? "" : ` (want ${want})`}`);
}
if (bad) process.exit(1);
console.log("wasm OK");
