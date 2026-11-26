#!/usr/bin/env -S python3 -I
"""A `-Zcodegen-backend=` path whose NAME identifies the backend and its knobs.

  harness/backend_path.py [--dylib PATH] [--salt S] [--gc TARGET_DIR] [K=V ...]
                                             -> prints the path
  harness/backend_path.py --explain [...]    the key's inputs, nothing created
  harness/backend_path.py --list             entries, most recently used first
  harness/backend_path.py --prune [--keep N] [--min-age HOURS]
                                             (`--min-age 0`: whatever its age)

Why. Neither cargo's fingerprint nor sccache's key covers the *contents* of
the dylib behind `-Zcodegen-backend=PATH`, or any `PLIRON_*` variable: both
see the flag as a string. A build with a fixed path is therefore reused
("Fresh" / cache hit) after the backend was rebuilt or a knob changed. This
script turns both into the string those caches do look at:

  target/backends/<key16>/librustc_codegen_pliron.{dylib,so,dll}

`key16` is 16 hex digits of sha256 over
  - the dylib's sha256,
  - every code-affecting `PLIRON_*` variable, `NAME=VALUE`, sorted
    (all of them except `IGNORED` below),
  - the sha256 of the file a knob names, for knobs that read an input
    file (`FILE_KNOBS`, and any value spelled `@file`),
  - `--salt` (anything else the caller wants in the identity).
The entry is a private copy (a copy-on-write clone where the filesystem has
them, hard-linked between entries of one dylib), never the build output
itself: `cargo build` may rewrite `target/debug/lib...` while a long build
still loads the entry, and a path's content never changes once made. A
symlink would give cargo and sccache the same identity (both hash the
string) but not that property.

Contract for callers: the environment the helper sees must be the one the
rustc processes get. Pass per-build knobs as `K=V` arguments (or run the
helper under the same `env ...`), and do not export a `PLIRON_*` variable
between the helper and cargo.

What follows from the path being an argument (each line measured on the
pinned toolchain, docs/backend-path.md has the commands and numbers):
  - cargo reruns rustc for exactly the units compiled with that RUSTFLAGS
    when the backend or a knob changed, and for nothing when neither did:
    no target-dir wipe, no `cargo clean -p`.
  - sccache (`RUSTC_WRAPPER=sccache`) never returns an object made by
    another backend or under other knobs, and does return one when the
    same backend and knobs come back.
  - Side effect 1, disk: cargo puts RUSTFLAGS in a unit's file-name hash
    (`-C extra-filename`, the `build/<pkg>/<hash>/` directory), not in
    `-C metadata`. Mangled symbols stay the same, and every path leaves
    its own set of units next to the others in the target dir (going back
    to an earlier path is "Fresh"). `--gc TARGET_DIR` removes the units
    compiled with any other `-Zcodegen-backend=` than the path printed,
    so a target dir holds one backend's artifacts, as after a wipe.
  - Side effect 2, mixing: the flag's value is in rustc's crate hash
    (SVH). Within one cargo invocation every crate gets the same path, so
    they agree. By hand they do not: a crate compiled against `dep` under
    one path cannot be linked with `dep` compiled under another (E0460
    "found possibly newer version of crate `dep`"), although the symbol
    names match.
  - Diagnostic knobs (`IGNORED`) are not in the key, so setting one does
    not rerun rustc on a warm cache. Use `--salt`, a fresh target dir, or
    touch the sources.

Entries are pruned on every call: the `--keep` most recently used stay
(default 8, env `BACKEND_PATH_KEEP`), and so does anything used in the last
`--min-age` hours (default 2, env `BACKEND_PATH_MIN_AGE`): a running build
loads its entry for every crate, and one that outlives that should touch
the entry's directory now and then (the dsa repo's pliron-bench/build.sh does). An entry
costs the dylib's size once the build output it was cloned from is
replaced. `harness/disk.sh --sweep` prunes to two (plus the last 2 hours').

Importable: `backend_path(dylib=None, env=None, salt="")` returns the path.
"""
import glob, hashlib, json, os, shutil, subprocess, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
VERSION = "backend-path-v1"

