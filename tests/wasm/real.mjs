// node real.mjs pliron.wasm stock.wasm — time every program of tests/wasm/real in both modules.
import { readFileSync } from "node:fs";
const load = async (p) => {
  const m = new WebAssembly.Module(readFileSync(p));
  const imports = WebAssembly.Module.imports(m);
  if (imports.length) throw new Error("unexpected imports: " + imports.map((i) => i.module + "." + i.name).join(" "));
  return new WebAssembly.Instance(m, {}).exports;
};
const P = await load(process.argv[2]);
const S = await load(process.argv[3]);
const names = ["sha256", "base64_crc", "mandelbrot", "nbody", "wordfreq", "records", "matmul", "sort", "sort_unstable", "sort_stable", "bsearch"];
const best = (m, i, reps) => {
  m.run(i, reps);
  let b = Infinity, r;
  for (let k = 0; k < 5; k++) {
    const t = performance.now();
    r = m.run(i, reps);
    b = Math.min(b, performance.now() - t);
  }
  return [b, r];
};
let bad = 0, geo = 0;
console.log("program".padEnd(12), "pliron ms".padStart(10), "llvm ms".padStart(10), "ratio".padStart(7));
for (let i = 0; i < P.count(); i++) {
  const reps = P.default_reps(i);
  const [tp, rp] = best(P, i, reps);
  const [ts, rs] = best(S, i, reps);
  const ok = rp === rs;
  bad += !ok;
  geo += Math.log(tp / ts);
  console.log(names[i].padEnd(12), tp.toFixed(2).padStart(10), ts.toFixed(2).padStart(10), (tp / ts).toFixed(2).padStart(7), ok ? "" : ` MISMATCH ${rp} vs ${rs}`);
}
console.log(`geomean ${Math.exp(geo / P.count()).toFixed(3)} (${bad} mismatches)`);
process.exit(bad ? 1 : 0);
