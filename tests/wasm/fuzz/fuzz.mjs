// node fuzz.mjs pliron.wasm stock.wasm — run every kN over many (n, off) pairs
// in both modules from identical data and compare results and a hash of all
// arrays afterwards.
import { readFileSync } from "node:fs";
const load = async (p) => (await WebAssembly.instantiate(readFileSync(p), {})).instance.exports;
const P = await load(process.argv[2]);
const S = await load(process.argv[3]);
const count = P.count();
const shapes = [[0, 0], [1, 0], [3, 1], [7, 3], [15, 0], [16, 0], [17, 2], [33, 5], [64, 0], [100, 1], [257, 2], [1000, 0], [4000, 7]];
let bad = 0;
for (let k = 0; k < count; k++) {
  const f = `k${k}`;
  for (const [n, off] of shapes) {
    for (const seed of [11, 99]) {
      P.init(seed);
      S.init(seed);
      let a, b;
      try { a = P[f](n, off); } catch (e) { a = `trap:${e.message}`; }
      try { b = S[f](n, off); } catch (e) { b = `trap:${e.message}`; }
      const ha = P.hash(), hb = S.hash();
      if (a !== b || ha !== hb) {
        bad++;
        console.log(`MISMATCH ${f}(n=${n}, off=${off}, seed=${seed}): ${a}/${ha} vs ${b}/${hb}`);
        break;
      }
    }
  }
}
console.log(bad ? `${bad} mismatches in ${count} kernels` : `fuzz OK (${count} kernels)`);
process.exit(bad ? 1 : 0);
