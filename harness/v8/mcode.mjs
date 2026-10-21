// node --print-wasm-code-function-index=N mcode.mjs mod.wasm kernel iters
import { readFileSync } from "node:fs";
const [p, k, it] = process.argv.slice(2);
const { instance } = await WebAssembly.instantiate(readFileSync(p), {});
const e = instance.exports;
for (let i = 0; i < 6; i++) { e[`${k}_prep`]?.(); e[k](Number(it)); }