# `PLIRON_*` variables left out of the key. Two kinds, both checked against
# their read sites; a variable that is not listed here is in the key.
IGNORED = frozenset({
    # Harness inputs the backend never reads. What they select is in the
    # key (the dylib's hash) or already a rustc argument (`-Clinker=`).
    "PLIRON_BACKEND", "PLIRON_WASM_LD", "PLIRON_PROFILE", "PLIRON_CRATES_NOCACHE",
    # Reporting only: these print or time, and gate no transform.
    "PLIRON_TRACE",             # tracing filter (src/lib.rs tracing_init)
    "PLIRON_TRACE_FILE",        # trace-event sink (pliron-cfg trace/mark)
    "PLIRON_STATS",             # pliron::stats target, egraph stats line
    "PLIRON_TIMING",            # pliron::timing target, Cranelift pass timer
    "PLIRON_VERSION",           # prints the source revision
    "PLIRON_MCPROF",            # memcache counters
    "PLIRON_CACHE_LOG",         # step-cache log
    "PLIRON_RA_EXPLAIN",        # regalloc2 decision log
    "PLIRON_RA2_EVICTDBG",      # regalloc2 eviction census log
    "PLIRON_OPT_BISECT_DEBUG",  # names bisect units (PLIRON_OPT_BISECT itself is in the key)
    "PLIRON_OPT_FNS_LOG",       # lists selected functions (PLIRON_OPT_FNS itself is in the key)
    "PLIRON_MEM_MARK",          # footprint marks
})
# Deliberately NOT ignored although they do not change code when they pass:
# checkers (`PLIRON_VERIFY`, `PLIRON_DIFFCHECK*`, `PLIRON_RA_CHECK`,
# `PLIRON_WASM_AUDIT`): a check that did not run because the build was
# "fresh" reads as a pass. Dumps and recorders (`PLIRON_CLIF`, `PLIRON_DUMP`,
# `PLIRON_CORPUS`, `PLIRON_REMARKS`, ...) too: some take other code paths
# (`PLIRON_DUMP` emits IR), and their output is what the caller wants.

# Knobs whose value is the path of a file the backend reads: its content is
# part of the key (a regenerated profile at the same path is a new build).
FILE_KNOBS = frozenset({"PLIRON_PGO_USE", "PLIRON_FIELD_ORDER"})


def lib_name():
    if sys.platform == "darwin":
        return "librustc_codegen_pliron.dylib"
    if os.name == "nt" or sys.platform in ("cygwin", "msys"):
        return "rustc_codegen_pliron.dll"
    return "librustc_codegen_pliron.so"


def default_dylib(env=None, root=ROOT):
    env = os.environ if env is None else env
    return os.path.join(root, "target", env.get("PLIRON_PROFILE") or "debug", lib_name())


def store_dir(root=ROOT):
    return os.environ.get("BACKEND_PATH_DIR") or os.path.join(root, "target", "backends")


def file_sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def code_env(env=None):
    """The sorted `(name, value)` pairs of `env` that are in the key."""
    env = os.environ if env is None else env
    return sorted((k, v) for k, v in env.items() if k.startswith("PLIRON_") and k not in IGNORED)


def key_material(dylib, env=None, salt=""):
    """(text hashed into the key, sha256 of the dylib)."""
    dsha = file_sha256(dylib)
    lines = [VERSION, f"dylib={dsha}"]
    for k, v in code_env(env):
        lines.append(f"env:{k}={v}")
        f = v[1:] if v.startswith("@") else v if k in FILE_KNOBS else None
        if f and os.path.isfile(f):
            lines.append(f"file:{k}={file_sha256(f)}")
    lines.append(f"salt={salt}")
    return "\n".join(lines) + "\n", dsha


def _clone(src, dst):
    """Copy `src` to `dst` without sharing the inode; copy-on-write if the
    filesystem can (APFS `cp -c`, `cp --reflink=auto` on Linux)."""
    cmd = {"darwin": ["cp", "-c"], "linux": ["cp", "--reflink=auto"]}.get(sys.platform)
    if cmd:
        try:
            subprocess.run(cmd + [src, dst], check=True, stderr=subprocess.DEVNULL)
            return
        except (OSError, subprocess.CalledProcessError):
            pass
    shutil.copyfile(src, dst)
    shutil.copymode(src, dst)


