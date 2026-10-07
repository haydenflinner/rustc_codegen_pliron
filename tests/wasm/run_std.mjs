import { readFileSync } from "node:fs";
const m = new WebAssembly.Module(readFileSync(process.argv[2]));
const imports = WebAssembly.Module.imports(m);
if (imports.length) { console.log("FAIL unexpected imports:", imports.map(i => i.module + "." + i.name).join(" ")); process.exit(1); }
const e = new WebAssembly.Instance(m, {}).exports;
let bad = 0;
for (const [n, f, w] of [
  ["words(100)", () => e.words(100), 6017],
  ["vec_sort(1000)", () => e.vec_sort(1000), 8388227136n],
  ["float_fmt", () => e.float_fmt(), 19],
  ["big_alloc(8)", () => e.big_alloc(8), 8 << 20],
]) {
  let r; try { r = f() } catch (x) { r = "trap: " + x.message }
  const ok = r === w; bad += !ok;
  console.log(ok ? "ok  " : "FAIL", n, "=", String(r), ok ? "" : `(want ${w})`);
}
console.log(bad ? `${bad} failed` : "wasm std OK");
process.exit(bad ? 1 : 0);
