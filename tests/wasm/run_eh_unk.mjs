import { readFileSync } from "node:fs";
const m = new WebAssembly.Module(readFileSync(process.argv[2]));
const imports = WebAssembly.Module.imports(m);
if (imports.length) { console.log("FAIL unexpected imports:", imports.map(i=>i.module+"."+i.name).join(" ")); process.exit(1); }
const e = new WebAssembly.Instance(m, {}).exports;
const r = e.eh_test();
console.log(r === 15 ? "wasm unknown EH OK" : `FAIL eh_test bitmask = ${r} (want 15)`);
process.exit(r === 15 ? 0 : 1);
