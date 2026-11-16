#!/usr/bin/env node
// peepdiff.mjs DIR [options] — translation validation by execution for the
// wasm byte peepholes (`wpeep`, src/wasm.rs).
//
// `PLIRON_WASM_MODDUMP=<dir>` makes the backend write every wasm object
// before and after the peepholes (`<name>.pre.wasm`, `<name>.post.wasm`).
// The two differ only in function bodies. This tool instantiates both in
// V8, calls every defined function of both with the same seeded
// arguments on the same seeded memory, and reports the functions that
// behave differently. No npm dependencies; needs a node whose V8 has
// multi-value, bulk memory and reference types (multi-memory too for
// `--coverage`).
//
//   node harness/wasm/peepdiff.mjs DIR
//        [--only SUBSTR]    modules whose file name contains SUBSTR
//        [--fn NAME|INDEX]  one function (substring of the name, or index)
//        [--trials N]       trials per function (default 16)
//        [--seed S]         PRNG seed (default 1)
//        [--every N]        every Nth function only (sampling; --phase K
//                           picks which residue)
//        [--json OUT]       machine-readable report
//        [--coverage]       also report which share of the code ran
//        [--mutate]         self-test: how many one-instruction mutants
//                           does a run with these options detect
//        [--strict]         also compare memory when both sides trap
//        [--trace]          with --fn: both traces of every trial (and,
//                           with --coverage, the places that never ran)
//        [--pages N]        linear memory pages (default 2)
//        [--fuel N]         loop iterations + calls per trial (default 8000)
//        [--calls N]        import calls per trial (default 64)
//        [--confirm N]      at most N extra module builds per module to
//                           isolate wrong bodies (default 64)
//        [-q]               totals and differences only
//
// Exit status: 0 no difference, 1 differences, 2 usage error or a module
// that could not be run.
//
// How a body is run
// -----------------
// An object is a complete module: memory, table and `__stack_pointer`
// are imports from `env`, external functions are `env.<sym>` imports,
// addresses are `GOT.mem.<sym>` / `GOT.func.<sym>` globals. Each side is
// edited only as far as needed to drive it, the same way on both sides,
// and only by appending (no index moves):
//   * the export section is replaced: every function (imports too) is
//     exported as `f<index>`, every global as `g<index>`;
//   * fuel: a mutable global, and "trap if it is 0, else decrement" at
//     every function entry and at the header of every loop some branch
//     targets, so no body can hang the run. A `loop` nothing branches to
//     is free (the peepholes remove those), so both sides burn the same
//     fuel on the same path and run out at the same place;
//   * `call_indirect (type t)` is redirected to a typed JS stub (through
//     an appended table) that gets the function pointer in a global.
//     When the pointer is the `GOT.func` slot of a function of this
//     module with that type the stub calls it; any other pointer behaves
//     like an unknown callee instead of trapping. Random memory holds no
//     vtables: without this almost every `dyn` call would end the trial;
//   * custom sections and a start section are dropped.
// Imports are generated:
//   * memory: `--pages` pages, laid out as
//       [0, 4096)            low guard (null and small integers land here)
//       [4096, SP0)          the stack the callee may allocate; nothing
//                            in memory or in the arguments points here
//       [SP0, SP0+1024)      the caller's frame
//       [SP0+1024, end)      data: GOT.mem symbols in the first half,
//                            the model allocator's arena in the second;
//   * `__stack_pointer` = SP0; `GOT.mem.*` distinct 16-aligned addresses;
//     `GOT.func.*` distinct table slots;
//   * every function import is a JS stub. `memcmp`/`bcmp`/`strlen`/
//     `memcpy`/`memmove`/`memset` and the Rust allocator entry points
//     are modelled; every other stub returns seeded values of its result
//     types and (on a seeded coin) writes seeded words at each
//     pointer-like argument, so "a call may write memory" is exercised.
// Per function and trial both instances get the same memory image (one
// of 8 seeded templates: zeros, pointer-heavy, small-integer-heavy,
// mixes; a third of the pointers in an image come from a pool of 24, so
// two loaded pointers are often equal), the same globals and the same
// arguments: i32 biased to in-range aligned addresses, small integers,
// 0, -1, and half of the time a pointer argument repeats an earlier one
// (the same object reached through two locals is where forwarding goes
// wrong); i64/f32/f64 from a mix with edge values. The trace is:
//   * every stub call: which import (or which indirect type and
//     pointer), the argument values, the stack pointer, and a CRC of all
//     memory the callee could read (the low guard and everything at or
//     above the stack pointer);
//   * how the call ended: result values, or the trap message;
//   * at a normal return, at fuel exhaustion and at the import-call
//     budget (points both sides reach in the same state): the stack
//     pointer, the other mutable globals and the CRC of the low guard
//     plus everything at or above the stack pointer (bytes below it are
//     dead).
// Internal calls go to the real callee of the same side, so a wrong
// callee makes its callers differ too. Every differing function is then
// re-run in isolation: a module with the pre bodies everywhere except
// that one function, against the all-pre module. A function that still
// differs there is wrong by itself (ISOLATED); one that does not is only
// a caller of a wrong one. When none of the functions such a caller can
// reach was caught by its own trials (a caller passes its callee real
// data, the callee's own trials pass it noise), the changed bodies in
// reach are bisected for the ones that make the caller's failing trial
// differ, and those are reported as ISOLATED too. Isolated functions
// are listed first, in full; callers get one line naming the callee.
//
// What counts as a difference
// ---------------------------
//   calls    the stub-call sequences differ (callee, arguments, visible
//            memory, stack pointer)
//   result   both return, with different values
//   trap     one traps and the other does not, or the trap messages differ
//   memory   both return (or both run out of fuel or import calls) with
//            different memory, stack pointer or globals
// A V8 stack overflow ends a trial as "inconclusive": frame sizes differ
// between the sides, so the depth at which V8 gives up does too, and
// only the common prefix of the two call sequences is compared.
//
// How much a clean run means
// --------------------------
// `--coverage` adds a probe at every function entry, block, loop, if and
// else header, after every `end` and `br_if` (a byte in an appended
// memory) and prints the share that ran: 40% of the probes on the wasm
// bench (std included) with the default 16 trials, 46% with 64, 27% on
// the dsa program. Small functions are covered well (three quarters of
// the probes of functions under 40 probes), big ones badly (a third
// over 200).
// `--mutate` ignores the post modules: for every function it builds one
// mutant of the pre body (one seeded site: an integer or float add/sub,
// and/or, eq/ne, lt/ge or shift swapped for its partner, or the offset
// of a load or store moved by 4), runs that body against the original
// with every other body unchanged, and prints the share of mutants
// detected and after how many trials (bench: 67% with 16 trials, 74%
// with 64; dsa: 62%). A wrong peephole in a body is found about as often
// as a wrong instruction in it.
//
// Blind spots
// -----------
//   * Paths the random inputs do not reach: more than half of the
//     blocks, see above. Most trials of big Rust functions end early in
//     a trap (a failed bounds check calls a panic stub that returns into
//     `unreachable`; a wild pointer leaves the memory). Read a clean run
//     as "no difference on the paths that ran".
//   * No real data. The `pliron.link` section (constant data, vtables)
//     is added after the peepholes and is not in the dump: `GOT.mem`
//     symbols point at seeded words, so lookup tables, strings and
//     vtables are random.
//   * Stubs are not the real callees: a `noreturn` import returns, an
//     import never calls back into the module and never unwinds, and an
//     indirect call through a pointer that is not a function of this
//     module is such a stub too.
//   * Memory when both sides trap is not compared unless `--strict`
//     (overwritten-store elimination may leave different bytes at a trap
//     between the two stores). Bytes below the stack pointer are never
//     compared.
//   * A difference that needs two pointers to be equal or to overlap
//     shows only in the trials where the generator made them so.
//   * Uninitialised memory is the same seeded bytes on both sides: it
//     makes no noise, and a read of it is compared as a defined value.
//   * NaN: values that cross the JS boundary (arguments, results, stub
//     arguments and results) are compared with NaN equal to NaN and are
//     only ever generated as the canonical NaN; payload bits are checked
//     only where they reach memory.
//   * Functions with a v128 parameter or result cannot be called from JS
//     and are skipped as entry points (they still run as callees); a
//     v128 import traps on both sides.
//   * `memory.grow` always fails (the memory is created at its maximum);
//     table writes by the code are not reset between trials.
//   * The stack is `SP0 - 4096` bytes: a body with a larger frame runs
//     into the guard and traps, on both sides.
//   * If a peephole ever changes how often a real loop header or a
//     function entry runs, the two sides run out of fuel at different
//     places and that trial reports a difference that is not one.
//   * Only the peepholes are checked: both dumps are downstream of
//     waffle, so a miscompile in waffle or the CLIF pipeline is in both.

import fs from 'node:fs';
import path from 'node:path';
import zlib from 'node:zlib';