def _entries(store):
    """[(last use, entry dir)] newest first."""
    out = []
    try:
        for e in os.scandir(store):
            if e.is_dir(follow_symlinks=False) and not e.name.startswith("tmp."):
                out.append((e.stat().st_mtime, e.path))
    except FileNotFoundError:
        pass
    return sorted(out, reverse=True)


def _dylib_sha_of(entry):
    try:
        with open(os.path.join(entry, "KEY")) as f:
            for line in f:
                if line.startswith("dylib="):
                    return line.strip()[6:]
    except OSError:
        pass
    return None


def prune(keep=None, min_age_h=None, store=None, protect=()):
    """Drop entries beyond the `keep` most recently used that have been idle
    for `min_age_h` hours. Returns the removed paths."""
    store = store or store_dir()
    keep = int(os.environ.get("BACKEND_PATH_KEEP", 8)) if keep is None else keep
    min_age_h = float(os.environ.get("BACKEND_PATH_MIN_AGE", 2)) if min_age_h is None else min_age_h
    now, gone = time.time(), []
    for i, (mt, path) in enumerate(_entries(store)):
        if i >= keep and now - mt >= min_age_h * 3600 and path not in protect:
            shutil.rmtree(path, ignore_errors=True)
            gone.append(path)
    try:  # creations that died half way
        for e in os.scandir(store):
            if e.name.startswith("tmp.") and now - e.stat().st_mtime > 3600:
                shutil.rmtree(e.path, ignore_errors=True)
    except OSError:
        pass
    return gone


def gc(target_dir, keep):
    """Remove from a cargo target dir every build unit whose fingerprint
    names a `-Zcodegen-backend=` other than `keep` (new build-dir layout:
    `<profile>/build/<pkg>/<hash>/`, which holds the unit's fingerprint and
    its outputs; units without the flag, e.g. host proc macros under
    `--target`, are left alone). Returns (units removed, bytes freed)."""
    n = freed = 0
    seen = set()
    for pat in ("*/build/*/*/fingerprint/*.json", "*/*/build/*/*/fingerprint/*.json"):
        for j in glob.glob(os.path.join(glob.escape(target_dir), pat)):
            unit = os.path.dirname(os.path.dirname(j))
            if unit in seen:
                continue
            try:
                with open(j) as f:
                    flags = json.load(f).get("rustflags") or []
            except (OSError, ValueError):
                continue
            be = [f.split("=", 1)[1] for f in flags
                  if isinstance(f, str) and f.startswith(("-Zcodegen-backend=", "codegen-backend="))]
            if not be or keep in be:
                continue
            seen.add(unit)
            for d, _, files in os.walk(unit):
                for x in files:
                    try:
                        freed += os.lstat(os.path.join(d, x)).st_size
                    except OSError:
                        pass
            shutil.rmtree(unit, ignore_errors=True)
            n += 1
    return n, freed


