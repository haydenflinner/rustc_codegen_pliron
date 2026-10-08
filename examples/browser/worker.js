import { WASI, File, Directory, OpenFile, PreopenDirectory, ConsoleStdout } from './node_modules/@bjorn3/browser_wasi_shim/dist/index.js';

const manifest = fetch('manifest.json', { cache: 'no-cache' }).then(r => r.json());

// gzip assets, kept in Cache Storage per build version so repeat visits skip the download.
async function fetchGz(name) {
  const { version } = await manifest;
  const url = `${name}?v=${version}`;
  const cache = await caches.open('rustc-wasm');
  let r = await cache.match(url);
  if (!r) {
    r = await fetch(url);
    if (!r.ok) throw new Error(`${name}: HTTP ${r.status}`);
    await cache.put(url, r.clone());
    for (const k of await cache.keys()) if (!k.url.endsWith(`?v=${version}`)) cache.delete(k);
  }
  return r.body.pipeThrough(new DecompressionStream('gzip'));
}

const wasm = async name =>
  WebAssembly.compileStreaming(new Response(await fetchGz(name), { headers: { 'content-type': 'application/wasm' } }));

const timed = async (what, p) => {
  const t = performance.now();
  const v = await p;
  postMessage({ log: `loaded ${what} in ${((performance.now() - t) / 1000).toFixed(1)}s` });
  return v;
};
const ready = timed('rustc + linker', Promise.all([wasm('rustc.wasm.gz'), wasm('ld.wasm.gz')]));

// Sysroot holds only the targets loaded so far; each target's libs arrive as one bundle.
const sysroot = new Directory([]);
const loaded = new Map();
function loadTarget(t) {
  if (!loaded.has(t)) loaded.set(t, timed(`${t} sysroot`, (async () => {
    const files = (await manifest).targets[t];
    const blob = new Uint8Array(await new Response(await fetchGz(`sysroot-${t}.bin.gz`)).arrayBuffer());
    for (const [p, off, len] of files) {
      const parts = p.split('/');
      let dir = sysroot;
      for (const d of parts.slice(0, -1)) {
        let next = dir.contents.get(d);
        if (!next) {
          next = new Directory([]);
          next.parent = dir;
          dir.contents.set(d, next);
        }
        dir = next;
      }
      dir.contents.set(parts.at(-1), new File(blob.subarray(off, off + len), { readonly: true }));
    }
  })()));
  return loaded.get(t);
}

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
    const [rustc, ld] = await ready;
    const std = /\bfn\s+main\s*\(/.test(src);
    await loadTarget(std ? 'wasm32-wasip1' : 'wasm32-unknown-unknown');
    if (std) return await runStd(rustc, src, sysroot);
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