// ---------------------------------------------------------------- options
const opt = {
  dir: null, only: null, fn: null, trials: 16, seed: 1, every: 1, phase: 0, json: null,
  pages: 2, fuel: 8000, calls: 64, confirm: 64, strict: false, trace: false, quiet: false,
  mutate: false, coverage: false,
};
{
  const a = process.argv.slice(2);
  const num = (s, flag) => {
    const n = Number(s);
    if (!Number.isFinite(n)) usage(`${flag} needs a number`);
    return n;
  };
  for (let i = 0; i < a.length; i++) {
    const k = a[i];
    const v = () => (i + 1 < a.length ? a[++i] : usage(`${k} needs a value`));
    if (k === '--only') opt.only = v();
    else if (k === '--fn') opt.fn = v();
    else if (k === '--trials') opt.trials = num(v(), k);
    else if (k === '--seed') opt.seed = num(v(), k) >>> 0;
    else if (k === '--every') opt.every = Math.max(1, num(v(), k));
    else if (k === '--phase') opt.phase = num(v(), k);
    else if (k === '--json') opt.json = v();
    else if (k === '--pages') opt.pages = Math.max(1, num(v(), k));
    else if (k === '--fuel') opt.fuel = num(v(), k);
    else if (k === '--calls') opt.calls = num(v(), k);
    else if (k === '--confirm') opt.confirm = num(v(), k);
    else if (k === '--strict') opt.strict = true;
    else if (k === '--mutate') opt.mutate = true;
    else if (k === '--coverage') opt.coverage = true;
    else if (k === '--trace') opt.trace = true;
    else if (k === '-q') opt.quiet = true;
    else if (k.startsWith('-')) usage(`unknown option ${k}`);
    else if (opt.dir === null) opt.dir = k;
    else usage(`unexpected argument ${k}`);
  }
  if (opt.dir === null) usage('no dump directory');
}
function usage(msg) {
  console.error(`peepdiff: ${msg}`);
  console.error('usage: peepdiff.mjs DIR [--only SUBSTR] [--fn NAME|INDEX] [--trials N] [--seed S]');
  console.error('         [--every N [--phase K]] [--json OUT] [--pages N] [--fuel N] [--calls N]');
  console.error('         [--confirm N] [--strict] [--coverage] [--mutate] [--trace] [-q]');
  process.exit(2);
}

// `zlib.crc32` is native (node 20.15 / 22.2 and later); older nodes get
// the table version, several times slower.
const crc32 = zlib.crc32 ?? (() => {
  const T = new Int32Array(256);
  for (let i = 0; i < 256; i++) {
    let c = i;
    for (let k = 0; k < 8; k++) c = c & 1 ? (c >>> 1) ^ 0xedb88320 : c >>> 1;
    T[i] = c;
  }
  return (u8, crc) => {
    let c = ~crc;
    for (let i = 0; i < u8.length; i++) c = T[(c ^ u8[i]) & 255] ^ (c >>> 8);
    return ~c >>> 0;
  };
})();

// ----------------------------------------------------------------- layout
const MEM = opt.pages * 65536;
const GUARD = 4096;
const SP0 = Math.max(GUARD + 4096, (MEM / 4) & ~15);
const CALLER = 1024;
const DATA_LO = SP0 + CALLER;
const PTR_HI = MEM - 256; // pointer-like values stay this far from the end
const MID = ((DATA_LO + PTR_HI) / 2) & ~15;
const NTEMPL = 8;

// ------------------------------------------------------------------- PRNG
function mix(a, b, c) {
  let h = (a ^ 0x9e3779b9) >>> 0;
  h = Math.imul(h ^ (b >>> 0), 0x85ebca6b) >>> 0;
  h = Math.imul(h ^ (h >>> 13) ^ (c >>> 0), 0xc2b2ae35) >>> 0;
  return (h ^ (h >>> 16)) >>> 0;
}
function mkRng(seed) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return (t ^ (t >>> 14)) >>> 0;
  };
}
function ptrLike(rng) {
  const span = (PTR_HI - SP0) >>> 4;
  let p = SP0 + ((rng() % span) << 4);
  const r = rng() & 7;
  if (r === 0) p += 4;
  else if (r === 1) p += 8;
  return p;
}
const F64_EDGE = [0, -0, 1, -1, 0.5, 2, 1e-300, 1e300, Infinity, -Infinity, NaN, 4294967296, -2147483648, 0.1, 255, 65535.5];
const I64_EDGE = [0n, 1n, -1n, 2n, 0x7fffffffn, 0x80000000n, 0xffffffffn, 0x100000000n,
  0x7fffffffffffffffn, -0x8000000000000000n, 10n, 255n, -2n];
// A value of wasm type `vt` (a type byte). `w` are the i32 class weights
// in percent: [pointer, small, zero, minus one, medium]; the rest is
// random bits.
function genVal(rng, vt, w) {
  switch (vt) {
    case 0x7f: {
      const r = rng() % 100;
      if (r < w[0]) return ptrLike(rng);
      if (r < w[0] + w[1]) return rng() % 9;
      if (r < w[0] + w[1] + w[2]) return 0;
      if (r < w[0] + w[1] + w[2] + w[3]) return -1;
      if (r < w[0] + w[1] + w[2] + w[3] + w[4]) return rng() % 4096;
      return rng() | 0;
    }
    case 0x7e: {
      const r = rng() % 100;
      if (r < 35) return I64_EDGE[rng() % I64_EDGE.length];
      if (r < 60) return BigInt(rng() % 17);
      if (r < 75) return BigInt(ptrLike(rng));
      return BigInt.asIntN(64, (BigInt(rng()) << 32n) | BigInt(rng()));
    }
    case 0x7d: {
      const r = rng() % 100;
      if (r < 40) return Math.fround(F64_EDGE[rng() % F64_EDGE.length]);
      if (r < 70) return (rng() % 33) - 16;
      return Math.fround(((rng() / 4294967296) - 0.5) * 2 ** ((rng() % 60) - 30));
    }
    case 0x7c: {
      const r = rng() % 100;
      if (r < 40) return F64_EDGE[rng() % F64_EDGE.length];
      if (r < 70) return (rng() % 33) - 16;
      return ((rng() / 4294967296) - 0.5) * 2 ** ((rng() % 120) - 60);
    }
    default:
      return 0;
  }
}
const W_ARG = [45, 22, 8, 5, 10];
const W_RET = [35, 25, 15, 5, 10];
// The arguments of one call. Half of the time a pointer argument repeats
// an earlier one (the same address, or 4 or 8 bytes on): code that
// reaches one object through two locals is where forwarding goes wrong.
function genArgs(rng, params) {
  const ptrs = [];
  return params.map((vt) => {
    if (vt === 0x7f && ptrs.length && rng() % 2 === 0) {
      const p = ptrs[rng() % ptrs.length];
      return p + [0, 0, 0, 0, 4, 8][rng() % 6];
    }
    const v = genVal(rng, vt, W_ARG);
    if (vt === 0x7f && v >= SP0 && v < PTR_HI) ptrs.push(v);
    return v;
  });
}

// The memory images. Template 0 is all zeros (empty slices, null
// pointers, discriminant 0), the others are words of seeded classes with
// seeded weights.
function mkTemplates(seed) {
  const out = [];
  for (let t = 0; t < NTEMPL; t++) {
    const u32 = new Uint32Array(MEM / 4);
    if (t > 0) {
      const rng = mkRng(mix(seed, 0x7e3a11, t));
      const wp = t === 1 ? 70 : 15 + (rng() % 50);
      const ws = t === 2 ? 60 : 8 + (rng() % 30);
      const wz = rng() % 15;
      const wb = 5 + (rng() % 15);
      // A third of the pointers come from a small pool, so that two
      // pointers loaded from memory are often the same address.
      const pool = Array.from({ length: 24 }, () => ptrLike(rng));
      for (let i = 0; i < u32.length; i++) {
        const r = rng() % 100;
        let v;
        if (r < wp) v = rng() % 3 === 0 ? pool[rng() % pool.length] : ptrLike(rng);
        else if (r < wp + ws) v = rng() % 9;
        else if (r < wp + ws + wz) v = 0;
        else if (r < wp + ws + wz + wb) {
          const x = rng();
          v = (x & 3) | (((x >> 2) & 3) << 8) | (((x >> 4) & 3) << 16) | (((x >> 6) & 3) << 24);
        } else if (r < 97) v = rng() % 4096;
        else v = rng();
        u32[i] = v;
      }
    }
    out.push(new Uint8Array(u32.buffer));
  }
  return out;
}

// ------------------------------------------------------------ wasm reader
function rdU(u8, c) {
  let r = 0, s = 0, b;
  do {
    b = u8[c.p++];
    if (s < 32) r |= (b & 0x7f) << s;
    s += 7;
  } while (b & 0x80);
  return r >>> 0;
}
function skipLeb(u8, c) {
  while (u8[c.p++] & 0x80);
}
function rdName(u8, c) {
  const n = rdU(u8, c);
  const s = Buffer.from(u8.buffer, u8.byteOffset + c.p, n).toString('utf8');
  c.p += n;
  return s;
}
function rdLimits(u8, c) {
  const fl = u8[c.p++];
  const min = rdU(u8, c);
  const max = fl & 1 ? rdU(u8, c) : undefined;
  return { min, max };
}
function leb(n) {
  const o = [];
  n >>>= 0;
  do {
    let b = n & 0x7f;
    n >>>= 7;
    if (n) b |= 0x80;
    o.push(b);
  } while (n);
  return o;
}
function section(id, payload) {
  return Buffer.concat([Buffer.from([id, ...leb(payload.length)]), payload]);
}

