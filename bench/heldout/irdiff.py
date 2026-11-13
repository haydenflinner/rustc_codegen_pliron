#!/usr/bin/env -S python3 -I
"""Exact instruction counts, ours against LLVM's, per function and per source line.

  bench/heldout/irdiff.py PROG [--env K=V ...] [--top N] [--fn SUBSTR | --line FILE:LINE] [--skip-build]

Sampling cannot say where we execute more instructions than LLVM: the
whole-process count is exact (`ab.py`) but the breakdown is samples,
which skid onto the instruction after a slow load or a branch target
and cannot resolve 1%. This counts. Both compilers build the program
for `aarch64-unknown-linux-musl` (static; linked with `wild` or
`rust-lld`, line tables on), and each binary runs once under
callgrind in a Linux container (docker; the image is built on first
use). callgrind executes every instruction and charges it to its
function and, through the line table, to its source line.

Output: the programs' totals, then

  * functions by self-instruction difference. Inlining differs between
    the compilers, so a function one side inlined shows as all-or-
    nothing: read this table for *where the instructions are*;
  * source lines by difference. A line's instructions are charged to it
    wherever they were inlined, so this table is independent of the
    inlining decisions: read it for *what code we generate worse*.

Same target for both sides, so the comparison is fair, but it is Linux
code: register `x18` is free there and the ABI details differ slightly
from macOS. Costs are instructions, not time.
"""
import argparse, collections, os, re, shutil, subprocess, sys, importlib.util

HERE = os.path.dirname(os.path.abspath(__file__))
TARGET = "aarch64-unknown-linux-musl"
IMAGE = "pliron-callgrind"


def load(name):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, name + ".py"))
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


run = load("run")


def linker_flags():
    wild = shutil.which("wild")
    if wild:
        return f"-Clinker={wild} -Clinker-flavor=ld.lld"
    return "-Clinker=rust-lld -Clinker-flavor=ld.lld"


def build(prog, binary, side, backend, env_extra, skip):
    tdir = os.path.join(run.OUT, f"ir-{side}", prog)
    exe = os.path.join(tdir, TARGET, "release", binary)
    if skip and os.path.exists(exe):
        return exe
    if side != "llvm":
        shutil.rmtree(tdir, ignore_errors=True)
    # Line tables through the profile, not RUSTFLAGS: a release profile
    # without `debug` makes cargo strip debug info at link.
    env = dict(os.environ, RUSTUP_TOOLCHAIN=run.TOOLCHAIN,
               CARGO_PROFILE_RELEASE_DEBUG="line-tables-only", CARGO_PROFILE_RELEASE_STRIP="none")
    flags = linker_flags()
    if side != "llvm":
        flags = f"-Zcodegen-backend={backend} {flags}"
        env.update(env_extra)
    env["RUSTFLAGS"] = flags
    cmd = ["cargo", "build", "--release", "--target", TARGET, "--target-dir", tdir,
           "--config", f'patch.crates-io.sightglass-api.path="{run.SHIM}"']
    r = subprocess.run(cmd, cwd=os.path.join(run.src_dir(prog), run.CRATE), env=env, capture_output=True, text=True)
    if r.returncode != 0 or not os.path.exists(exe):
        log = os.path.join(run.OUT, f"build-ir-{side}-{prog}.log")
        open(log, "w").write(r.stdout + r.stderr)
        sys.exit(f"irdiff: {side} build of {prog} failed (see {log})")
    return exe


def ensure_image():
    if subprocess.run(["docker", "image", "inspect", IMAGE], capture_output=True).returncode == 0:
        return
    df = "FROM debian:trixie-slim\nRUN apt-get update && apt-get install -y --no-install-recommends valgrind && rm -rf /var/lib/apt/lists/*\n"
    r = subprocess.run(["docker", "build", "-t", IMAGE, "-"], input=df, text=True, capture_output=True)
    if r.returncode != 0:
        sys.exit("irdiff: could not build the callgrind image:\n" + r.stderr[-2000:])


def callgrind(prog, exe, tag):
    out_dir = os.path.join(run.OUT, "ir-out")
    os.makedirs(out_dir, exist_ok=True)
    out = os.path.join(out_dir, f"{prog}.{tag}.callgrind")
    if os.path.exists(out):
        os.remove(out)
    cmd = ["docker", "run", "--rm",
           "-v", f"{run.src_dir(prog)}:/src:ro", "-v", f"{os.path.dirname(exe)}:/exe:ro", "-v", f"{out_dir}:/out",
           "-w", "/src", IMAGE,
           "valgrind", "--tool=callgrind", f"--callgrind-out-file=/out/{os.path.basename(out)}",
           "--dump-line=yes", "--compress-strings=no", "--compress-pos=no",
           f"/exe/{os.path.basename(exe)}"]
    r = subprocess.run(cmd, capture_output=True, text=True)
    if r.returncode != 0 or not os.path.exists(out):
        sys.exit(f"irdiff: callgrind run failed for {tag}:\n{r.stderr[-2000:]}")
    return out


def demangle(names):
    tool = run.llvm_tool("llvm-cxxfilt") or shutil.which("llvm-cxxfilt") or "/opt/homebrew/opt/llvm/bin/llvm-cxxfilt"
    names = list(names)
    try:
        out = subprocess.run([tool], input="\n".join(names), capture_output=True, text=True).stdout.split("\n")
    except OSError:
        return {n: n for n in names}
    return dict(zip(names, out))


