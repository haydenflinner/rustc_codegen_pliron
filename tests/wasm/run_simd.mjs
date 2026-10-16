// node run_simd.mjs wasmsimd.wasm [reference.wasm] — run every export; with a reference module
// (stock LLVM build) the two must agree on every result.
import { readFileSync } from "node:fs";
const load = async (p) => (await WebAssembly.instantiate(readFileSync(p), {})).instance.exports;
const e = await load(process.argv[2]);
const r = process.argv[3] ? await load(process.argv[3]) : null;
const tests = [
  ["arch_i32x4", [5, 9]], ["arch_i32x4", [0xfffffff0, 77]],
  ["arch_u8x16_sum", []], ["arch_count_byte", [0x41]], ["arch_count_byte", [0]],
  ["arch_f32x4", [1.5]], ["arch_f32x4", [-3.25]],
  ["arch_shuffle", [10]], ["arch_bitselect_any", []],
  ["psimd_dot", []], ["psimd_u8_max", []], ["psimd_f32", [0.75]],
  ["psimd_cmp_select", [0x80000000]], ["psimd_cmp_select", [0x10000000]],
  ["psimd_i16_sat", []], ["psimd_wide", []], ["psimd_small", []],
  ["loop_sum_u32", []], ["loop_sum_u8", []], ["loop_axpy", [3]],
  ["loop_count_eq", [0x41]], ["loop_max_u8", []],
  ["ops_int", [0]], ["ops_int", [5]], ["ops_i32", [3]], ["ops_i32", [20]],
  ["ops_i64", [9]], ["ops_i64", [0xdeadbeef]],
  ["ops_sat", [100]], ["ops_sat", [0]],
  ["ops_float", [1.25]], ["ops_float", [-7.75]], ["ops_f64", [2.5]], ["ops_f64", [-123.456]],
  ["ops_cast", [0]], ["ops_cast", [77]], ["ops_shuffle", [5]], ["ops_bits", []],
];
let bad = 0;
for (const x of [e, r]) if (x) x.fill(12345);
const call = (m, n, args) => { try { return m[n](...args); } catch (err) { return `trap: ${err.message}`; } };
for (const [n, args] of tests) {
  const got = call(e, n, args);
  const want = r ? call(r, n, args) : got;
  const ok = Object.is(got, want);
  bad += !ok;
  console.log(`${ok ? "ok  " : "FAIL"} ${n}(${args}) = ${got}${ok ? "" : ` (stock ${want})`}`);
}
if (bad) process.exit(1);
console.log("wasm simd OK");
