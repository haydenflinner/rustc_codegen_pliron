// node harness/dsa/bench.mjs [options] [name-filter ...]
//
// Runtime comparison of the dsa `literate-wasm` module (Loro CRDT page +
// the Mimas/"Nina" interpreter, a wasm-bindgen cdylib) built by stock
// rustc+LLVM and by the pliron backend. Both packages (harness/dsa/build.sh)
// are loaded into this one process and every workload is timed on each,
// interleaved, after a warm-up for V8's tier-up — the same shape as
// bench/wasm/run.mjs.
//
//   --llvm=DIR      LLVM package dir     (/tmp/dsa-bench/pkg-llvm-eval)
//   --pliron=DIR    pliron package dir   (/tmp/dsa-bench/pkg-pliron-eval)
//                   (the -eval packages: features eval,squish,cad, see build.sh)
//   --extra=DIR     a second pliron build, compared against LLVM as well
//   --rounds=N      timed rounds per workload (9)
//   --target-ms=N   one timed call is `iters` workload calls, ~N ms (150)
//   --json=FILE     also write the table as JSON
//   --list          print the workloads and exit
//   --no-probe      skip the self-checks (below)
//   --side=TAG --loop=N   profile mode: load only that side (llvm, pliron,
//                   extra), warm up, then run each selected workload N
//                   times. Use under `node --cpu-prof` (harness/dsa/prof.sh).
//
// Columns: median ms per workload call on each side; `ratio` = median of
// the per-round ratios other/llvm with its min..max over the rounds;
// `best` = fastest round of the other side / fastest round of LLVM. On a
// loaded machine `best` is the steadier number (load only ever adds
// time); the two agree when the machine is quiet. To see the noise floor,
// pass the LLVM package on both sides (`--pliron=/tmp/dsa-bench/pkg-llvm`).
//
// Every workload call returns a result; the i-th call's result must be the
// same on every side (the call sequence is identical on each). A
// difference prints a MISMATCH line: same source, same inputs, so it is a
// miscompile on one side (or nondeterminism in the workload — the
// workloads here pin peer ids, seeds and inputs, and LLVM agrees with
// itself across runs).
//
// Self-checks: a second, instrumented instance of each side counts the
// entries from JS into the module and the time spent inside it, and a
// no-op export is timed on its own. A row gets a note when more than 30%
// of a call is outside the module (`hash_text` is 62% JS string copies and
// swings with the interleaving), or when the export wrappers themselves
// are 5% or more of a call on some side (the CRDT rows: rust-lld's
// `command_export` wrappers rerun the static constructors on every
// entry). A second geomean leaves the noted rows out.
//
// Workloads whose cargo feature is missing from either package
// (`capabilities()`) are skipped and named.
import { readFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { performance } from "node:perf_hooks";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const W = join(HERE, "workloads");
const argv = process.argv.slice(2);
const opt = (k, d) => argv.find((a) => a.startsWith(`--${k}=`))?.slice(k.length + 3) ?? d;
const flag = (k) => argv.includes(`--${k}`);
const filters = argv.filter((a) => !a.startsWith("--"));
const ROUNDS = Number(opt("rounds", 9));
const TARGET_MS = Number(opt("target-ms", 150));
const BASE = process.env.DSA_BENCH ?? "/tmp/dsa-bench";

// ---------------------------------------------------------------- loading
let generation = 0;
// With `stat`, the instance's exports are wrapped: `stat.calls` counts
// every entry from JS into the module (allocator calls of the glue
// included) and `stat.inside` is the time between an outermost entry and
// its return. Such an instance is for the self-checks only, never timed.
async function load(tag, dir, stat = null) {
  dir = resolve(dir);
  const wasm = readFileSync(join(dir, "literate_wasm_bg.wasm"));
  const t0 = performance.now();
  const instantiate = WebAssembly.instantiate;
  if (stat) {
    WebAssembly.instantiate = async (bytes, imports) => {
      const r = await instantiate(bytes, imports);
      const real = (r.instance ?? r).exports;
      const wrapped = {};
      for (const [k, v] of Object.entries(real)) {
        wrapped[k] = typeof v !== "function" ? v : function (...a) {
          stat.calls++;
          if (stat.depth++ === 0) stat.t0 = performance.now();
          try { return v.apply(this, a); } finally { if (--stat.depth === 0) stat.inside += performance.now() - stat.t0; }
        };
      }
      stat.real = real;
      return { instance: { exports: wrapped }, module: r.module };
    };
  }
  // The query string keeps two packages with the same glue apart in the
  // ESM cache even if the dirs are symlinks to one another.
  // A new generation is a new instance: a side is reloaded after a trap,
  // because a Rust panic or a stack overflow leaves the old instance's
  // shadow stack and borrow flags in an unknown state.
  const m = await import(pathToFileURL(join(dir, "literate_wasm.js")).href + `?side=${tag}&gen=${generation++}`);
  try { await m.default({ module_or_path: wasm }); } finally { WebAssembly.instantiate = instantiate; }
  let info = "";
  try { info = readFileSync(join(dir, "BUILDINFO"), "utf8"); } catch {}
  return { tag, dir, m, bytes: wasm.length, initMs: performance.now() - t0, caps: new Set(m.capabilities()), info };
}

// ---------------------------------------------------------------- helpers
const text = (f) => readFileSync(join(W, f), "utf8");
const sha = (s) => createHash("sha1").update(s).digest("hex").slice(0, 16);
// Deterministic LCG so every side sees the same "random" edit script.
function lcg(seed) {
  let s = seed >>> 0;
  return (n) => { s = (Math.imul(s, 1103515245) + 12345) >>> 0; return (s >>> 8) % n; };
}

// One fresh REPL session per call: parse + typecheck + compile of the
// program text, then the VM run (the run dominates: each program is sized
// to ~100 ms and stays inside the REPL's 20M-op budget).
const vm = (file) => ({
  feature: "eval",
  what: `ReplSession.eval(${file})`,
  run({ m }) {
    const r = new m.ReplSession();
    const out = r.eval(text(file));
    r.free();
    const o = JSON.parse(out);
    if (o.err || !o.committed) throw new Error(`eval failed: ${String(o.err).slice(0, 300)}`);
    return o.out.trim();
  },
});

const GAME_INPUT = (i, seed) => JSON.stringify({
  held: i % 40 < 12 ? ["ArrowLeft", "ArrowUp"] : i % 40 < 20 ? [" "] : i % 40 < 30 ? ["ArrowRight", " "] : ["ArrowDown"],
  mouseX: -1e4, mouseY: -1e4, mouseDown: false, dt: 1 / 60, events: [], pads: [], touches: [],
  ...(seed == null ? {} : { seed }),
});
// A running game session, stepped `frames` frames from the same snapshot
// on every call (gameSnapAt/gameRestoreAt), so each call is the same work.
const game = (file, frames) => ({
  feature: "eval",
  what: `gameRestoreAt + ${frames} x gameFrame(${file})`,
  prep({ m }) {
    const page = new m.PageHandle();
    page.gameStart(text(file), null);
    for (let i = 0; i < 20; i++) page.gameFrame(GAME_INPUT(i, i === 0 ? 7 : null), null);
    return { page, snap: page.gameSnapAt(0) };
  },
  run(_, st) {
    st.page.gameRestoreAt(0, st.snap);
    let last = "";
    for (let i = 0; i < frames; i++) last = st.page.gameFrame(GAME_INPUT(20 + i, null), null);
    const o = JSON.parse(last);
    if (o.error) throw new Error(`game fault: ${String(o.error).slice(0, 300)}`);
    return `${o.prims.length} prims ${sha(last)}`;
  },
});

const TYPST = (i, sections) => Array.from({ length: sections }, (_, k) =>
  `= Section ${k} of run ${i}\nLorem ipsum dolor sit amet, $x^${k} + sum_(j=0)^n j = (n(n+1))/2$ consectetur adipiscing elit, ` +
  `sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.\n\n- item one\n- item *two* and _three_\n`).join("\n");

// ---------------------------------------------------------------- workloads
// run(side, state, i) -> result string. `i` is the call index (0, 1, 2, ..
// identical on every side); workloads that hit a memoizing layer (Typst's
// comemo) fold it into their input so no call is a cache hit.
const WORKLOADS = {
  // --- the Mimas VM: one language feature each ---
  vm_loop: vm("vm_loop.mim"),
  vm_fib_rec: vm("vm_fib_rec.mim"),
  vm_mandelbrot: vm("vm_mandelbrot.mim"),
  vm_physics: vm("vm_physics.mim"),
  vm_sieve: vm("vm_sieve.mim"),
  vm_collections: vm("vm_collections.mim"),
  vm_strings: vm("vm_strings.mim"),
  vm_match: vm("vm_match.mim"),
  vm_closures: vm("vm_closures.mim"),

  // --- the compile path on a real page (pong + its kits, 1.3k lines) ---
  compile_page: {
    feature: "eval",
    what: "new DebugSession(page_pong) + compileInfo: parse, typecheck, bytecode; nothing runs",
    run({ m }) {
      const d = new m.DebugSession(text("page_pong.mimas"));
      const info = d.compileInfo();
      d.free();
      return sha(info);
    },
  },
  dims_page: {
    feature: "eval",
    what: "dimsSource(page_asteroids): the units/dimension checker over a page",
    prep: ({ m }) => ({ page: new m.PageHandle() }),
    run: (_, st) => sha(st.page.dimsSource(text("page_asteroids.mimas"))),
  },
  checks_page: {
    feature: "eval",
    what: "runChecksSource(page_tetris): compile, then run the page's 85 `check` lines",
    prep: ({ m }) => ({ page: new m.PageHandle() }),
    run(_, st) {
      const o = JSON.parse(st.page.runChecksSource(text("page_tetris.mimas")));
      if (o.compileError) throw new Error(`compile error: ${o.compileError.slice(0, 300)}`);
      return `${o.summary} ${sha(JSON.stringify(o.results.map((r) => [r.name, r.line, r.ok, r.message])))}`;
    },
  },
  live_page: {
    feature: "squish",
    what: "liveDrawsSource(page_asteroids): compile, run top level, draw every live_* fn",
    prep: ({ m }) => ({ page: new m.PageHandle() }),
    run(_, st) {
      const o = JSON.parse(st.page.liveDrawsSource(text("page_asteroids.mimas")));
      if (o.compileError) throw new Error(`compile error: ${o.compileError.slice(0, 300)}`);
      return `${o.scenes.map((s) => `${s.name}:${s.error ?? s.prims.length}`).join(",")} ${sha(JSON.stringify(o.scenes))}`;
    },
  },

  // --- game frames: the per-frame hot path of the web host ---
  game_tetris: game("page_tetris.mimas", 120),
  game_asteroids: { ...game("page_asteroids.mimas", 30), feature: "squish" },

  // --- the Loro document (no interpreter) ---
  crdt_edits: {
    feature: "page",
    what: "1500 spliceSource edits at random offsets, save(), load(), source()",
    run({ m }) {
      const rnd = lcg(12345);
      const p = m.PageHandle.empty();
      p.setPeerId("1");
      p.replaceSource("fn main() {\n}\n");
      let len = p.sourceLen();
      for (let i = 0; i < 1500; i++) {
        const at = rnd(len + 1);
        if (i % 5 === 4 && len > 20) {
          const d = Math.min(1 + rnd(4), len - at);
          p.spliceSource(at, d, "");
          len -= d;
        } else {
          const t = `let x${i % 97} = ${i};\n`.slice(0, 1 + rnd(12));
          p.spliceSource(at, 0, t);
          len += t.length;
        }
      }
      const src = p.source();
      const q = m.PageHandle.load(p.save());
      const back = q.source();
      p.free();
      q.free();
      return `${src.length} roundtrip=${back === src} ${sha(src)}`;
    },
  },
  crdt_sync: {
    feature: "page",
    what: "two peers, 400 concurrent edits, exportSince/importUpdate every 4th",
    run({ m }) {
      const rnd = lcg(999);
      const a = m.PageHandle.empty(), b = m.PageHandle.empty();
      a.setPeerId("1");
      b.setPeerId("2");
      a.replaceSource("alpha\n");
      b.importUpdate(a.exportSince(b.vvBytes()));
      for (let i = 0; i < 400; i++) {
        const w = i % 2 ? a : b, o = i % 2 ? b : a;
        w.spliceSource(rnd(w.sourceLen() + 1), 0, `w${i} `);
        if (i % 4 === 3) {
          o.importUpdate(w.exportSince(o.vvBytes()));
          w.importUpdate(o.exportSince(w.vvBytes()));
        }
      }
      b.importUpdate(a.exportSince(b.vvBytes()));
      a.importUpdate(b.exportSince(a.vvBytes()));
      const sa = a.source(), sb = b.source();
      a.free();
      b.free();
      return `converged=${sa === sb} ${sa.length} ${sha(sa)}`;
    },
  },
  ink_strokes: {
    feature: "page",
    what: "20 strokes x 100 pushPoint, strokesJson(), save()",
    run({ m }) {
      const p = m.PageHandle.empty();
      p.setPeerId("1");
      for (let k = 0; k < 20; k++) {
        const id = p.beginStroke("ink", 0.1, 0.2, 0.3, 1, 3, "pen", false, undefined);
        for (let i = 0; i < 100; i++) p.pushPoint(id, 10 + i * 1.5 + k, 20 + Math.sin(i / 7) * 30 + k * 3, 0.5, i * 8);
        p.endStroke(id);
      }
      const strokes = JSON.parse(p.strokesJson());
      const pts = strokes.map((s) => (s.points ?? s.pts ?? []).length);
      const bytes = p.save().length;
      p.free();
      // Stroke ids are random and the snapshot carries wall-clock time, so
      // only the shape is compared.
      return `${strokes.length} strokes, points ${sha(JSON.stringify(strokes.map((s) => s.points ?? s.pts ?? null)))} ${pts.reduce((x, y) => x + y, 0)} save~${Math.round(bytes / 4096)}p`;
    },
  },
  hash_text: {
    feature: "page",
    what: "hashText (blake3) of a 3.6 MB string, x8",
    prep: () => ({ big: "The quick brown fox jumps over the lazy dog. ".repeat(80000) }),
    run({ m }, st) {
      let h = "";
      for (let i = 0; i < 8; i++) h = m.hashText(st.big);
      return h;
    },
  },

  // --- the heavy optional stacks (default features only) ---
  tape_codec: {
    feature: "tape",
    what: "tapeEncode + tapeDecode of 20k input frames (Vortex/BtrBlocks)",
    prep() {
      const rnd = lcg(7);
      const frames = [];
      for (let i = 0; i < 20000; i++) {
        frames.push(JSON.stringify({ held: i % 50 < 25 ? ["ArrowLeft"] : [], mouseX: 100 + rnd(500), mouseY: 50 + rnd(300), mouseDown: rnd(10) === 0, dt: 0.016, events: [], pads: [] }));
      }
      return { fj: JSON.stringify(frames) };
    },
    run({ m }, st) {
      const blob = m.tapeEncode(st.fj);
      return `${blob.length} B ${sha(blob)} -> ${sha(m.tapeDecode(blob))}`;
    },
  },
  typeset_svg: {
    feature: "typeset",
    what: "typesetSvg: Typst compile + layout of 30 sections with math, to SVG",
    prep: ({ m }) => ({ page: new m.PageHandle() }),
    run(_, st, i) {
      const o = st.page.typesetSvg(TYPST(i, 30), 600, undefined);
      return `${o.length} ${sha(o)}`;
    },
  },
  typeset_raster: {
    feature: "typeset",
    what: "typesetMarkup: Typst compile + layout + tiny-skia raster of 8 sections",
    prep: ({ m }) => ({ page: new m.PageHandle() }),
    run(_, st, i) {
      const o = st.page.typesetMarkup(TYPST(i, 8), 600, undefined, 1, undefined, undefined);
      return `${o.length} ${sha(o)}`;
    },
  },
  polars_query: {
    feature: "pl",
    what: "ReplSession.eval(polars_query.mim): 20k-row CSV parse, join, group, aggregate, sort",
    run({ m }) {
      const r = new m.ReplSession();
      const o = JSON.parse(r.eval(text("polars_query.mim")));
      r.free();
      if (o.err || !o.committed) throw new Error(`eval failed: ${String(o.err).slice(0, 300)}`);
      return `${o.out.split("\n").length} lines ${sha(o.out)}`;
    },
  },
};

// ---------------------------------------------------------------- main
const names = Object.keys(WORKLOADS).filter((n) => !filters.length || filters.some((f) => n.includes(f)));
if (flag("list")) {
  for (const n of names) console.log(`${n.padEnd(16)} [${WORKLOADS[n].feature}] ${WORKLOADS[n].what}`);
  process.exit(0);
}

// First line of the error plus the innermost wasm frames of its stack.
function describe(e) {
  const head = String(e?.message ?? e).split("\n")[0];
  const frames = String(e?.stack ?? "").split("\n").filter((l) => l.includes("wasm://")).slice(0, 3)
    .map((l) => l.trim().replace(/^at /, "").replace(/ \(wasm:\/\/.*$/, "").slice(0, 120));
  return `${e?.name && !head.startsWith(e.name) ? `${e.name}: ` : ""}${head}${frames.length ? ` [in ${frames.join(" <- ")}]` : ""}`;
}
const median = (a) => a.slice().sort((x, y) => x - y)[a.length >> 1];
const geomean = (rs) => Math.exp(rs.reduce((a, r) => a + Math.log(r), 0) / rs.length);
const fmt = (t) => t.toFixed(t < 10 ? 3 : t < 100 ? 2 : 1).padStart(9);

// One workload on one side: state, call counter, results by call index.
function runner(side, name) {
  const w = WORKLOADS[name];
  const st = w.prep ? w.prep(side) : undefined;
  let i = 0;
  const results = [];
  return {
    results,
    // `n` workload calls; returns ms for all of them.
    call(n = 1) {
      let ms = 0;
      for (let k = 0; k < n; k++) {
        const t0 = performance.now();
        const r = w.run(side, st, i);
        ms += performance.now() - t0;
        results[i++] = r;
      }
      return ms;
    },
  };
}

// ---- profile mode
const profSide = opt("side", null);
if (profSide) {
  const dir = opt(profSide, join(BASE, `pkg-${profSide}-eval`));
  const side = await load(profSide, dir);
  const loops = Number(opt("loop", 20));
  for (const name of names) {
    if (!side.caps.has(WORKLOADS[name].feature)) continue;
    const r = runner(side, name);
    const ms = r.call(loops);
    console.log(`${profSide} ${name}: ${loops} calls, ${(ms / loops).toFixed(2)} ms/call, result ${r.results.at(-1)}`);
  }
  process.exit(0);
}

const sides = [await load("llvm", opt("llvm", join(BASE, "pkg-llvm-eval"))), await load("pliron", opt("pliron", join(BASE, "pkg-pliron-eval")))];
if (opt("extra", null)) sides.push(await load("extra", opt("extra")));
for (const s of sides) {
  console.log(`${s.tag.padEnd(7)}${s.dir}  ${s.bytes} B  init ${s.initMs.toFixed(0)} ms  caps ${[...s.caps].join(",")}  stamp ${s.m.buildStamp()}`);
}
if (new Set(sides.map((s) => s.m.buildStamp())).size > 1) {
  console.log("note: source stamps differ. The stamp is dsa HEAD + a hash of `git status --porcelain`, so a new untracked file between the two builds changes it too; compare the HEAD part.");
}
console.log(`rounds ${ROUNDS}, target ${TARGET_MS} ms per timed call, node ${process.version}`);

// ---- self-checks (`--no-probe` skips them)
// A second, instrumented instance of every side says what a row measures:
// how much of a call is spent inside the module at all, and how many
// times JS enters it. The entry cost is a no-op export timed on its own:
// rust-lld wraps every export of a cdylib in a `command_export` function
// that reruns the static constructors, so a row that enters the module
// thousands of times mostly times those wrappers.
const PROBE = !flag("no-probe");
const probes = [];
if (PROBE) {
  for (const s of sides) {
    const stat = { calls: 0, inside: 0, depth: 0, t0: 0, real: null };
    const side = await load(s.tag, s.dir, stat);
    const free = Object.entries(stat.real).find(([k]) => /^__wbindgen_free/.test(k))?.[1];
    let entryNs = null;
    if (free) {
      // `free(ptr, 0, align)` returns at once.
      const N = 200000;
      for (let i = 0; i < 20000; i++) free(0, 0, 1);
      entryNs = Infinity;
      for (let rep = 0; rep < 5; rep++) {
        const t0 = performance.now();
        for (let i = 0; i < N; i++) free(0, 0, 1);
        entryNs = Math.min(entryNs, (performance.now() - t0) * 1e6 / N);
      }
    }
    probes.push({ side, stat, entryNs });
  }
  console.log(`entry cost per export call (a no-op export): ${probes.map((p) => `${p.side.tag} ${p.entryNs == null ? "?" : p.entryNs.toFixed(0)} ns`).join(", ")}`);
}
// One workload on the instrumented instance: entries per call, the share
// of the call spent inside the module, and the entry cost as a share.
function probe(k, name) {
  const { side, stat, entryNs } = probes[k];
  const r = runner(side, name);
  r.call(2);
  stat.calls = 0; stat.inside = 0;
  const N = 2;
  const ms = r.call(N);
  return { calls: stat.calls / N, wasm: Math.min(1, stat.inside / ms), entry: entryNs == null ? 0 : stat.calls * entryNs / 1e6 / ms };
}
const noted = new Set();
console.log("");
const others = sides.slice(1);
console.log(`${"workload".padEnd(16)} ${"iters".padStart(5)} ${"llvm ms".padStart(9)}` +
  others.map((s) => ` ${(s.tag + " ms").padStart(9)} ${"ratio".padStart(6)} ${"min..max".padStart(12)} ${"best".padStart(6)}`).join(""));

const table = [];
const problems = [];
const ratios = others.map(() => []);
const bests = others.map(() => []);
for (const name of names) {
  const w = WORKLOADS[name];
  const missing = sides.filter((s) => !s.caps.has(w.feature)).map((s) => s.tag);
  if (missing.length) {
    console.log(`${name.padEnd(16)} skipped: feature '${w.feature}' not in ${missing.join(", ")}`);
    table.push({ name, skipped: `feature ${w.feature} missing in ${missing.join(",")}` });
    continue;
  }
  let rs;
  const fails = [];
  // Set-up and warm-up, per side; a side that throws is reported and the
  // workload is not timed.
  try {
    rs = sides.map((s) => {
      try { return runner(s, name); } catch (e) { fails.push(`${s.tag} prep: ${describe(e)}`); return null; }
    });
    if (!fails.length) {
      // Warm-up: at least 3 calls and 400 ms on each side (Liftoff ->
      // TurboFan), at most 8 calls.
      sides.forEach((s, k) => {
        try {
          let ms = 0, n = 0;
          while (n < 3 || (ms < 400 && n < 8)) { ms += rs[k].call(); n++; }
        } catch (e) { fails.push(`${s.tag}: ${describe(e)}`); }
      });
    }
  } catch (e) { fails.push(String(e)); }
  if (fails.length) {
    console.log(`${name.padEnd(16)} FAILED  ${fails.join(" | ")}`);
    problems.push(`${name}: ${fails.join(" | ")}`);
    table.push({ name, failed: fails });
    for (let k = 0; k < sides.length; k++) if (fails.some((f) => f.startsWith(sides[k].tag))) sides[k] = await load(sides[k].tag, sides[k].dir);
    continue;
  }
  // iters from the slower side's best warm call.
  const one = Math.max(...rs.map((r) => Math.min(r.call(), r.call())));
  const iters = Math.max(1, Math.min(100, Math.round(TARGET_MS / Math.max(one, 0.01))));
  const ts = sides.map(() => []);
  const rr = others.map(() => []);
  try {
    for (let round = 0; round < ROUNDS; round++) {
      // Alternate which side goes first so a drift in machine load does
      // not always land on the same column.
      const order = sides.map((_, k) => k);
      if (round & 1) order.reverse();
      const t = [];
      for (const k of order) t[k] = rs[k].call(iters) / iters;
      t.forEach((x, k) => ts[k].push(x));
      others.forEach((_, k) => rr[k].push(t[k + 1] / t[0]));
    }
  } catch (e) {
    console.log(`${name.padEnd(16)} FAILED while timing: ${describe(e)}`);
    problems.push(`${name}: while timing: ${describe(e)}`);
    table.push({ name, failed: [describe(e)] });
    for (let k = 0; k < sides.length; k++) sides[k] = await load(sides[k].tag, sides[k].dir);
    continue;
  }
  // Result check, every call index.
  let mism = null;
  for (let k = 1; k < sides.length && !mism; k++) {
    const n = Math.min(rs[0].results.length, rs[k].results.length);
    for (let i = 0; i < n; i++) {
      if (rs[0].results[i] !== rs[k].results[i]) { mism = { side: sides[k].tag, i, llvm: rs[0].results[i], other: rs[k].results[i] }; break; }
    }
  }
  const row = { name, iters, llvm_ms: median(ts[0]), llvm_best_ms: Math.min(...ts[0]), result: rs[0].results[0], sides: {} };
  let line = `${name.padEnd(16)} ${String(iters).padStart(5)} ${fmt(row.llvm_ms)}`;
  others.forEach((s, k) => {
    const r = median(rr[k]);
    // A workload whose result differs is not the same work: keep it out of the geomean.
    if (!mism) ratios[k].push(r);
    const best = Math.min(...ts[k + 1]) / Math.min(...ts[0]);
    if (!mism) bests[k].push(best);
    row.sides[s.tag] = { ms: median(ts[k + 1]), ratio: r, min: Math.min(...rr[k]), max: Math.max(...rr[k]), best, best_ms: Math.min(...ts[k + 1]) };
    line += ` ${fmt(median(ts[k + 1]))} ${r.toFixed(3).padStart(6)} ${`${Math.min(...rr[k]).toFixed(2)}..${Math.max(...rr[k]).toFixed(2)}`.padStart(12)} ${best.toFixed(3).padStart(6)}`;
  });
  if (PROBE) {
    try {
      const ps = sides.map((_, k) => probe(k, name));
      row.probe = Object.fromEntries(ps.map((p, k) => [sides[k].tag, p]));
      const notes = [];
      const wasm = Math.max(...ps.map((p) => p.wasm));
      if (wasm < 0.7) notes.push(`${((1 - wasm) * 100).toFixed(0)}% of a call is JS`);
      const ent = ps.map((p, k) => [sides[k].tag, p.entry]).filter(([, e]) => e >= 0.05);
      if (ent.length) notes.push(`${ps[0].calls.toFixed(0)} entries/call, entry cost ${ent.map(([t, e]) => `${t} ${(e * 100).toFixed(0)}%`).join(" ")}`);
      if (notes.length) { line += `  [${notes.join("; ")}]`; row.notes = notes; noted.add(name); }
    } catch (e) {
      line += `  [probe failed: ${describe(e)}]`;
      for (let k = 0; k < probes.length; k++) {
        const stat = { calls: 0, inside: 0, depth: 0, t0: 0, real: null };
        probes[k] = { side: await load(sides[k].tag, sides[k].dir, stat), stat, entryNs: probes[k].entryNs };
      }
    }
  }
  console.log(line);
  if (mism) {
    const msg = `MISMATCH ${name} call ${mism.i}: llvm=${JSON.stringify(String(mism.llvm).slice(0, 200))} ${mism.side}=${JSON.stringify(String(mism.other).slice(0, 200))}`;
    console.log(msg);
    problems.push(msg);
    row.mismatch = mism;
  }
  table.push(row);
}

console.log("");
others.forEach((s, k) => {
  if (ratios[k].length) console.log(`geomean ${s.tag}/llvm = ${geomean(ratios[k]).toFixed(3)} by median round ratio, ${geomean(bests[k]).toFixed(3)} by best round (${ratios[k].length} workloads with equal results; <1 means ${s.tag} is faster)`);
});
if (noted.size) {
  others.forEach((s, k) => {
    const keep = table.filter((r) => r.sides?.[s.tag] && !r.mismatch && !noted.has(r.name)).map((r) => r.sides[s.tag].ratio);
    if (keep.length) console.log(`geomean ${s.tag}/llvm = ${geomean(keep).toFixed(3)} over the ${keep.length} rows without a note`);
  });
  console.log("notes: a row marked JS spends that share of each call outside the module (glue, string copies), which no backend changes; an entry-cost share is time in the export wrappers themselves (measured on a no-op export, times the entries per call).");
}
console.log(problems.length ? `\n${problems.length} problem(s):\n${problems.map((p) => `  ${p}`).join("\n")}` : "results: every workload returned the same value on every side");
if (opt("json", null)) {
  const { writeFileSync } = await import("node:fs");
  writeFileSync(opt("json"), JSON.stringify({ sides: sides.map((s) => ({ tag: s.tag, dir: s.dir, bytes: s.bytes, info: s.info })), rounds: ROUNDS, table, geomean: others.map((s, k) => ({ tag: s.tag, geomean: ratios[k].length ? geomean(ratios[k]) : null, geomean_best: bests[k].length ? geomean(bests[k]) : null })) }, null, 1));
}
process.exit(problems.length ? 1 : 0);