def short_path(p):
    p = os.path.normpath(p) if p and p != "?" else p
    for marker in ("/registry/src/", "/rustlib/src/rust/", "/rustc/"):
        i = p.find(marker)
        if i >= 0:
            rest = p[i + len(marker):]
            return rest.split("/", 1)[1] if marker != "/rustlib/src/rust/" and "/" in rest else rest
    return p


def parse(path):
    """callgrind out -> (total, {fn: self Ir}, {(file, line): self Ir})."""
    fns, lines, by = collections.Counter(), collections.Counter(), collections.Counter()
    fl = fn = cur = None
    skip = False
    for l in open(path, errors="replace"):
        l = l.rstrip("\n")
        if not l:
            continue
        c = l[0]
        if c.isdigit() or c in "+-*":
            if skip:  # the inclusive cost line of a call
                skip = False
                continue
            p = l.split()
            if len(p) >= 2 and p[0].isdigit():
                ir = int(p[1])
                fns[fn] += ir
                lines[(cur, int(p[0]))] += ir
                by[(fn, cur, int(p[0]))] += ir
            continue
        if l.startswith("fl="):
            fl = cur = l[3:]
        elif l.startswith(("fi=", "fe=")):
            cur = l[3:]
        elif l.startswith("fn="):
            fn = l[3:]
            cur = fl
        elif l.startswith("calls="):
            skip = True
    return sum(fns.values()), fns, lines, by


def norm_fn(n):
    # Crate disambiguators and monomorphization hashes differ per build.
    n = re.sub(r"\[[0-9a-f]{12,16}\]", "", n)
    n = re.sub(r"::h[0-9a-f]{16}$", "", n)
    n = re.sub(r"'\d+$", "", n)  # callgrind's recursion-level suffix
    return n


def main():
    ap = argparse.ArgumentParser(usage=__doc__.replace("%", "%%"))
    ap.add_argument("prog")
    ap.add_argument("--env", action="append", default=[])
    ap.add_argument("--top", type=int, default=25)
    ap.add_argument("--fn")
    ap.add_argument("--line", help="FILE:LINE (a suffix of the table's spelling): the functions that execute it")
    ap.add_argument("--skip-build", action="store_true")
    ap.add_argument("--backend", default=os.path.join(run.ROOT, "target", "release", run.DYLIB))
    a = ap.parse_args()
    binary = next((b for n, b, _ in run.BENCHES if n == a.prog), None)
    if binary is None:
        sys.exit(f"irdiff: unknown program {a.prog}")
    env = dict(kv.split("=", 1) for kv in a.env)
    ensure_image()
    sides = {}
    for side in ("llvm", "pliron"):
        exe = build(a.prog, binary, side, a.backend, env, a.skip_build)
        sides[side] = parse(callgrind(a.prog, exe, side))
    (lt, lf, ll, lb), (pt, pf, pl, pb) = sides["llvm"], sides["pliron"]
    print(f"{a.prog}: instructions llvm {lt / 1e6:.2f}M  pliron {pt / 1e6:.2f}M  x{pt / lt:.3f}  (+{(pt - lt) / 1e6:.2f}M)")
    dm = demangle(set(lf) | set(pf))
    lfn, pfn = collections.Counter(), collections.Counter()
    for k, v in lf.items():
        lfn[norm_fn(dm.get(k, k))] += v
    for k, v in pf.items():
        pfn[norm_fn(dm.get(k, k))] += v
    gap = pt - lt

    def table(title, lc, pc, fmt):
        print(f"\n{title}  (difference in instructions; % of the whole gap)")
        keys = sorted(set(lc) | set(pc), key=lambda k: pc[k] - lc[k], reverse=True)
        rows = keys[:a.top] + ["..."] + keys[-max(a.top // 3, 5):]
        for k in rows:
            if k == "...":
                print("      ...")
                continue
            d = pc[k] - lc[k]
            print(f"  {d:+11d} {100 * d / gap if gap else 0:+6.1f}%  {lc[k]:>10d} -> {pc[k]:>10d}  {fmt(k)}")

    if a.line:
        # Which functions execute this line's instructions, each side.
        want_f, _, want_n = a.line.rpartition(":")
        for name, tab in (("llvm", lb), ("pliron", pb)):
            c = collections.Counter()
            for (fn, f, n), v in tab.items():
                if n == int(want_n) and short_path(f or "?").endswith(want_f):
                    c[norm_fn(dm.get(fn, fn))] += v
            print(f"\n{a.line} in {name}: {sum(c.values())} instructions")
            for k, v in c.most_common(a.top):
                print(f"  {v:>10d}  {k[:120]}")
        return
    if a.fn:
        lfn = collections.Counter({k: v for k, v in lfn.items() if a.fn in k})
        pfn = collections.Counter({k: v for k, v in pfn.items() if a.fn in k})
    table("functions, self", lfn, pfn, lambda k: k[:110])
    if not a.fn:
        ln = collections.Counter({(short_path(f or "?"), n): v for (f, n), v in ll.items()})
        pn = collections.Counter({(short_path(f or "?"), n): v for (f, n), v in pl.items()})
        # Merge after shortening.
        table("source lines", ln, pn, lambda k: f"{k[0]}:{k[1]}")
        fl_, fp_ = collections.Counter(), collections.Counter()
        for (f, _), v in ln.items():
            fl_[f] += v
        for (f, _), v in pn.items():
            fp_[f] += v
        table("source files", fl_, fp_, lambda k: k)


if __name__ == "__main__":
    main()
