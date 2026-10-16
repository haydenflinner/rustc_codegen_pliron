// node bench.mjs pliron.wasm stock.wasm [reps] — time every k_* kernel in both
// modules (best of 5) and check the checksums agree.
import { readFileSync } from "node:fs";
const load = async (p) => (await WebAssembly.instantiate(readFileSync(p), {})).instance.exports;
const [pp, sp] = process.argv.slice(2, 4);
const reps = Number(process.argv[4] ?? 2000);
const only = process.argv[5];
const P = await load(pp);
const S = await load(sp);
P.init(7);
S.init(7);
const names = Object.keys(P).filter((n) => n.startsWith("k_") && (!only || n.includes(only)));
const time = (m, n, r) => {
  m[n](Math.max(1, r >> 4)); // warm up / tier up
  let best = Infinity;
  for (let i = 0; i < 5; i++) {
    const t = performance.now();
    m[n](r);
    best = Math.min(best, performance.now() - t);
  }
  return best;
};
let bad = 0;
let geo = 0;
console.log("kernel".padEnd(18), "pliron ms".padStart(10), "stock ms".padStart(10), "ratio".padStart(7));
for (const n of names) {
  P.init(7);
  S.init(7);
  const a = P[n](3), b = S[n](3);
  const ok = a === b;
  bad += !ok;
  const tp = time(P, n, reps), ts = time(S, n, reps);
  geo += Math.log(tp / ts);
  console.log(
    n.padEnd(18),
    tp.toFixed(2).padStart(10),
    ts.toFixed(2).padStart(10),
    (tp / ts).toFixed(2).padStart(7),
    ok ? "" : `  MISMATCH ${a} vs ${b}`,
  );
}
console.log(`geomean ratio ${Math.exp(geo / names.length).toFixed(3)}  (${bad} mismatches)`);
if (bad) process.exit(1);
