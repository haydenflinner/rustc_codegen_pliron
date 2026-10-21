// node run.mjs <llvm.wasm> <pliron.wasm> [extra.wasm] [name-filter]
//
// Times every `k_<name>(iters)` export in the modules and prints a
// comparison table, the wasm counterpart of bench/run.sh's criterion
// run. `k_<name>_prep` (kernels whose input is consumed) is called
// before each timed call and is not part of the timing. An optional
// third module adds a second comparison column beside pliron.
//
// `--layouts=N` (default 1): V8 places TurboFan code in tier-up order,
// and a hot loop's address alone can swing a tight kernel ±25%
// (chars_alpha: byte-identical machine code, 4.0 vs 5.2 ms). Layout j
// times each kernel in fresh instances after tiering up j cheap other
// kernels first, which shifts its code; the row reports the median
// ratio across layouts and the min..max range.
import { readFileSync } from "node:fs";
import { performance } from "node:perf_hooks";

const args = process.argv.slice(2);
const LAYOUTS = Number(args.find((a) => a.startsWith("--layouts="))?.slice(10) ?? 1);
const filter = args.find((a) => !a.endsWith(".wasm") && !a.startsWith("--")) ?? "";
const [llvmPath, plirPath, extraPath] = args.filter((a) => a.endsWith(".wasm"));
if (!llvmPath || !plirPath) {
  console.error("usage: node run.mjs <llvm.wasm> <pliron.wasm> [extra.wasm] [name-filter]");
  process.exit(2);
}

const SAMPLES = 9;
const TARGET_MS = 150; // per timed call

// `salt` > 0 appends a custom section so V8's native-module cache can't
// hand back the code (and code placement) of an earlier instance.
async function load(p, salt = 0) {
  let buf = readFileSync(p);
  const bytes = buf.length;
  if (salt) {
    const pad = Buffer.from([0, 4 + salt, 3, 0x70, 0x61, 0x64, ...new Array(salt).fill(0)]);
    buf = Buffer.concat([buf, pad]);
  }
  const { instance } = await WebAssembly.instantiate(buf, {});
  return { e: instance.exports, bytes };
}
const WARMERS = ["k_eqbug", "k_fib", "k_chars_ascii", "k_sortw", "k_contains_rng"];
function warm(e, name, j) {
  for (const w of WARMERS.filter((w) => w !== name && typeof e[w] === "function").slice(0, j)) {
    for (let i = 0; i < 3; i++) {
      e[`${w}_prep`]?.();
      e[w](1);
    }
  }
}

const mods = [{ tag: "pliron", path: plirPath, ...(await load(plirPath)) }];
if (extraPath) mods.push({ tag: "extra", path: extraPath, ...(await load(extraPath)) });
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

// One layout's samples: warm-up, iters pick, interleaved rounds.
function measure(name, L, mods) {
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
  return { ls, pss, rrs };
}

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
  // Layout 0 reuses the first instances; layout j > 0 re-instantiates.
  const per = [];
  for (let j = 0; j < LAYOUTS; j++) {
    const Lj = j ? await load(llvmPath, j) : L;
    const mj = j ? await Promise.all(mods.map((m) => load(m.path, j))) : mods;
    if (j) {
      warm(Lj.e, name, j);
      for (const m of mj) warm(m.e, name, j);
    }
    per.push(measure(name, Lj, mj));
  }
  const ls = per.flatMap((x) => x.ls);
  const pss = mods.map((_, i) => per.flatMap((x) => x.pss[i]));
  const rrs = mods.map((_, i) => per.map((x) => median(x.rrs[i])));
  const lm = median(ls);
  let row = `${tag.padEnd(14)} ${ms(lm)}`;
  mods.forEach((m, i) => {
    const pm = median(pss[i]);
    const r = median(rrs[i]);
    ratios[i].push(r);
    row += ` ${ms(pm)} ${pct(r)}`;
    if (LAYOUTS > 1) row += ` [${pct(Math.min(...rrs[i])).trim()}..${pct(Math.max(...rrs[i])).trim()}]`;
  });
  console.log(`${row}${note}`);
}
const geomean = (rs) => Math.exp(rs.reduce((a, r) => a + Math.log(r), 0) / rs.length);
if (ratios[0].length) {
  console.log(
    `\n${mods.map((m, i) => `geomean ${m.tag}/llvm = ${geomean(ratios[i]).toFixed(3)}`).join("   ")} (${ratios[0].length} kernels)`,
  );
}
