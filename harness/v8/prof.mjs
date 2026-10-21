// node --cpu-prof --cpu-prof-dir=D prof.mjs mod.wasm kernel iters reps
import { readFileSync } from "node:fs";
const [p, k, it, reps] = process.argv.slice(2);
const { instance } = await WebAssembly.instantiate(readFileSync(p), {});
const e = instance.exports;
for (let i = 0; i < Number(reps); i++) { e[`${k}_prep`]?.(); e[k](Number(it)); }
