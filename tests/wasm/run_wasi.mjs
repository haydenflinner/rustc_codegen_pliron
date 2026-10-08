import { WASI } from 'node:wasi';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'pliron-wasi-'));
const wasi = new WASI({ version: 'preview1', args: ['wasitest', 'a1'], env: { HELLO: 'world', WASI_DIR: '/work' }, preopens: { '/work': dir }, returnOnExit: true });
const m = await WebAssembly.compile(fs.readFileSync(process.argv[2]));
const extra = WebAssembly.Module.imports(m).filter(i => i.module !== 'wasi_snapshot_preview1');
if (extra.length) { console.error('unexpected imports', extra); process.exit(1); }
const code = wasi.start(await WebAssembly.instantiate(m, wasi.getImportObject()));
fs.rmSync(dir, { recursive: true, force: true });
process.exit(code);
