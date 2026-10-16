// node run.mjs <llvm.wasm> <pliron.wasm> [dialect.wasm] [name-filter]
//
// Times every `k_<name>(iters)` export in the modules and prints a
// comparison table, the wasm counterpart of bench/run.sh's criterion
// run. `k_<name>_prep` (kernels whose input is consumed) is called
// before each timed call and is not part of the timing. With a third
// module (the PLIRON_WASM_CLIF=0 build) a second comparison column
// shows the dialect->waffle fallback beside the default CLIF path.
import { readFileSync } from "node:fs";
import { performance } from "node:perf_hooks";

const args = process.argv.slice(2);
const filter = args.find((a) => !a.endsWith(".wasm")) ?? "";
const [llvmPath, plirPath, dialPath] = args.filter((a) => a.endsWith(".wasm"));
if (!llvmPath || !plirPath) {
  console.error("usage: node run.mjs <llvm.wasm> <pliron.wasm> [dialect.wasm] [name-filter]");
  process.exit(2);
}

const SAMPLES = 9;
const TARGET_MS = 150; // per timed call

async function load(p) {
  const buf = readFileSync(p);
  const { instance } = await WebAssembly.instantiate(buf, {});
  return { e: instance.exports, bytes: buf.length };
}

const mods = [{ tag: "pliron", path: plirPath, ...(await load(plirPath)) }];
if (dialPath) mods.push({ tag: "dialect", path: dialPath, ...(await load(dialPath)) });
const L = await load(llvmPath);

// Call k_<name>_prep if present, then time e[k](iters).
function timed(e, name, iters) {
  e[`${name}_prep`]?.();
  const t0 = performance.now();
  e[name](iters);
  return performance.now() - t0;
}
const median = (a) => a.slice().sort((x, y) => x - y)[a.length >> 1];
const ms = (t) => (t.toFixed(3) + "ms").padStart(9);
const pct = (r) => (((r - 1) * 100).toFixed(1) + "%").padStart(8);

console.log(`llvm   ${llvmPath} (${L.bytes} B)`);
for (const m of mods) console.log(`${m.tag.padEnd(7)}${m.path} (${m.bytes} B)`);
console.log("");
console.log(
  `${"kernel".padEnd(14)} ${"LLVM".padStart(9)}${mods.map((m) => ` ${m.tag.padStart(9)} ${"change".padStart(8)}`).join("")}`,
);

const ratios = mods.map(() => []);
for (const name of Object.keys(L.e)
  .filter((n) => n.startsWith("k_") && !n.endsWith("_prep") && typeof L.e[n] === "function")
  .filter((n) => n.slice(2).includes(filter))
  .sort()) {
  const tag = name.slice(2);
  if (mods.some((m) => typeof m.e[name] !== "function")) {
    console.log(`${tag.padEnd(14)}  missing from a pliron module`);
    continue;
  }
  // Correctness cross-check: one call's accumulator must agree.
  let note = "";
  try {
    L.e[`${name}_prep`]?.();
    const la = L.e[name](1);
    for (const m of mods) {
      m.e[`${name}_prep`]?.();
      const pa = m.e[name](1);
      if (la !== pa) note = `  (ret differs: ${la} vs ${pa})`;
    }
  } catch (err) {
    console.log(`${tag.padEnd(14)}  trap: ${err.message}`);
    continue;
  }
  // Warm up (V8 Liftoff -> TurboFan tier-up), then pick iters for a
  // ~TARGET_MS call on the slowest single-call time.
  timed(L.e, name, 1);
  for (const m of mods) timed(m.e, name, 1);
  const ts = [timed(L.e, name, 1), ...mods.map((m) => timed(m.e, name, 1))];
  const iters = Math.max(1, Math.min(200, Math.round(TARGET_MS / Math.max(...ts, 0.01))));
  timed(L.e, name, iters);
  for (const m of mods) timed(m.e, name, iters);
  // Interleave samples round-robin across modules: a load burst hits
  // every column in the same round, so the per-round ratio stays
  // meaningful even when absolute times inflate. Report the median
  // round-ratio rather than the ratio of medians.
  const ls = [];
  const pss = mods.map(() => []);
  const rrs = mods.map(() => []);
  for (let s = 0; s < SAMPLES; s++) {
    const lt = timed(L.e, name, iters) / iters;
    ls.push(lt);
    mods.forEach((m, i) => {
      const pt = timed(m.e, name, iters) / iters;
      pss[i].push(pt);
      rrs[i].push(pt / lt);
    });
  }
  const lm = median(ls);
  let row = `${tag.padEnd(14)} ${ms(lm)}`;
  mods.forEach((m, i) => {
    const pm = median(pss[i]);
    const r = median(rrs[i]);
    ratios[i].push(r);
    row += ` ${ms(pm)} ${pct(r)}`;
  });
  console.log(`${row}${note}`);
}
const geomean = (rs) => Math.exp(rs.reduce((a, r) => a + Math.log(r), 0) / rs.length);
if (ratios[0].length) {
  console.log(
    `\n${mods.map((m, i) => `geomean ${m.tag}/llvm = ${geomean(ratios[i]).toFixed(3)}`).join("   ")} (${ratios[0].length} kernels)`,
  );
}