def backend_path(dylib=None, env=None, salt="", keep=None, min_age_h=None, store=None):
    """The keyed path for `dylib` under `env` (default: this process's),
    created if missing. Raises FileNotFoundError when there is no dylib."""
    env = os.environ if env is None else env
    dylib = os.path.abspath(dylib or default_dylib(env))
    store = store or store_dir()
    text, dsha = key_material(dylib, env, salt)
    key = hashlib.sha256(text.encode()).hexdigest()[:16]
    entry = os.path.join(store, key)
    dst = os.path.join(entry, os.path.basename(dylib))

    def valid():
        # An entry is trusted by its KEY file and size: nothing writes to
        # one after the rename below publishes it.
        try:
            with open(os.path.join(entry, "KEY")) as f:
                return f.read() == text and os.path.getsize(dst) == os.path.getsize(dylib)
        except OSError:
            return False

    if not valid():
        os.makedirs(store, exist_ok=True)
        tmp = os.path.join(store, f"tmp.{key}.{os.getpid()}")
        shutil.rmtree(tmp, ignore_errors=True)
        os.makedirs(tmp)
        tdst = os.path.join(tmp, os.path.basename(dylib))
        # Same dylib under other knobs: share its bytes with a hard link
        # (entries are immutable, the build output is not).
        for _, other in _entries(store):
            if _dylib_sha_of(other) == dsha:
                try:
                    os.link(os.path.join(other, os.path.basename(dylib)), tdst)
                    break
                except OSError:
                    pass
        if not os.path.exists(tdst):
            _clone(dylib, tdst)
            if file_sha256(tdst) != dsha:  # rebuilt under us
                shutil.rmtree(tmp, ignore_errors=True)
                raise RuntimeError(f"{dylib} changed while it was being copied; rerun")
        with open(os.path.join(tmp, "KEY"), "w") as f:
            f.write(text)
        with open(os.path.join(tmp, "SOURCE"), "w") as f:
            f.write(dylib + "\n")
        if valid():  # a concurrent call published the same key first
            shutil.rmtree(tmp, ignore_errors=True)
        else:
            shutil.rmtree(entry, ignore_errors=True)  # damaged, if there at all
            try:
                os.rename(tmp, entry)
            except OSError:
                shutil.rmtree(tmp, ignore_errors=True)
                if not valid():
                    raise
    try:
        os.utime(entry)  # last use, for the pruning order
    except OSError:
        pass
    prune(keep, min_age_h, store, protect=(entry,))
    # RUSTFLAGS wants a native, forward-slash spelling on Windows (cargo
    # mangles POSIX-spelled paths in flag values).
    if sys.platform in ("msys", "cygwin"):
        try:
            return subprocess.run(["cygpath", "-m", dst], check=True, capture_output=True, text=True).stdout.strip()
        except (OSError, subprocess.CalledProcessError):
            pass
    return dst.replace("\\", "/") if os.name == "nt" else dst


def main(argv):
    dylib = keep = min_age = None
    salt, mode, env, gc_dirs = "", "path", dict(os.environ), []
    it = iter(argv)
    for a in it:
        if a == "--dylib":
            dylib = next(it)
        elif a == "--salt":
            salt = next(it)
        elif a == "--gc":
            gc_dirs.append(next(it))
        elif a == "--keep":
            keep = int(next(it))
        elif a == "--min-age":
            min_age = float(next(it))
        elif a in ("--explain", "--list", "--prune"):
            mode = a[2:]
        elif a in ("-h", "--help"):
            print(__doc__)
            return 0
        elif "=" in a and not a.startswith("-"):
            k, v = a.split("=", 1)
            env[k] = v
        else:
            sys.exit(f"backend_path.py: unknown argument `{a}` (see --help)")
    if mode == "list":
        for mt, path in _entries(store_dir()):
            age = (time.time() - mt) / 3600
            knobs = []
            try:
                with open(os.path.join(path, "KEY")) as f:
                    knobs = [l.strip()[4:] for l in f if l.startswith("env:")]
            except OSError:
                pass
            print(f"{os.path.basename(path)}  used {age:6.1f} h ago  dylib {(_dylib_sha_of(path) or '?')[:12]}  {' '.join(knobs) or '<no knobs>'}")
        return 0
    if mode == "prune":
        for p in prune(keep, min_age):
            print(f"removed {p}")
        return 0
    dylib = os.path.abspath(dylib or default_dylib(env))
    if not os.path.isfile(dylib):
        sys.exit(f"backend_path.py: no backend dylib at {dylib} (cargo build, or --dylib PATH)")
    if mode == "explain":
        text, _ = key_material(dylib, env, salt)
        sys.stdout.write(f"# {dylib}\n{text}key={hashlib.sha256(text.encode()).hexdigest()[:16]}\n")
        ign = sorted(k for k in env if k.startswith("PLIRON_") and k in IGNORED)
        if ign:
            print("ignored: " + " ".join(ign))
        return 0
    path = backend_path(dylib, env, salt, keep, min_age)
    for d in gc_dirs:
        n, freed = gc(d, path)
        if n:
            print(f"backend_path.py: {d}: removed {n} units of other backends ({freed / 1e6:.0f} MB)", file=sys.stderr)
    print(path)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