function parseModule(u8) {
  if (u8.length < 8 || u8[0] !== 0 || u8[1] !== 0x61 || u8[2] !== 0x73 || u8[3] !== 0x6d) {
    throw new Error('not a wasm module');
  }
  const m = {
    u8, sections: [], types: [], imports: [], nImpF: 0, nImpG: 0, nImpT: 0, nImpM: 0,
    funcType: [], nDefG: 0, defGlobals: [], nDefM: 0, nDefT: 0, exports: [], bodies: [], names: new Map(),
  };
  const c = { p: 8 };
  while (c.p < u8.length) {
    const id = u8[c.p++];
    const size = rdU(u8, c);
    const s = c.p, e = c.p + size;
    if (e > u8.length) throw new Error('truncated section');
    m.sections.push({ id, s, e });
    const q = { p: s };
    if (id === 1) {
      const n = rdU(u8, q);
      for (let i = 0; i < n; i++) {
        if (u8[q.p++] !== 0x60) throw new Error('type section: not a function type');
        const np = rdU(u8, q);
        const params = Array.from(u8.subarray(q.p, q.p + np));
        q.p += np;
        const nr = rdU(u8, q);
        const results = Array.from(u8.subarray(q.p, q.p + nr));
        q.p += nr;
        m.types.push({ params, results });
      }
    } else if (id === 2) {
      const n = rdU(u8, q);
      for (let i = 0; i < n; i++) {
        const mod = rdName(u8, q), name = rdName(u8, q), kind = u8[q.p++];
        const im = { mod, name, kind };
        if (kind === 0) {
          im.type = rdU(u8, q);
          im.index = m.nImpF++;
        } else if (kind === 1) {
          q.p++;
          im.limits = rdLimits(u8, q);
          im.index = m.nImpT++;
        } else if (kind === 2) {
          im.limits = rdLimits(u8, q);
          im.index = m.nImpM++;
        } else if (kind === 3) {
          im.vt = u8[q.p++];
          im.mut = u8[q.p++] === 1;
          im.index = m.nImpG++;
        } else throw new Error(`import kind ${kind}`);
        m.imports.push(im);
      }
    } else if (id === 3) {
      const n = rdU(u8, q);
      for (let i = 0; i < n; i++) m.funcType.push(rdU(u8, q));
    } else if (id === 4) m.nDefT = rdU(u8, q);
    else if (id === 5) m.nDefM = rdU(u8, q);
    else if (id === 6) {
      m.nDefG = rdU(u8, q);
      // (valtype, mut) of each; the init expression is skipped by
      // scanning to its `end` through the instruction decoder.
      for (let i = 0; i < m.nDefG; i++) {
        const vt = u8[q.p++], mut = u8[q.p++] === 1;
        m.defGlobals.push({ vt, mut });
        q.p = scanExpr(u8, q.p, e);
      }
    } else if (id === 7) {
      const n = rdU(u8, q);
      for (let i = 0; i < n; i++) {
        const name = rdName(u8, q), kind = u8[q.p++], idx = rdU(u8, q);
        m.exports.push({ name, kind, idx });
      }
    } else if (id === 10) {
      const n = rdU(u8, q);
      for (let i = 0; i < n; i++) {
        const sz = rdU(u8, q);
        m.bodies.push({ s: q.p, e: q.p + sz });
        q.p += sz;
      }
    } else if (id === 0) {
      const nm = rdName(u8, q);
      if (nm === 'name') {
        while (q.p < e) {
          const sid = u8[q.p++], ssz = rdU(u8, q), se = q.p + ssz;
          if (sid === 1) {
            const n = rdU(u8, q);
            for (let i = 0; i < n; i++) {
              const idx = rdU(u8, q);
              m.names.set(idx, rdName(u8, q));
            }
          }
          q.p = se;
        }
      }
    }
    c.p = e;
  }
  if (m.bodies.length !== m.funcType.length) throw new Error('function and code counts differ');
  for (const x of m.exports) if (x.kind === 0 && !m.names.has(x.idx)) m.names.set(x.idx, x.name);
  return m;
}

// One instruction's immediates. Returns the position after it.
// `on(op, pos, a, start)` sees calls (0x10, a = callee), indirect calls
// (0x11, a = type, start = where the instruction begins), block starts
// (0x02; 0x03 for a loop; pos = after the block type), block ends (0x0b)
// and every branch label (0x0c, a = depth).
function stepInstr(u8, p, on) {
  const c = { p };
  const op = u8[c.p++];
  const memarg = () => {
    const al = rdU(u8, c);
    if (al & 0x40) skipLeb(u8, c);
    skipLeb(u8, c);
  };
  if (op >= 0x45 && op <= 0xc4) return c.p;
  if (op >= 0x28 && op <= 0x3e) {
    memarg();
    return c.p;
  }
  switch (op) {
    case 0x00: case 0x01: case 0x05: case 0x0f: case 0x1a: case 0x1b:
    case 0xd1: case 0xd3: case 0xd4: case 0x0a: case 0x19:
      return c.p;
    case 0x0b: case 0x18:
      if (op === 0x18) skipLeb(u8, c);
      if (on) on(0x0b, c.p);
      return c.p;
    case 0x02: case 0x04: case 0x06:
      skipLeb(u8, c);
      if (on) on(0x02, c.p);
      return c.p;
    case 0x03:
      skipLeb(u8, c);
      if (on) on(0x03, c.p);
      return c.p;
    case 0x0c: case 0x0d: case 0xd5: case 0xd6: {
      const d = rdU(u8, c);
      if (on) on(0x0c, c.p, d);
      return c.p;
    }
    case 0x20: case 0x21: case 0x22: case 0x23: case 0x24:
    case 0x25: case 0x26: case 0x07: case 0x08: case 0x09: case 0x14: case 0x15:
    case 0xd2: case 0x3f: case 0x40: case 0xd0:
      skipLeb(u8, c);
      return c.p;
    case 0x0e: {
      const n = rdU(u8, c);
      for (let i = 0; i <= n; i++) {
        const d = rdU(u8, c);
        if (on) on(0x0c, c.p, d);
      }
      return c.p;
    }
    case 0x10: case 0x12: {
      const f = rdU(u8, c);
      if (on) on(0x10, c.p, f);
      return c.p;
    }
    case 0x11: case 0x13: {
      const t = rdU(u8, c);
      skipLeb(u8, c);
      if (on) on(0x11, c.p, t, p);
      return c.p;
    }
    case 0x1c: {
      const n = rdU(u8, c);
      c.p += n;
      return c.p;
    }
    case 0x1f: {
      skipLeb(u8, c);
      const n = rdU(u8, c);
      const ds = [];
      for (let i = 0; i < n; i++) {
        const k = u8[c.p++];
        if (k < 2) skipLeb(u8, c);
        ds.push(rdU(u8, c));
      }
      if (on) {
        for (const d of ds) on(0x0c, c.p, d);
        on(0x02, c.p);
      }
      return c.p;
    }
    case 0x41: case 0x42:
      skipLeb(u8, c);
      return c.p;
    case 0x43:
      return c.p + 4;
    case 0x44:
      return c.p + 8;
    case 0xfc: {
      const s = rdU(u8, c);
      if (s <= 7) return c.p;
      if (s === 8 || s === 10 || s === 12 || s === 14) {
        skipLeb(u8, c);
        skipLeb(u8, c);
        return c.p;
      }
      if (s === 9 || s === 11 || s === 13 || (s >= 15 && s <= 17)) {
        skipLeb(u8, c);
        return c.p;
      }
      throw new Error(`opcode 0xfc ${s}`);
    }
    case 0xfd: {
      const s = rdU(u8, c);
      if (s <= 11 || s === 92 || s === 93) memarg();
      else if (s === 12 || s === 13) c.p += 16;
      else if (s >= 21 && s <= 34) c.p += 1;
      else if (s >= 84 && s <= 91) {
        memarg();
        c.p += 1;
      }
      return c.p;
    }
    case 0xfe: {
      const s = rdU(u8, c);
      if (s === 3) c.p += 1;
      else memarg();
      return c.p;
    }
    default:
      throw new Error(`opcode 0x${op.toString(16)}`);
  }
}
// A constant expression: position after its `end`.
function scanExpr(u8, p, lim) {
  let depth = 0;
  while (p < lim) {
    const op = u8[p];
    p = stepInstr(u8, p, null);
    if (op === 0x02 || op === 0x03 || op === 0x04) depth++;
    else if (op === 0x0b) {
      if (depth === 0) return p;
      depth--;
    }
  }
  throw new Error('unterminated expression');
}
// A function body (locals + code): its direct callees, its indirect
// calls and where fuel is charged: the function entry and the header of
// every loop some branch targets. A `loop` nothing branches to runs once
// per entry like a block (the peepholes remove those), so it is free:
// both sides then burn the same fuel on the same path.
function scanBody(u8, s, e) {
  const c = { p: s };
  const nl = rdU(u8, c);
  for (let i = 0; i < nl; i++) {
    skipLeb(u8, c);
    c.p++;
  }
  const info = { calls: [], ind: [], loops: [c.p], probes: [c.p], probeBase: -1 };
  const stack = []; // open blocks: a loop's header position, or -1
  const hit = new Set();
  const on = (op, pos, a, start) => {
    if (op === 0x10) info.calls.push(a);
    else if (op === 0x11) info.ind.push({ s: start, e: pos, t: a });
    else if (op === 0x02) stack.push(-1);
    else if (op === 0x03) stack.push(pos);
    else if (op === 0x0b) stack.pop();
    else if (a < stack.length) {
      const h = stack[stack.length - 1 - a];
      if (h >= 0) hit.add(h);
    }
  };
  // Coverage probes (`--coverage`): the entry, after every block, loop,
  // if and else header, after every `end` but the last, after `br_if`.
  let p = c.p;
  while (p < e) {
    const op = u8[p];
    p = stepInstr(u8, p, on);
    if ((op >= 0x02 && op <= 0x05) || op === 0x0d || (op === 0x0b && p < e)) info.probes.push(p);
  }
  if (p !== e) throw new Error('body decode overran');
  for (const h of [...hit].sort((x, y) => x - y)) info.loops.push(h);
  return info;
}
function sleb(n) {
  const o = [];
  for (;;) {
    const b = n & 0x7f;
    n >>= 7;
    if ((n === 0 && !(b & 0x40)) || (n === -1 && b & 0x40)) {
      o.push(b);
      return o;
    }
    o.push(b | 0x80);
  }
}
// The body as a code-section entry (size prefix included) with the two
// edits that make it drivable:
//   * the fuel check at the entry and after every real loop header;
//   * `call_indirect (type t)` becomes "global.set $idx; i32.const t;
//     call_indirect (type t) (table $stubs)": the function pointer goes
//     to a global the stub reads, and the call lands on the typed JS
//     stub in slot t of an appended table, whatever the pointer was;
//   * with `--coverage`, "one byte of the coverage memory = 1" at every
//     probe position.
function instrument(u8, s, e, info, k) {
  const edits = info.loops.map((pos) => ({ at: pos, end: pos, ins: k.fuelSeq }));
  for (const x of info.ind) {
    edits.push({
      at: x.s, end: x.e,
      ins: Buffer.from([0x24, ...k.idxG, 0x41, ...sleb(x.t), u8[x.s], ...leb(x.t), ...k.stubTable]),
    });
  }
  if (k.cov && info.probeBase >= 0) {
    info.probes.forEach((pos, j) => {
      edits.push({ at: pos, end: pos, ins: Buffer.from([0x41, ...sleb(info.probeBase + j), 0x41, 1, 0x3a, 0x40, ...k.covMem, 0]) });
    });
  }
  edits.sort((a, b) => a.at - b.at || a.end - b.end);
  const parts = [];
  let last = s;
  for (const ed of edits) {
    parts.push(u8.subarray(last, ed.at), ed.ins);
    last = ed.end;
  }
  parts.push(u8.subarray(last, e));
  const body = Buffer.concat(parts);
  return Buffer.concat([Buffer.from(leb(body.length)), body]);
}

