// node pgo_extract.mjs lib.wasm out.prof [exported_fn args...]
// Runs every exported function once (or the named fn), then walks the
// __pliron_prof_hdrs table in linear memory and writes a pliron .prof
// file — the wasm module carries no profiling runtime (docs/pgo.md).
import { readFileSync, writeFileSync } from "node:fs";

const [wasmPath, outPath, fnName, ...argStrs] = process.argv.slice(2);
const { instance } = await WebAssembly.instantiate(readFileSync(wasmPath), {});
const e = instance.exports;

// Exercise the module so counters move.
const calls = fnName ? [[fnName, argStrs.map(BigInt)]] : Object.keys(e)
    .filter((n) => typeof e[n] === "function" && !n.startsWith("__") && n !== "main")
    .map((n) => [n, []]);
for (const [n, args] of calls) {
    try { e[n](...args); } catch { /* traps are fine for profiling */ }
}

const tab = e.__pliron_prof_hdrs?.value;
if (tab === undefined) {
    console.error("no __pliron_prof_hdrs export — module not instrumented?");
    process.exit(1);
}
const mem = new DataView(e.memory.buffer);
const le = true;
const u32 = (a) => mem.getUint32(a, le);
const u64 = (a) => mem.getBigUint64(Number(a), le);

const nhdr = u32(tab);
const REC_LEN = 36; // {u64 sh, u64 fp, u64 ctr_base, u32 nb, u32 ne, u32 pad}
const recs = []; // {sh, fp, blocks:[], edges:[{p,s,count}]}
for (let i = 0; i < nhdr; i++) {
    const hdr = u32(tab + 4 + 4 * i);
    if (u64(hdr) !== 0x3130464f52504c50n) continue; // "PLPROF01" LE
    const ctrs = u64(hdr + 8);
    const nctrs = u64(hdr + 16);
    const recs_len = u64(hdr + 24);
    let p = hdr + 32;
    const end = p + Number(recs_len);
    while (p < end) {
        const sh = u64(p), fp = u64(p + 8), base = Number(u64(p + 16));
        const nb = u32(p + 24), ne = u32(p + 28);
        let k = p + REC_LEN;
        const blocks = [];
        for (let b = 0; b < nb; b++) blocks.push(u64(ctrs + 8n * BigInt(base + b)));
        const edges = [];
        for (let j = 0; j < ne; j++, k += 4) {
            const key = u32(k);
            edges.push({ p: key >>> 16, s: key & 0xffff,
                count: u64(ctrs + 8n * BigInt(base + nb + j)) });
        }
        recs.push({ sh, fp, blocks, edges });
        p = (k + 7) & ~7;
        void nctrs;
    }
}

// .prof: magic u64, nfuncs u64, per fn {sh, fp, nb u32, ne u32,
//         nb x u64 counts, ne x u64 edge counts, ne x u32 keys}
const bytes = [];
const w64 = (v) => { for (let i = 0; i < 8; i++) bytes.push(Number((v >> BigInt(8 * i)) & 0xffn)); };
const w32 = (v) => { for (let i = 0; i < 4; i++) bytes.push((v >>> (8 * i)) & 0xff); };
w64(0x3130464f52504c50n);
w64(BigInt(recs.length));
for (const r of recs) {
    w64(r.sh); w64(r.fp);
    w32(r.blocks.length); w32(r.edges.length);
    for (const c of r.blocks) w64(c);
    for (const ed of r.edges) w64(ed.count);
    for (const ed of r.edges) w32((ed.p << 16) | ed.s);
}
writeFileSync(outPath, Buffer.from(bytes));
console.log(`${outPath}: ${recs.length} functions`);
