// node run.mjs <llvm.wasm> <pliron.wasm> [extra.wasm] [name-filter]
//
// Times every `k_<name>(iters)` export in the modules and prints a
// comparison table, the wasm counterpart of bench/run.sh's benchfmt
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
// `--best`: on a loaded machine (other jobs, a thread parked on an
// efficiency core) the median round-ratio wanders by tens of percent.
// Each layout then reports min(pliron samples) / min(LLVM samples): a
// sample only has to run undisturbed once per side. Use it when the
// default run shows wide [min..max] ranges or inflated LLVM times.
const BEST = args.includes("--best");
// `--perturb=N` (1..8, default 1 = off): also time each kernel in N-1
// perturbed copies of every module (LLVM's too) and report the median
// ratio over all of them, with the min..max range. Two V8 effects decide
// a kernel's speed with our code unchanged, and `--layouts` (tier-up
// order) covers neither (docs/history/2026-11-17-v8-placement.md):
// - TurboFan's wasm inlining budget for a function depends on module-wide
//   statistics that grow with the number of small functions: removing
//   five small functions (ICF) lowers `fib`'s budget from 146 to 144
//   nodes, it no longer inlines its own recursive call, and it runs
//   12.8 ms instead of 6.0 (`--trace-wasm-inlining`); adding two unused
//   functions back restores both. Copies append 3, 8 or 16 tiny unused
//   functions.
// - Apple cores run a hot loop faster when its header is 8-byte
//   aligned: `parse_only` takes 243 ms with an even number of 4-byte
//   instructions ahead of its loop and 259 ms with an odd number, over
//   16 consecutive shifts (dropping one `i32.and`, PLIRON_WASM_NOREALIGN,
//   is one such shift). Function starts are 32-byte aligned, so only
//   code inside a function moves a loop. Half the copies start every
//   function with one store (`i32.store 0 -> [64]`, one arm64
//   instruction, below any data), which shifts every loop by 4 bytes.
// N=2 gives one copy with both effects flipped, N=4 both parities with
// and without extra functions, N=8 the full grid. A change that only
// moves code (ICF, NOREALIGN) should be judged with N >= 4.
const PERTURB = Math.max(1, Math.min(8, Number(args.find((a) => a.startsWith("--perturb="))?.slice(10) ?? 1)));
const VARIANTS = [
  { fns: 0, pad: false },
  { fns: 8, pad: true },
  { fns: 0, pad: true },
  { fns: 8, pad: false },
  { fns: 3, pad: false },
  { fns: 3, pad: true },
  { fns: 16, pad: false },
  { fns: 16, pad: true },
].slice(0, PERTURB);

