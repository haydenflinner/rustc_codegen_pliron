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

async function run(mod, args, work, sysroot, out = s => postMessage({ log: s })) {
  const fds = [
    new OpenFile(new File([])),
    ConsoleStdout.lineBuffered(out),
    ConsoleStdout.lineBuffered(out),
    new PreopenDirectory('/', new Map([['sysroot', sysroot], ['work', work]])),
  ];
  const wasi = new WASI(args, ['RUSTC_SYSROOT=/sysroot'], fds);
  const inst = await WebAssembly.instantiate(mod, { wasi_snapshot_preview1: wasi.wasiImport });
  return wasi.start(inst);
}

const secs = t => ((performance.now() - t) / 1000).toFixed(2);

// std program: rustc links it in-process (pliron-wasm-ld is linked into rustc.wasm), then it runs on WASI here.
async function runStd(rustc, src, sysroot) {
  const work = new Directory([['main.rs', new File(new TextEncoder().encode(src))]]);
  let t = performance.now();
  let code = await run(rustc, ['rustc', '--sysroot', '/sysroot', '--target', 'wasm32-wasip1',
    '--crate-type', 'bin', '--crate-name', 'main', '-Copt-level=1', '-Cpanic=abort',
    '-Zcodegen-backend=pliron', '-Clinker=pliron-wasm-ld', '-Clink-self-contained=no',
    '-Lnative=/sysroot/wasi-libc', '/work/main.rs', '-o', '/work/main.wasm'], work, sysroot);
  postMessage({ log: `rustc (compile + link) exit ${code} in ${secs(t)}s` });
  if (code) return postMessage({ done: true });
  const prog = await WebAssembly.compile(work.contents.get('main.wasm').data);
  t = performance.now();
  code = await run(prog, ['main'], new Directory([]), new Directory([]), s => postMessage({ out: s }));
  postMessage({ log: `main.wasm exit ${code} in ${secs(t)}s`, done: true, ran: true });
}

onmessage = async ({ data: { src, exports } }) => {
  try {
    const { rustc, ld, sysroot } = await ready;
    if (/\bfn\s+main\s*\(/.test(src)) return await runStd(rustc, src, sysroot);
    const work = new Directory([['main.rs', new File(new TextEncoder().encode(src))]]);
    let t = performance.now();
    let code = await run(rustc, ['rustc', '--sysroot', '/sysroot', '--target', 'wasm32-unknown-unknown',
      '--crate-type', 'staticlib', '--crate-name', 'main', '-Copt-level=1', '-Cpanic=abort',
      '-Zcodegen-backend=pliron', '/work/main.rs', '-o', '/work/libmain.a'], work, sysroot);
    postMessage({ log: `rustc exit ${code} in ${secs(t)}s` });
    if (code) return postMessage({ done: true });
    t = performance.now();
    code = await run(ld, ['pliron-wasm-ld', '--no-entry', ...exports.flatMap(e => ['--export', e]),
      '/work/libmain.a', '-o', '/work/main.wasm'], work, sysroot);
    postMessage({ log: `pliron-wasm-ld exit ${code} in ${secs(t)}s` });
    postMessage({ done: true, wasm: code ? null : work.contents.get('main.wasm').data });
  } catch (e) {
    postMessage({ log: String(e?.stack ?? e), done: true });
  }
};