// One-instruction mutants for `--mutate`: opcode -> its partner.
const SWAP = new Map();
for (const [a, b] of [[0x6a, 0x6b], [0x71, 0x72], [0x46, 0x47], [0x48, 0x4e], [0x49, 0x4f], [0x74, 0x76],
  [0x7c, 0x7d], [0x83, 0x84], [0x51, 0x52], [0x53, 0x59], [0x54, 0x5a], [0x86, 0x88],
  [0xa0, 0xa1], [0x92, 0x93], [0x61, 0x62], [0x5b, 0x5c]]) {
  SWAP.set(a, b);
  SWAP.set(b, a);
}
// The body `[s, e)` with one seeded site changed, or null when it has
// no site. Same length, so loop positions keep their offsets.
function mutant(u8, s, e, rng) {
  const c = { p: s };
  const nl = rdU(u8, c);
  for (let i = 0; i < nl; i++) {
    skipLeb(u8, c);
    c.p++;
  }
  const sites = [];
  let p = c.p;
  while (p < e) {
    const op = u8[p];
    const q = stepInstr(u8, p, null);
    if (SWAP.has(op)) sites.push(p);
    // A load or store with a one-byte alignment and a one-byte offset
    // that stays one byte when moved by 4.
    else if (op >= 0x28 && op <= 0x3e && q === p + 3 && u8[p + 2] < 0x7c) sites.push(p);
    p = q;
  }
  if (sites.length === 0) return null;
  const at = sites[rng() % sites.length];
  const out = Buffer.from(u8.subarray(s, e));
  const op = u8[at];
  if (SWAP.has(op)) out[at - s] = SWAP.get(op);
  else out[at - s + 2] = u8[at + 2] + 4;
  return { body: out, at: at - s, op };
}

const RANK = { 1: 1, 2: 2, 3: 3, 4: 4, 5: 5, 13: 6, 6: 7, 7: 8, 8: 9, 9: 10, 12: 11, 10: 12, 11: 13 };

// Everything of the instantiable module except the code entries:
// `head` (bytes before the code section) and `tail` (after it).
// Appended, so that no existing index moves: two mutable i32 globals
// (the fuel, the last indirect-call target) and one table (the typed
// stubs indirect calls land on).
function prepare(m) {
  const u8 = m.u8;
  const nF = m.nImpF + m.funcType.length;
  const nG = m.nImpG + m.nDefG;
  const nT = m.nImpT + m.nDefT;
  const nM = m.nImpM + m.nDefM;
  const g = leb(nG);
  const k = {
    fuelG: nG, idxGlobal: nG + 1,
    fuelSeq: Buffer.from([0x23, ...g, 0x45, 0x04, 0x40, 0x00, 0x0b, 0x23, ...g, 0x41, 0x01, 0x6b, 0x24, ...g]),
    idxG: leb(nG + 1), stubTable: leb(nT), cov: opt.coverage, covMem: leb(nM),
  };
  const info = m.bodies.map((b) => scanBody(u8, b.s, b.e));
  let nProbes = 0;
  for (const inf of info) {
    inf.probeBase = nProbes;
    nProbes += inf.probes.length;
  }
  const ex = [];
  const exp = (name, kind, idx) => {
    const nb = Buffer.from(name);
    ex.push(Buffer.from(leb(nb.length)), nb, Buffer.from([kind, ...leb(idx)]));
  };
  let nEx = 0;
  for (let i = 0; i < nF; i++, nEx++) exp(`f${i}`, 0, i);
  for (let i = 0; i < nG + 2; i++, nEx++) exp(`g${i}`, 3, i);
  if (m.nDefM > 0) (exp('m0', 2, m.nImpM), nEx++);
  (exp('stubs', 1, nT), nEx++);
  if (k.cov) (exp('cov', 2, nM), nEx++);
  const exportSec = section(7, Buffer.concat([Buffer.from(leb(nEx)), ...ex]));
  // `add` after the entries of section `sec` (or as a new section).
  const extend = (id, sec, add, count) => {
    if (!sec) return section(id, Buffer.concat([Buffer.from(leb(count)), add]));
    const q = { p: sec.s };
    const n = rdU(u8, q);
    return section(id, Buffer.concat([Buffer.from(leb(n + count)), u8.subarray(q.p, sec.e), add]));
  };
  const globals = Buffer.from([0x7f, 0x01, 0x41, 0x00, 0x0b, 0x7f, 0x01, 0x41, 0x00, 0x0b]);
  const table = Buffer.from([0x70, 0x00, ...leb(Math.max(1, m.types.length))]);
  const head = [Buffer.from([0, 0x61, 0x73, 0x6d, 1, 0, 0, 0])], tail = [];
  const covMemory = Buffer.from([0x00, ...leb(Math.max(1, Math.ceil(nProbes / 65536)))]);
  let cur = head, tDone = false, mDone = !k.cov, gDone = false, eDone = false;
  for (const sec of m.sections) {
    if (sec.id === 0 || sec.id === 8) continue;
    const r = RANK[sec.id];
    if (r === undefined) throw new Error(`section id ${sec.id}`);
    if (!tDone && r > RANK[4]) (cur.push(extend(4, null, table, 1)), (tDone = true));
    if (!mDone && r > RANK[5]) (cur.push(extend(5, null, covMemory, 1)), (mDone = true));
    if (!gDone && r > RANK[6]) (cur.push(extend(6, null, globals, 2)), (gDone = true));
    if (!eDone && r > RANK[7]) (cur.push(exportSec), (eDone = true));
    if (sec.id === 4) (cur.push(extend(4, sec, table, 1)), (tDone = true));
    else if (sec.id === 5 && k.cov) (cur.push(extend(5, sec, covMemory, 1)), (mDone = true));
    else if (sec.id === 6) (cur.push(extend(6, sec, globals, 2)), (gDone = true));
    else if (sec.id === 7) (cur.push(exportSec), (eDone = true));
    else if (sec.id === 10) cur = tail;
    else cur.push(section(sec.id, u8.subarray(sec.s, sec.e)));
  }
  if (cur !== tail) throw new Error('no code section');
  const entries = m.bodies.map((b, i) => instrument(u8, b.s, b.e, info[i], k));
  return { head: Buffer.concat(head), tail: Buffer.concat(tail), entries, info, k, nProbes };
}
function assemble(prep, entries) {
  let n = 0;
  for (const e of entries) n += e.length;
  const cnt = Buffer.from(leb(entries.length));
  const hdr = Buffer.from([10, ...leb(n + cnt.length)]);
  return Buffer.concat([prep.head, hdr, cnt, ...entries, prep.tail]);
}

