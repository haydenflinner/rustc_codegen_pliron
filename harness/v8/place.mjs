// node place.mjs mod.wasm kernel warm1,warm2,...  -> median ms of kernel after warming others
import { readFileSync } from "node:fs";
import { performance } from "node:perf_hooks";
const [p, k, warm] = process.argv.slice(2);
const { instance } = await WebAssembly.instantiate(readFileSync(p), {});
const e = instance.exports;
for (const w of (warm || "").split(",").filter(Boolean)) { for (let i = 0; i < 3; i++) { e[`${w}_prep`]?.(); e[w](1); } }
const t = [];
for (let i = 0; i < 15; i++) { e[`${k}_prep`]?.(); const t0 = performance.now(); e[k](40); t.push(performance.now() - t0); }
t.sort((a, b) => a - b); console.log((t[7] / 40).toFixed(3));
