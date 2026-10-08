import { WASI, File, Directory, OpenFile, PreopenDirectory, ConsoleStdout } from './node_modules/@bjorn3/browser_wasi_shim/dist/index.js';

const ready = (async () => {
  const t = performance.now();
  const [rustc, ld, manifest] = await Promise.all([
    WebAssembly.compileStreaming(fetch('rustc.wasm')),
    WebAssembly.compileStreaming(fetch('ld.wasm')),
    fetch('manifest.json').then(r => r.json()),
  ]);
  const root = new Map();
  await Promise.all(manifest.map(async p => {
    const data = await (await fetch('sysroot/' + p)).arrayBuffer();
    const parts = p.split('/');
    let dir = root;
    for (const d of parts.slice(0, -1)) {
      if (!dir.has(d)) dir.set(d, new Map());
      dir = dir.get(d);
    }
    dir.set(parts.at(-1), new File(data, { readonly: true }));
  }));
  const toDir = m => new Directory([...m].map(([k, v]) => [k, v instanceof Map ? toDir(v) : v]));
  postMessage({ log: `loaded rustc + sysroot in ${((performance.now() - t) / 1000).toFixed(1)}s` });
  return { rustc, ld, sysroot: toDir(root) };
})();

async function run(mod, args, work, sysroot) {
  const log = s => postMessage({ log: s });
  const fds = [
    new OpenFile(new File([])),
    ConsoleStdout.lineBuffered(log),
    ConsoleStdout.lineBuffered(log),
    new PreopenDirectory('/', new Map([['sysroot', sysroot], ['work', work]])),
  ];
  const wasi = new WASI(args, ['RUSTC_SYSROOT=/sysroot'], fds);
  const inst = await WebAssembly.instantiate(mod, { wasi_snapshot_preview1: wasi.wasiImport });
  return wasi.start(inst);
}

onmessage = async ({ data: { src, exports } }) => {
  try {
    const { rustc, ld, sysroot } = await ready;
    const work = new Directory([['main.rs', new File(new TextEncoder().encode(src))]]);
    let t = performance.now();
    let code = await run(rustc, ['rustc', '--sysroot', '/sysroot', '--target', 'wasm32-unknown-unknown',
      '--crate-type', 'staticlib', '--crate-name', 'main', '-Copt-level=1', '-Cpanic=abort',
      '-Zcodegen-backend=pliron', '/work/main.rs', '-o', '/work/libmain.a'], work, sysroot);
    postMessage({ log: `rustc exit ${code} in ${((performance.now() - t) / 1000).toFixed(2)}s` });
    if (code) return postMessage({ done: true });
    t = performance.now();
    code = await run(ld, ['pliron-wasm-ld', '--no-entry', ...exports.flatMap(e => ['--export', e]),
      '/work/libmain.a', '-o', '/work/main.wasm'], work, sysroot);
    postMessage({ log: `pliron-wasm-ld exit ${code} in ${((performance.now() - t) / 1000).toFixed(2)}s` });
    postMessage({ done: true, wasm: code ? null : work.contents.get('main.wasm').data });
  } catch (e) {
    postMessage({ log: String(e?.stack ?? e), done: true });
  }
};