// A module of typed stubs for the stub table: the main module's type
// section, one imported and re-exported function per requested type (a
// JS function cannot go into a table, an exported import can).
function helperModule(m, typeIdxs) {
  const tsec = m.sections.find((s) => s.id === 1);
  const parts = [Buffer.from([0, 0x61, 0x73, 0x6d, 1, 0, 0, 0]), section(1, m.u8.subarray(tsec.s, tsec.e))];
  const im = [Buffer.from(leb(typeIdxs.length))], ex = [Buffer.from(leb(typeIdxs.length))];
  typeIdxs.forEach((t, i) => {
    const nb = Buffer.from(`t${t}`);
    im.push(Buffer.from([1, 0x73, ...leb(nb.length)]), nb, Buffer.from([0, ...leb(t)]));
    ex.push(Buffer.from(leb(nb.length)), nb, Buffer.from([0, ...leb(i)]));
  });
  parts.push(section(2, Buffer.concat(im)), section(7, Buffer.concat(ex)));
  return Buffer.concat(parts);
}

// ---------------------------------------------------------------- running
const BUDGET = new Error('peepdiff: import-call budget');
const MODEL_OOB = 'peepdiff: modelled import out of bounds';
// Endings at a point both sides reach in the same state: memory is
// compared there. A V8 stack overflow is not one (frame sizes differ).
const ALIGNED = new Set(['ret', 'fuel', 'budget']);

function vtName(vt) {
  return { 0x7f: 'i32', 0x7e: 'i64', 0x7d: 'f32', 0x7c: 'f64', 0x7b: 'v128' }[vt] ?? `0x${vt.toString(16)}`;
}

// One instantiated side. `ctx` is the mutable per-call state its stubs
// write to.
function instantiate(m, prep, bytes, gotFuncSlot, indTypes) {
  const ctx = {
    ev: [], calls: 0, rng: null, brk: MID, snapAt: -2, snap: null,
    memory: new WebAssembly.Memory({ initial: opt.pages, maximum: opt.pages }),
    u8: null, u32: null, sp: null, resets: [],
  };
  const importsObj = {};
  let memImported = false;
  let table = null;
  // `memory.grow` (even of 0 pages, even when it fails to add any)
  // detaches the buffer the views were made on.
  const views = () => {
    if (ctx.u8 === null || ctx.u8.length === 0) {
      ctx.u8 = new Uint8Array(ctx.memory.buffer, 0, MEM);
      ctx.u32 = new Uint32Array(ctx.memory.buffer, 0, MEM / 4);
    }
  };
  ctx.views = views;
  const crcLive = () => {
    views();
    const sp = ctx.sp ? ctx.sp.value >>> 0 : SP0;
    const lo = Math.min(Math.max(sp, GUARD), MEM);
    return crc32(ctx.u8.subarray(lo, MEM), crc32(ctx.u8.subarray(0, GUARD), 0));
  };
  ctx.crcLive = crcLive;
  const record = (id, args) => {
    if (++ctx.calls > opt.calls) throw BUDGET;
    const ev = ctx.ev;
    if (ctx.snapAt === ctx.calls - 1) ctx.snap = ctx.u8.slice();
    ev.push(id, ctx.sp ? ctx.sp.value : 0, crcLive(), args);
  };
  const inRange = (p, n) => {
    if (p + n > MEM) throw new WebAssembly.RuntimeError(MODEL_OOB);
  };
  const models = {
    memcmp: (a, b, n) => {
      (a >>>= 0), (b >>>= 0), (n >>>= 0);
      inRange(a, n), inRange(b, n);
      return Math.sign(Buffer.compare(ctx.u8.subarray(a, a + n), ctx.u8.subarray(b, b + n)));
    },
    strlen: (p) => {
      p >>>= 0;
      let q = p;
      while (q < MEM && ctx.u8[q]) q++;
      inRange(q, 1);
      return q - p;
    },
    memcpy: (d, s, n) => {
      (d >>>= 0), (s >>>= 0), (n >>>= 0);
      inRange(d, n), inRange(s, n);
      ctx.u8.copyWithin(d, s, s + n);
      return d;
    },
    memset: (d, c, n) => {
      (d >>>= 0), (n >>>= 0);
      inRange(d, n);
      ctx.u8.fill(c & 255, d, d + n);
      return d;
    },
    alloc: (size, align) => {
      (size >>>= 0), (align >>>= 0);
      const al = align > 0 && align <= 4096 && (align & (align - 1)) === 0 ? align : 16;
      const p = (ctx.brk + al - 1) & ~(al - 1);
      if (size > PTR_HI - p) return 0;
      ctx.brk = p + size;
      return p;
    },
  };
  models.bcmp = models.memcmp;
  models.memmove = models.memcpy;
  models.alloc_zeroed = (size, align) => {
    const p = models.alloc(size, align);
    if (p) ctx.u8.fill(0, p, p + (size >>> 0));
    return p;
  };
  models.dealloc = () => {};
  models.realloc = (ptr, old, align, nw) => {
    const p = models.alloc(nw, align);
    if (p) {
      const n = Math.min(old >>> 0, nw >>> 0);
      inRange(ptr >>> 0, n);
      ctx.u8.copyWithin(p, ptr >>> 0, (ptr >>> 0) + n);
    }
    return p;
  };
  const modelOf = (name, ty) => {
    const sig = ty.params.map(vtName).join(',') + '>' + ty.results.map(vtName).join(',');
    const iii = 'i32,i32,i32>i32';
    if ((name === 'memcmp' || name === 'bcmp' || name === 'memcpy' || name === 'memmove' || name === 'memset') && sig === iii) return models[name];
    if (name === 'strlen' && sig === 'i32>i32') return models.strlen;
    const r = /(?:^|[0-9_])__r(?:ust|dl|g)_(alloc|alloc_zeroed|dealloc|realloc)$/.exec(name);
    if (!r) return null;
    const want = { alloc: 'i32,i32>i32', alloc_zeroed: 'i32,i32>i32', dealloc: 'i32,i32,i32>', realloc: 'i32,i32,i32,i32>i32' }[r[1]];
    return sig === want ? models[r[1]] : null;
  };
  // A stub of type `ty`. `id` names it in the trace.
  const mkStub = (id, name, ty) => {
    const model = name === null ? null : modelOf(name, ty);
    if (model) {
      return (...args) => {
        record(id, args);
        return model(...args);
      };
    }
    const fx = effects(ty);
    return (...args) => {
      record(id, args);
      return fx(args);
    };
  };
  // What an unmodelled callee does: on a seeded coin, seeded words at
  // each pointer-like argument; seeded results.
  const effects = (ty) => {
    const res = ty.results, par = ty.params;
    return (args) => {
      const rng = ctx.rng;
      const sp = ctx.sp ? ctx.sp.value >>> 0 : SP0;
      for (let i = 0; i < par.length; i++) {
        if (par[i] !== 0x7f) continue;
        const p = args[i] >>> 0;
        if (p < sp || p < GUARD || p + 16 > MEM || (p & 3) !== 0 || (rng() & 1) === 0) continue;
        const n = 1 + (rng() & 3);
        for (let k = 0; k < n; k++) ctx.u32[(p >> 2) + k] = genVal(rng, 0x7f, W_RET);
      }
      if (res.length === 0) return undefined;
      if (res.length === 1) return genVal(rng, res[0], W_RET);
      return res.map((vt) => genVal(rng, vt, W_RET));
    };
  };
  // The stub every `call_indirect (type t)` lands on. The function
  // pointer is in the `$idx` global: when it is the slot of a function
  // of this module with that type, the call goes there; any other
  // pointer gets the effects of an unknown callee instead of a trap.
  const typeKey = (ty) => `${ty.params.join(',')}>${ty.results.join(',')}`;
  const mkIndirect = (t) => {
    const key = typeKey(m.types[t]), fx = effects(m.types[t]), id = -1 - t;
    return (...args) => {
      const idx = ctx.lastIdx.value;
      record(id, [idx, ...args]);
      const f = ctx.slotFn.get(idx);
      if (f !== undefined && f.key === key) return f.fn(...args);
      return fx(args);
    };
  };

  let gotMem = 0;
  const nGotMem = m.imports.filter((i) => i.kind === 3 && i.mod === 'GOT.mem').length;
  const span = MID - DATA_LO;
  const step = Math.max(16, Math.floor(span / Math.max(1, nGotMem)) & ~15);
  for (const im of m.imports) {
    const o = (importsObj[im.mod] ??= {});
    if (im.kind === 0) o[im.name] = mkStub(im.index, im.name, m.types[im.type]);
    else if (im.kind === 2) {
      o[im.name] = ctx.memory;
      memImported = true;
    } else if (im.kind === 1) {
      table = new WebAssembly.Table({ element: 'anyfunc', initial: Math.max(im.limits.min, gotFuncSlot.size + 1) });
      o[im.name] = table;
    } else {
      let init = 0;
      if (im.mod === 'GOT.mem') init = DATA_LO + (((gotMem++ * step) % span) & ~15);
      else if (im.mod === 'GOT.func') init = gotFuncSlot.get(im.name) ?? 0;
      else if (im.name === '__stack_pointer') init = SP0;
      const ty = vtName(im.vt);
      const v = ty === 'i64' ? BigInt(init) : init;
      const gl = new WebAssembly.Global({ value: ty, mutable: im.mut }, v);
      o[im.name] = gl;
      if (im.name === '__stack_pointer' && im.vt === 0x7f) ctx.sp = gl;
      if (im.mut) ctx.resets.push([gl, v]);
    }
  }
  const mod = new WebAssembly.Module(bytes);
  const inst = new WebAssembly.Instance(mod, importsObj);
  const ex = inst.exports;
  if (!memImported) {
    if (!ex.m0) throw new Error('module has no memory');
    ctx.memory = ex.m0;
    const have = ex.m0.buffer.byteLength / 65536;
    if (have < opt.pages) ex.m0.grow(opt.pages - have);
  }
  if (ctx.memory.buffer.byteLength < MEM) throw new Error('memory smaller than the layout');
  views();
  for (let i = 0; i < m.nDefG; i++) {
    if (m.defGlobals[i].mut) {
      const gl = ex[`g${m.nImpG + i}`];
      ctx.resets.push([gl, gl.value]);
    }
  }
  ctx.fuel = ex[`g${prep.k.fuelG}`];
  ctx.lastIdx = ex[`g${prep.k.idxGlobal}`];
  // Named table slots hold the function itself.
  ctx.slotFn = new Map();
  for (const [name, slot] of gotFuncSlot) {
    const fi = m.symFunc.get(name);
    if (fi === undefined) continue;
    const fn = ex[`f${fi}`];
    const ty = m.types[fi < m.nImpF ? m.impFuncType[fi] : m.funcType[fi - m.nImpF]];
    ctx.slotFn.set(slot, { fn, key: typeKey(ty) });
    if (table) table.set(slot, fn);
  }
  if (indTypes.length) {
    const imp = { s: {} };
    for (const t of indTypes) imp.s[`t${t}`] = mkIndirect(t);
    const h = new WebAssembly.Instance((m.helper ??= new WebAssembly.Module(helperModule(m, indTypes))), imp).exports;
    for (const t of indTypes) ex.stubs.set(t, h[`t${t}`]);
  }
  ctx.ex = ex;
  return ctx;
}

