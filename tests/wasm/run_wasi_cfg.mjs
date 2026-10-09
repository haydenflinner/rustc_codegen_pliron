// node run_wasi_cfg.mjs prog.wasm — run a wasm32-wasip1 program configured by
// the WASI_RUN_CFG env var (JSON: {args, env, preopens}). Exit code propagates;
// a wasm trap surfaces as an uncaught RuntimeError (nonzero exit).
import { WASI } from "node:wasi";
import fs from "node:fs";
const cfg = JSON.parse(process.env.WASI_RUN_CFG || "{}");
const wasi = new WASI({
  version: "preview1",
  args: cfg.args ?? ["prog"],
  env: cfg.env ?? {},
  preopens: cfg.preopens ?? {},
  returnOnExit: true,
});
const m = await WebAssembly.compile(fs.readFileSync(process.argv[2]));
const code = wasi.start(await WebAssembly.instantiate(m, wasi.getImportObject()));
process.exit(code);
