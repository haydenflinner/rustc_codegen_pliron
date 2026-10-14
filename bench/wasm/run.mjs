// node run.mjs <llvm.wasm> <pliron.wasm> [name-filter]
//
// Times every `k_<name>(iters)` export in both modules and prints a
// comparison table, the wasm counterpart of bench/run.sh's criterion
// run. `k_<name>_prep` (kernels whose input is consumed) is called
// before each timed call and is not part of the timing.
import { readFileSync } from "node:fs";
import { performance } from "node:perf_hooks";

const [llvmPath, plirPath, filter = ""] = process.argv.slice(2);
if (!llvmPath || !plirPath) {
  console.error("usage: node run.mjs <llvm.wasm> <pliron.wasm> [name-filter]");
  process.exit(2);
}

const SAMPLES = 5;
const TARGET_MS = 150; // per timed call

async function load(p) {
  const buf = readFileSync(p);
  const { instance } = await WebAssembly.instantiate(buf, {});
  return { e: instance.exports, bytes: buf.length };
}

const L = await load(llvmPath);
const P = await load(plirPath);

// Call k_<name>_prep if present, then time e[k](iters).
function timed(e, name, iters) {
  e[`${name}_prep`]?.();
  const t0 = performance.now();
  e[name](iters);
  return performance.now() - t0;
}
const median = (a) => a.slice().sort((x, y) => x - y)[a.length >> 1];

const kernels = Object.keys(L.e)
  .filter((n) => n.startsWith("k_") && !n.endsWith("_prep") && typeof L.e[n] === "function")
  .filter((n) => n.slice(2).includes(filter))
  .sort();

console.log(`llvm   ${llvmPath} (${L.bytes} B)`);
console.log(`pliron ${plirPath} (${P.bytes} B)`);
console.log("");
console.log(`${"kernel".padEnd(14)} ${"LLVM".padStart(9)} ${"pliron".padStart(9)} ${"change".padStart(8)}`);

const ratios = [];
for (const name of kernels) {
  const tag = name.slice(2);
  if (typeof P.e[name] !== "function") {
    console.log(`${tag.padEnd(14)}  missing from pliron module`);
    continue;
  }
  // Correctness cross-check: one call's accumulator must agree.
  let note = "";
  try {
    L.e[`${name}_prep`]?.();
    const la = L.e[name](1);
    P.e[`${name}_prep`]?.();
    const pa = P.e[name](1);
    if (la !== pa) note = `  (ret differs: ${la} vs ${pa})`;
  } catch (err) {
    console.log(`${tag.padEnd(14)}  trap: ${err.message}`);
    continue;
  }
  // Warm up (V8 Liftoff -> TurboFan tier-up), then pick iters for a
  // ~TARGET_MS call on the slower of the two single-call times.
  timed(L.e, name, 1);
  timed(P.e, name, 1);
  const t1 = timed(L.e, name, 1);
  const t2 = timed(P.e, name, 1);
  const iters = Math.max(1, Math.min(200, Math.round(TARGET_MS / Math.max(t1, t2, 0.01))));
  timed(L.e, name, iters);
  timed(P.e, name, iters);
  const ls = [];
  const ps = [];
  for (let s = 0; s < SAMPLES; s++) {
    ls.push(timed(L.e, name, iters) / iters);
    ps.push(timed(P.e, name, iters) / iters);
  }
  const lm = median(ls);
  const pm = median(ps);
  ratios.push(pm / lm);
  const chg = ((pm / lm - 1) * 100).toFixed(1);
  console.log(
    `${tag.padEnd(14)} ${(lm.toFixed(3) + "ms").padStart(9)} ${(pm.toFixed(3) + "ms").padStart(9)} ${(chg + "%").padStart(8)}${note}`,
  );
}
if (ratios.length) {
  const g = Math.exp(ratios.reduce((a, r) => a + Math.log(r), 0) / ratios.length);
  console.log(`\ngeomean pliron/llvm = ${g.toFixed(3)} (${ratios.length} kernels)`);
}