function trapKind(e, ctx) {
  if (e === BUDGET) return 'budget';
  if (e instanceof RangeError) return 'stack';
  const msg = e && e.message ? String(e.message) : String(e);
  if (e instanceof WebAssembly.RuntimeError) {
    if (msg === 'unreachable' && ctx.fuel.value === 0) return 'fuel';
    return msg;
  }
  return `${e?.constructor?.name ?? 'throw'}: ${msg}`;
}

// One call of function `fi` on one side. Returns the trace.
function runOne(ctx, templates, fi, t, args, snapAt) {
  ctx.views();
  ctx.u8.set(templates[t % NTEMPL]);
  for (const [gl, v] of ctx.resets) gl.value = v;
  ctx.fuel.value = opt.fuel;
  ctx.ev = [];
  ctx.calls = 0;
  ctx.brk = MID;
  ctx.rng = mkRng(mix(opt.seed ^ 0x51ed270b, fi, t));
  ctx.snapAt = snapAt ?? -2;
  ctx.snap = null;
  const tr = { ev: ctx.ev, end: 'ret', res: undefined, sp: 0, crc: 0, globals: null };
  try {
    tr.res = ctx.ex[`f${fi}`](...args);
  } catch (e) {
    tr.end = trapKind(e, ctx);
  }
  tr.sp = ctx.sp ? ctx.sp.value : 0;
  tr.crc = ctx.crcLive();
  tr.globals = ctx.resets.map(([gl]) => gl.value);
  if (snapAt === -1) ctx.snap = ctx.u8.slice();
  return tr;
}

function sameVal(a, b) {
  if (Array.isArray(a)) {
    if (!Array.isArray(b) || a.length !== b.length) return false;
    for (let i = 0; i < a.length; i++) if (!Object.is(a[i], b[i])) return false;
    return true;
  }
  return Object.is(a, b);
}
// null when the traces agree, else { kind, at } (`at`: the event index
// for 'calls').
function compare(a, b) {
  const ea = a.ev, eb = b.ev;
  const n = Math.min(ea.length, eb.length);
  for (let i = 0; i < n; i += 4) {
    if (ea[i] !== eb[i] || !sameVal(ea[i + 3], eb[i + 3])) return { kind: 'calls', at: i / 4, what: 'callee or arguments' };
    if (ea[i + 1] !== eb[i + 1]) return { kind: 'calls', at: i / 4, what: 'stack pointer' };
    if (ea[i + 2] !== eb[i + 2]) return { kind: 'calls', at: i / 4, what: 'memory visible to the callee' };
  }
  if (a.end === 'stack' || b.end === 'stack') return a.end === b.end && ea.length === eb.length ? null : { kind: 'inconclusive' };
  if (ea.length !== eb.length) return { kind: 'calls', at: n / 4, what: 'number of calls' };
  if (a.end !== b.end) return { kind: 'trap' };
  if (!ALIGNED.has(a.end) && !opt.strict) return null;
  if (!sameVal(a.res, b.res)) return { kind: 'result' };
  if (a.sp !== b.sp) return { kind: 'memory', what: 'stack pointer' };
  if (!sameVal(a.globals, b.globals)) return { kind: 'memory', what: 'globals' };
  if (a.crc !== b.crc) return { kind: 'memory', what: 'memory' };
  return null;
}

function fmtVal(v) {
  if (Array.isArray(v)) return `(${v.map(fmtVal).join(', ')})`;
  if (typeof v === 'bigint') return `${v}n`;
  if (v === undefined) return '()';
  if (Object.is(v, -0)) return '-0';
  if (typeof v === 'number' && Number.isInteger(v) && Math.abs(v) >= 4096 && Math.abs(v) < 2 ** 32) return `0x${(v >>> 0).toString(16)}`;
  return String(v);
}

// ------------------------------------------------------------- one module
function loadPair(dir, base) {
  const pre = parseModule(new Uint8Array(fs.readFileSync(path.join(dir, `${base}.pre.wasm`))));
  const post = parseModule(new Uint8Array(fs.readFileSync(path.join(dir, `${base}.post.wasm`))));
  const secEq = (id) => {
    const a = pre.sections.find((s) => s.id === id), b = post.sections.find((s) => s.id === id);
    if (!a || !b) return !a && !b;
    return Buffer.compare(pre.u8.subarray(a.s, a.e), post.u8.subarray(b.s, b.e)) === 0;
  };
  for (const id of [1, 2, 3, 4, 5, 6, 7, 9]) {
    if (!secEq(id)) throw new Error(`pre and post differ outside the code section (section ${id})`);
  }
  return { pre, post };
}

function importName(m, id) {
  if (id < 0) return `<indirect call, type ${-1 - id}; first value is the target>`;
  const im = m.imports.find((i) => i.kind === 0 && i.index === id);
  return im ? im.name : `import ${id}`;
}
function fmtEvent(m, ev, k) {
  if (k * 4 >= ev.length) return '(no such call)';
  const i = k * 4;
  return `${importName(m, ev[i])}${fmtVal(ev[i + 3])} sp=0x${(ev[i + 1] >>> 0).toString(16)} mem=${(ev[i + 2] >>> 0).toString(16).padStart(8, '0')}`;
}
function fmtEnd(tr) {
  return tr.end === 'ret' ? `returns ${fmtVal(tr.res)}` : `traps: ${tr.end}`;
}
function firstMemDiff(a, b, lo) {
  const n = Math.min(a.length, b.length);
  let i = 0;
  while (i < n && (a[i] === b[i] || (i >= GUARD && i < lo))) i++;
  if (i >= n) return null;
  const s = i & ~3;
  const hex = (u) => Buffer.from(u.subarray(s, s + 8)).toString('hex');
  return `first at 0x${i.toString(16)}: pre ${hex(a)} post ${hex(b)}`;
}