// The module `buf` with `fns` unused `unreachable` functions appended
// and, when `pad`, one `i32.store 0 -> [64]` at every function's entry.
function perturb(buf, { fns, pad }) {
  if (!fns && !pad) return buf;
  const b = buf;
  let p = 8;
  const uleb = () => {
    let v = 0, s = 0, c;
    do { c = b[p++]; v += (c & 0x7f) * 2 ** s; s += 7; } while (c & 0x80);
    return v;
  };
  const enc = (v) => {
    const o = [];
    do { let x = v & 0x7f; v = Math.floor(v / 128); o.push(v ? x | 0x80 : x); } while (v);
    return o;
  };
  const skipLimits = () => { const f = uleb(); uleb(); if (f & 1) uleb(); };
  const out = [b.subarray(0, 8)];
  let nimp = 0, firstType = 0;
  const PAD = [0x41, 0xc0, 0x00, 0x41, 0x00, 0x36, 0x02, 0x00];
  while (p < b.length) {
    const hdr = p;
    const id = b[p++];
    const size = uleb();
    const start = p, end = p + size;
    if (id === 2) {
      const n = uleb();
      for (let i = 0; i < n; i++) {
        p += uleb(); p += uleb();
        const kind = b[p++];
        if (kind === 0) { nimp++; uleb(); }
        else if (kind === 1) { p++; skipLimits(); }
        else if (kind === 2) skipLimits();
        else if (kind === 3) p += 2;
        else if (kind === 4) { p++; uleb(); }
      }
    }
    if (id === 3 && fns) {
      p = start;
      const n = uleb();
      if (n) firstType = uleb();
      p = start; uleb();
      const body = [...enc(n + fns), ...b.subarray(p, end), ...new Array(fns).fill(enc(firstType)).flat()];
      out.push(Uint8Array.from([3, ...enc(body.length)]), Uint8Array.from(body));
      p = end;
      continue;
    }
    if (id === 10) {
      p = start;
      const n = uleb();
      const parts = [Uint8Array.from(enc(n + fns))];
      for (let i = 0; i < n; i++) {
        const sz = uleb();
        const bs = p, be = p + sz;
        let fb = b.subarray(bs, be);
        if (pad) {
          const save = p;
          const nl = uleb();
          for (let k = 0; k < nl; k++) {
            uleb();
            const t = b[p++];
            if (t === 0x63 || t === 0x64) uleb(); // (ref null? ht)
          }
          const at = p - bs;
          p = save;
          fb = Uint8Array.from([...fb.subarray(0, at), ...PAD, ...fb.subarray(at)]);
        }
        parts.push(Uint8Array.from(enc(fb.length)), fb);
        p = be;
      }
      for (let i = 0; i < fns; i++) parts.push(Uint8Array.from([3, 0, 0x00, 0x0b]));
      const len = parts.reduce((a, x) => a + x.length, 0);
      out.push(Uint8Array.from([10, ...enc(len)]), ...parts);
      p = end;
      continue;
    }
    out.push(b.subarray(hdr, end));
    p = end;
  }
  return Buffer.concat(out);
}
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
async function load(p, salt = 0, variant = VARIANTS[0]) {
  let buf = readFileSync(p);
  const bytes = buf.length;
  buf = perturb(buf, variant);
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
  // Layout 0 of variant 0 reuses the first instances; every other
  // (variant, layout) re-instantiates.
  const per = [];
  for (let v = 0; v < VARIANTS.length; v++) {
    for (let j = 0; j < LAYOUTS; j++) {
      const fresh = v || j;
      const salt = v * LAYOUTS + j;
      const Lj = fresh ? await load(llvmPath, salt, VARIANTS[v]) : L;
      const mj = fresh ? await Promise.all(mods.map((m) => load(m.path, salt, VARIANTS[v]))) : mods;
      if (j) {
        warm(Lj.e, name, j);
        for (const m of mj) warm(m.e, name, j);
      }
      if (v) {
        // A perturbed copy must compute what the original did.
        Lj.e[`${name}_prep`]?.();
        const la = Lj.e[name](1);
        for (const m of mj) {
          m.e[`${name}_prep`]?.();
          const pa = m.e[name](1);
          if (la !== pa) note = `  (ret differs in copy ${v}: ${la} vs ${pa})`;
        }
      }
      per.push(measure(name, Lj, mj));
    }
  }
  const ls = per.flatMap((x) => x.ls);
  const pss = mods.map((_, i) => per.flatMap((x) => x.pss[i]));
  const rrs = mods.map((_, i) =>
    per.map((x) => (BEST ? Math.min(...x.pss[i]) / Math.min(...x.ls) : median(x.rrs[i]))),
  );
  const lm = BEST ? Math.min(...ls) : median(ls);
  let row = `${tag.padEnd(14)} ${ms(lm)}`;
  mods.forEach((m, i) => {
    const pm = BEST ? Math.min(...pss[i]) : median(pss[i]);
    const r = median(rrs[i]);
    ratios[i].push(r);
    row += ` ${ms(pm)} ${pct(r)}`;
    if (per.length > 1) row += ` [${pct(Math.min(...rrs[i])).trim()}..${pct(Math.max(...rrs[i])).trim()}]`;
  });
  console.log(`${row}${note}`);
}
const geomean = (rs) => Math.exp(rs.reduce((a, r) => a + Math.log(r), 0) / rs.length);
if (ratios[0].length) {
  console.log(
    `\n${mods.map((m, i) => `geomean ${m.tag}/llvm = ${geomean(ratios[i]).toFixed(3)}`).join("   ")} (${ratios[0].length} kernels)`,
  );
}