function runModule(dir, base, templates, report) {
  const t0 = Date.now();
  const { pre, post } = loadPair(dir, base);
  const m = pre;
  const nDef = m.funcType.length;
  if (nDef === 0) return null;
  const prepPre = prepare(pre), prepPost = prepare(post);
  // Symbols: exported names and function import names -> function index.
  m.symFunc = new Map();
  m.impFuncType = [];
  for (const im of m.imports) {
    if (im.kind !== 0) continue;
    m.symFunc.set(im.name, im.index);
    m.impFuncType[im.index] = im.type;
  }
  for (const x of m.exports) if (x.kind === 0) m.symFunc.set(x.name, x.idx);
  const gotFuncSlot = new Map();
  for (const im of m.imports) if (im.kind === 3 && im.mod === 'GOT.func') gotFuncSlot.set(im.name, gotFuncSlot.size + 1);
  const indCount = new Map();
  for (const pr of [prepPre, prepPost]) for (const inf of pr.info) for (const x of inf.ind) indCount.set(x.t, (indCount.get(x.t) ?? 0) + 1);
  const indTypes = [...indCount.keys()].sort((a, b) => a - b).filter((t) => {
    const ty = m.types[t];
    return ty && !ty.params.includes(0x7b) && !ty.results.includes(0x7b);
  });
  const A = instantiate(m, prepPre, assemble(prepPre, prepPre.entries), gotFuncSlot, indTypes);
  const B = instantiate(m, prepPost, assemble(prepPost, prepPost.entries), gotFuncSlot, indTypes);

  const name = (d) => m.names.get(m.nImpF + d) ?? `func[${m.nImpF + d}]`;
  let want = null;
  if (opt.fn !== null) {
    want = new Set();
    if (/^\d+$/.test(opt.fn)) want.add(Number(opt.fn) - m.nImpF);
    else for (let d = 0; d < nDef; d++) if (name(d).includes(opt.fn)) want.add(d);
  }
  const argsOf = (d, t) => {
    const rng = mkRng(mix(opt.seed, m.nImpF + d, t));
    return genArgs(rng, m.types[m.funcType[d]].params);
  };
  const st = {
    module: base, functions: nDef, tested: 0, changed: 0, skippedV128: 0, trials: 0, inconclusive: 0,
    returned: 0, ends: {}, differ: [], ms: 0,
  };
  const changedBody = pre.bodies.map((a, d) => {
    const b = post.bodies[d];
    return Buffer.compare(pre.u8.subarray(a.s, a.e), post.u8.subarray(b.s, b.e)) !== 0;
  });
  st.changed = changedBody.filter(Boolean).length;
  if (opt.mutate) {
    st.mutants = 0;
    st.killed = 0;
    st.killedAt = new Array(opt.trials).fill(0);
    st.survivors = [];
    for (let d = 0; d < nDef; d++) {
      if (want ? !want.has(d) : d % opt.every !== opt.phase % opt.every) continue;
      const ty = m.types[m.funcType[d]];
      if (ty.params.includes(0x7b) || ty.results.includes(0x7b)) continue;
      const b = pre.bodies[d];
      const mu = mutant(pre.u8, b.s, b.e, mkRng(mix(opt.seed, 0x6d757461, d)));
      if (!mu) continue;
      const fi = m.nImpF + d;
      const entries = prepPre.entries.slice();
      const inf = scanBody(mu.body, 0, mu.body.length);
      entries[d] = instrument(mu.body, 0, mu.body.length, inf, prepPre.k);
      const H = instantiate(m, prepPre, assemble(prepPre, entries), gotFuncSlot, indTypes);
      st.mutants++;
      st.tested++;
      let killed = -1;
      const ends = {};
      for (let t = 0; t < opt.trials && killed < 0; t++) {
        const ar = argsOf(d, t);
        const ta = runOne(A, templates, fi, t, ar);
        ends[ta.end] = (ends[ta.end] ?? 0) + 1;
        const x = compare(ta, runOne(H, templates, fi, t, ar));
        if (x && x.kind !== 'inconclusive') killed = t;
      }
      if (killed >= 0) (st.killed++, st.killedAt[killed]++);
      else st.survivors.push({ index: fi, name: name(d), at: mu.at, op: mu.op, ends });
    }
    st.ms = Date.now() - t0;
    report.push(st);
    return st;
  }
  const differing = new Map();
  for (let d = 0; d < nDef; d++) {
    if (want ? !want.has(d) : d % opt.every !== opt.phase % opt.every) continue;
    const ty = m.types[m.funcType[d]];
    if (ty.params.includes(0x7b) || ty.results.includes(0x7b)) {
      st.skippedV128++;
      continue;
    }
    const fi = m.nImpF + d;
    st.tested++;
    let returned = false, first = null, nDiff = 0;
    for (let t = 0; t < opt.trials; t++) {
      const args = argsOf(d, t);
      const ta = runOne(A, templates, fi, t, args);
      const tb = runOne(B, templates, fi, t, args);
      st.trials++;
      st.ends[ta.end] = (st.ends[ta.end] ?? 0) + 1;
      if (ta.end === 'ret') returned = true;
      const df = compare(ta, tb);
      if (opt.trace && want) {
        console.log(`  trial ${t} args ${fmtVal(args)}`);
        for (const [side, tr] of [['pre ', ta], ['post', tb]]) {
          console.log(`    ${side}: ${fmtEnd(tr)}; sp=0x${(tr.sp >>> 0).toString(16)} mem=${(tr.crc >>> 0).toString(16)}`);
          for (let k = 0; k * 4 < tr.ev.length; k++) console.log(`      call ${k}: ${fmtEvent(m, tr.ev, k)}`);
        }
      }
      if (!df) continue;
      if (df.kind === 'inconclusive') {
        st.inconclusive++;
        continue;
      }
      nDiff++;
      if (!first) first = { trial: t, df, args, ta, tb };
    }
    if (returned) st.returned++;
    if (first) differing.set(d, { first, nDiff });
  }

  // Describe the differing functions and find the wrong bodies.
  if (differing.size) {
    const D = new Set(differing.keys());
    const hybrid = (ds) => {
      const entries = prepPre.entries.slice();
      for (const d of ds) entries[d] = prepPost.entries[d];
      return instantiate(m, prepPre, assemble(prepPre, entries), gotFuncSlot, indTypes);
    };
    const differs = (H, d, t) => {
      const ar = argsOf(d, t);
      const x = compare(runOne(A, templates, m.nImpF + d, t, ar), runOne(H, templates, m.nImpF + d, t, ar));
      return x !== null && x.kind !== 'inconclusive';
    };
    // Defined functions `d` can reach: direct calls, and every function
    // whose address this module takes once an indirect call is in reach.
    const taken = [...gotFuncSlot.keys()].map((n) => m.symFunc.get(n)).filter((fi) => fi !== undefined && fi >= m.nImpF).map((fi) => fi - m.nImpF);
    const reach = (d0) => {
      const seen = new Set([d0]), work = [d0];
      let ind = false;
      while (work.length) {
        const inf = prepPre.info[work.pop()];
        const next = inf.calls.filter((f) => f >= m.nImpF).map((f) => f - m.nImpF);
        if (inf.ind.length && !ind) {
          ind = true;
          next.push(...taken);
        }
        for (const c of next) if (!seen.has(c)) (seen.add(c), work.push(c));
      }
      seen.delete(d0);
      return seen;
    };
    let budget = opt.confirm;
    const wrong = new Set(); // bodies wrong with every other body from pre
    for (const [d, { first, nDiff }] of differing) {
      const fi = m.nImpF + d;
      const { trial, df, args, ta, tb } = first;
      const lines = [];
      if (df.kind === 'calls') {
        lines.push(`import call ${df.at} differs (${df.what})`);
        lines.push(`pre : ${fmtEvent(m, ta.ev, df.at)}`);
        lines.push(`post: ${fmtEvent(m, tb.ev, df.at)}`);
        if (df.what.startsWith('memory')) {
          const ra = runOne(A, templates, fi, trial, args, df.at), sa = A.snap;
          runOne(B, templates, fi, trial, args, df.at);
          const sp = ra.ev[df.at * 4 + 1] >>> 0;
          if (sa && B.snap) lines.push(firstMemDiff(sa, B.snap, sp) ?? 'no differing byte found on replay');
        }
      } else if (df.kind === 'memory') {
        lines.push(`${df.what} differs after ${fmtEnd(ta)}`);
        if (df.what === 'stack pointer') lines.push(`pre sp=0x${(ta.sp >>> 0).toString(16)} post sp=0x${(tb.sp >>> 0).toString(16)}`);
        else if (df.what === 'globals') lines.push(`pre ${fmtVal(ta.globals)} post ${fmtVal(tb.globals)}`);
        else {
          runOne(A, templates, fi, trial, args, -1);
          const sa = A.snap;
          runOne(B, templates, fi, trial, args, -1);
          lines.push(firstMemDiff(sa, B.snap, Math.max(ta.sp >>> 0, GUARD)) ?? 'no differing byte found on replay');
        }
      } else {
        lines.push(`pre  ${fmtEnd(ta)}`);
        lines.push(`post ${fmtEnd(tb)}`);
      }
      // Isolation: this body from post, every other body from pre.
      let isolated = null;
      if (budget > 0) {
        budget--;
        const H = hybrid([d]);
        isolated = false;
        for (let t = 0; t < opt.trials && !isolated; t++) isolated = differs(H, d, t);
        if (isolated) wrong.add(d);
      }
      st.differ.push({
        index: fi, name: name(d), kind: df.kind, trial, seed: opt.seed, trialsDiffering: nDiff,
        args: fmtVal(args), detail: lines, isolated, via: null, wrongCallees: [],
      });
    }
    // A caller that differs only through its callees, none of which was
    // caught by its own trials: bisect the changed bodies it can reach
    // for the ones that make the caller's failing trial differ.
    for (const x of st.differ.slice()) {
      if (x.isolated !== false) continue;
      const d = x.index - m.nImpF;
      const r = reach(d);
      if (![...r].some((c) => wrong.has(c)) && budget > 0) {
        let S = [...r].filter((c) => changedBody[c]).sort((a, b) => a - b);
        const bad = (ds) => (budget-- > 0 ? differs(hybrid(ds), d, x.trial) : false);
        if (S.length && bad(S)) {
          while (S.length > 1) {
            const L = S.slice(0, S.length >> 1), R = S.slice(S.length >> 1);
            if (bad(L)) S = L;
            else if (bad(R)) S = R;
            else break; // needs bodies from both halves
          }
          for (const c of S) {
            wrong.add(c);
            if (st.differ.some((y) => y.index === m.nImpF + c)) continue;
            st.differ.push({
              index: m.nImpF + c, name: name(c), kind: 'callee', trial: x.trial, seed: opt.seed, trialsDiffering: 0,
              args: x.args, isolated: true, via: x.index, wrongCallees: [],
              detail: [`its own ${opt.trials} trials agree; with this body${S.length > 1 ? ` and ${S.length - 1} more` : ''} from post and every other from pre, ` +
                `caller func[${x.index}] differs (${x.kind}, trial ${x.trial})`],
            });
          }
        }
      }
      x.wrongCallees = [...r].filter((c) => wrong.has(c)).map((c) => m.nImpF + c);
    }
    for (const x of st.differ) if (x.kind === 'callee') x.isolated = true;
    const rank = (x) => (x.isolated ? 0 : x.isolated === null ? 1 : 2);
    st.differ.sort((x, y) => rank(x) - rank(y) || x.index - y.index);
  }
  if (opt.coverage) {
    st.coverage = {};
    for (const [side, ctx, prep, mod] of [['pre', A, prepPre, pre], ['post', B, prepPost, post]]) {
      const bits = new Uint8Array(ctx.ex.cov.buffer);
      let hit = 0, fns = 0;
      for (let d = 0; d < nDef; d++) {
        const inf = prep.info[d];
        let h = 0;
        const missed = [];
        inf.probes.forEach((pos, j) => {
          if (bits[inf.probeBase + j]) h++;
          else missed.push(pos - mod.bodies[d].s);
        });
        hit += h;
        if (h > 0) fns++;
        if (want && want.has(d)) (st.coverageDetail ??= []).push({ side, index: m.nImpF + d, probes: inf.probes.map((pos) => pos - mod.bodies[d].s), missed });
        if (opt.trace && want && want.has(d)) {
          console.log(`  coverage ${side} func[${m.nImpF + d}]: ${h}/${inf.probes.length} probes executed; body offsets not executed: ${missed.join(' ') || 'none'}`);
        }
      }
      st.coverage[side] = { hit, total: prep.nProbes, functionsEntered: fns };
    }
  }
  st.ms = Date.now() - t0;
  report.push(st);
  return st;
}

// ------------------------------------------------------------------- main
let bases;
try {
  bases = fs.readdirSync(opt.dir).filter((f) => f.endsWith('.pre.wasm')).map((f) => f.slice(0, -9)).sort();
} catch (e) {
  usage(`cannot read ${opt.dir}: ${e.message}`);
}
if (opt.only) bases = bases.filter((b) => b.includes(opt.only));
if (bases.length === 0) usage(`no *.pre.wasm in ${opt.dir}`);
const templates = mkTemplates(opt.seed);
const report = [];
const failed = [];
const T0 = Date.now();
for (const base of bases) {
  if (!fs.existsSync(path.join(opt.dir, `${base}.post.wasm`))) {
    failed.push({ module: base, error: 'no .post.wasm' });
    continue;
  }
  let st;
  try {
    st = runModule(opt.dir, base, templates, report);
  } catch (e) {
    failed.push({ module: base, error: String(e && e.message ? e.message : e) });
    console.log(`${base}: SKIPPED: ${failed.at(-1).error}`);
    continue;
  }
  if (!st) continue;
  if (opt.mutate) {
    if (!opt.quiet) console.log(`${base}: ${st.killed}/${st.mutants} mutants detected, ${st.ms} ms`);
    continue;
  }
  if (!opt.quiet || st.differ.length) {
    console.log(`${base}: ${st.tested}/${st.functions} functions (${st.changed} changed by wpeep), ${st.trials} trials, ` +
      `${st.returned} returned at least once, ` +
      (st.coverage ? `coverage ${pct(st.coverage.pre.hit, st.coverage.pre.total)}, ` : '') + `${st.differ.length} differ, ${st.ms} ms`);
  }
  for (const x of st.differ) {
    // `isolated`: true = wrong with every other body from pre, false =
    // differs only through its callees, null = not checked (--confirm).
    if (x.isolated === false) {
      if (opt.quiet) continue;
      console.log(`  DIFF [caller only] func[${x.index}] ${x.name}`);
      console.log(`    ${x.kind}, trial ${x.trial}; wrong callee: ${x.wrongCallees.map((i) => `func[${i}]`).join(' ') || 'not found'}`);
      continue;
    }
    console.log(`  DIFF [${x.isolated ? 'ISOLATED' : 'not isolated'}] func[${x.index}] ${x.name}`);
    if (x.kind === 'callee') {
      for (const l of x.detail) console.log(`    ${l}`);
      console.log(`    replay: node harness/wasm/peepdiff.mjs ${opt.dir} --only ${base} --fn ${x.via} --seed ${x.seed} --trials ${x.trial + 1} --trace`);
      continue;
    }
    console.log(`    ${x.kind}: trial ${x.trial} of seed ${x.seed} (${x.trialsDiffering} of ${opt.trials} trials differ), args ${x.args}`);
    for (const l of x.detail) console.log(`    ${l}`);
    console.log(`    replay: node harness/wasm/peepdiff.mjs ${opt.dir} --only ${base} --fn ${x.index} --seed ${x.seed} --trials ${x.trial + 1} --trace`);
  }
  const callers = st.differ.filter((x) => x.isolated === false);
  if (opt.quiet && callers.length) {
    const lost = callers.filter((x) => x.wrongCallees.length === 0);
    console.log(`  ${callers.length} more differ only through a callee` +
      (lost.length ? `; no wrong callee found for ${lost.map((x) => `func[${x.index}]`).join(' ')}` : ''));
  }
}
function pct(a, b) {
  return `${((a / Math.max(1, b)) * 100).toFixed(1)}%`;
}
const secs = ((Date.now() - T0) / 1000).toFixed(1);
// `process.exitCode`, not `process.exit()`: a piped stdout must drain.
if (opt.mutate) {
  let mutants = 0, killed = 0;
  const at = new Array(opt.trials).fill(0);
  for (const st of report) {
    mutants += st.mutants;
    killed += st.killed;
    st.killedAt.forEach((n, i) => (at[i] += n));
  }
  let acc = 0;
  const cum = at.map((n, i) => `${i + 1}:${pct((acc += n), mutants)}`).join(' ');
  console.log(`peepdiff --mutate: ${killed}/${mutants} one-instruction mutants detected (${pct(killed, mutants)}) ` +
    `in ${report.length} modules, ${opt.trials} trials, seed ${opt.seed}, ${secs} s`);
  console.log(`  detected after N trials: ${cum}`);
  if (opt.json) fs.writeFileSync(opt.json, JSON.stringify({ options: opt, mutants, killed, killedAt: at, failed, modules: report }, null, 1));
  process.exitCode = failed.length ? 2 : 0;
} else {
  const tot = {
    modules: report.length, functions: 0, tested: 0, changed: 0, trials: 0, returned: 0, inconclusive: 0,
    skippedV128: 0, differ: 0, isolated: 0, ends: {}, seconds: Number(secs),
  };
  for (const st of report) {
    for (const k of ['functions', 'tested', 'changed', 'trials', 'returned', 'inconclusive', 'skippedV128']) tot[k] += st[k];
    tot.differ += st.differ.length;
    tot.isolated += st.differ.filter((x) => x.isolated).length;
    for (const [k, v] of Object.entries(st.ends)) tot.ends[k] = (tot.ends[k] ?? 0) + v;
  }
  const ends = Object.entries(tot.ends).sort((a, b) => b[1] - a[1]).map(([k, v]) => `${k} ${v}`).join(', ');
  console.log(`peepdiff: ${tot.modules} modules, ${tot.tested}/${tot.functions} functions (${tot.changed} changed by wpeep), ` +
    `${tot.trials} trials (seed ${opt.seed}), ${secs} s`);
  console.log(`  reach: ${tot.returned} functions returned in at least one trial; pre-side endings: ${ends || 'none'}`);
  if (opt.coverage) {
    const c = (tot.coverage = { pre: { hit: 0, total: 0, functionsEntered: 0 }, post: { hit: 0, total: 0, functionsEntered: 0 } });
    for (const st of report) for (const side of ['pre', 'post']) for (const k of ['hit', 'total', 'functionsEntered']) c[side][k] += st.coverage[side][k];
    console.log(`  coverage: ${pct(c.pre.hit, c.pre.total)} of ${c.pre.total} pre probes executed (post ${pct(c.post.hit, c.post.total)} of ${c.post.total}); ` +
      `${c.pre.functionsEntered} functions entered`);
  }
  console.log(`  ${tot.inconclusive} inconclusive trials (V8 stack overflow), ` +
    `${tot.skippedV128} functions skipped (v128 signature), ${failed.length} modules skipped`);
  console.log(tot.differ === 0 ? '  no differences' : `  ${tot.differ} functions DIFFER (${tot.isolated} wrong in isolation)`);
  if (opt.json) {
    fs.writeFileSync(opt.json, JSON.stringify({ options: opt, layout: { MEM, GUARD, SP0, DATA_LO, MID }, totals: tot, failed, modules: report }, null, 1));
  }
  process.exitCode = tot.differ ? 1 : failed.length ? 2 : 0;
}
