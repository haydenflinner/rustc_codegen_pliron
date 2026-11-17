#!/usr/bin/env -S python3 -I
"""Is a native delta within what code placement alone does?

  harness/placement.py heldout PROG[,PROG..]|all [--cand NAME:K=V,..]... [--copies K]
                       [--no-exact] [--no-cycles] [--rounds N] [--keep]
  harness/placement.py kernels FILTER [--cand NAME:K=V,..]... [--copies K] [--rounds N]

The native counterpart of `bench/wasm/run.mjs --perturb=N`. A change that
moves code moves loop heads relative to 16- and 64-byte lines: the
emitter's loop pads (executed `nop`s on a loop's fall-through entry) grow
or shrink, and loops cross other fetch lines. Callgrind counts the `nop`s
as instructions and the core's cycles follow the lines, so a candidate's
small loss can be placement and not worse code.

Copies. Copy 0 is the build as it is. Copy i > 0 sets
`PLIRON_PLACE_PHASE=P_i` (vendored Cranelift; a measurement aid, 0 = off):
every function starts P_i bytes past a 64-byte line behind a pad that is
never executed, and the emitter aligns loop heads, the 64-byte loop rule
and constants for those real addresses. The code is the default's, laid
out as if P_i bytes of code preceded it. P_i walks 36, 24, 12, 48, 20, 56,
44 (every residue mod 16 by copy 4). For cycles the copy is also relinked
under a random function order (seed i: ld64 `-order_file`, the same
objects), which moves every function's page and set bits.

  heldout   The held-out programs (`bench/heldout/run.py`).
            Exact: each copy is built for aarch64-linux-musl and run once
            under callgrind with `--dump-instr=yes` (docker, as `irdiff.py`);
            every executed instruction is classified from its encoding:
            `nop`, jump (`b`, `br`), conditional branch, call, return, load,
            store, other. A phase changes only `nop`s (the table checks it:
            the non-`nop` spread over copies must be 0). A candidate's delta
            is split the same way: `+23.3k, of which -1.2k nop` is direct
            evidence. Cycles: the macOS build of each copy (`ab.py`'s build
            cache) timed `--rounds` times interleaved under `/usr/bin/time -l`.
  kernels   The native kernels (`bench/run.sh`), each copy built with the
            keyed backend path. Per kernel: ns/op over `--rounds`
            interleaved epochs per copy, and exact instructions per op on
            macOS from two fixed-count runs (`-benchtime Nx` and 2N:
            the difference over N cancels setup). No `nop` split.

`--cand NAME:K=V,..` (repeatable) builds the same copies with that
environment. Verdict per program and kernel: the base's range over its
copies (min..max against the base's median) and the candidate's median
over its copies against the base's median. A candidate median inside the
base's range is placement; outside it is a real effect. Instructions are
judged without placement by the non-`nop` delta, which no copy moves.

Builds are cached by a stamp of their environment and the backend dylib.
Exact builds (`target/heldout/ir-pliron-pl-*`) are deleted after their
callgrind summary is written (`--keep` keeps them); summaries stay in
`target/placement/`. `--clean` deletes this tool's macOS build
directories (`target/heldout/ab-pl*`) afterwards.
"""
import argparse, collections, gzip, hashlib, importlib.util, json, os, random, re, shlex, shutil, \
    statistics, struct, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
HELD = os.path.join(ROOT, "bench", "heldout")
PHASES = [0, 36, 24, 12, 48, 20, 56, 44]
CLASSES = ["nop", "jump", "cbranch", "call", "ret", "load", "store", "other"]


def load(path, name):
    spec = importlib.util.spec_from_file_location(name, path)
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


run = load(os.path.join(HELD, "run.py"), "run")
OUT = os.path.join(ROOT, "target", "placement")
DYLIB = os.path.join(ROOT, "target", "release", run.DYLIB)


def log(msg):
    print(msg, file=sys.stderr, flush=True)


# --------------------------------------------------------------------------
# Instruction classes from the aarch64 encoding.

def klass(w):
    if w == 0xD503201F:
        return "nop"
    top6 = w & 0xFC000000
    if top6 == 0x14000000:
        return "jump"  # b
    if top6 == 0x94000000:
        return "call"  # bl
    if (w & 0xFF000010) == 0x54000000 or (w & 0x7E000000) in (0x34000000, 0x36000000):
        return "cbranch"  # b.cond, cbz/cbnz, tbz/tbnz
    if (w & 0xFE000000) == 0xD6000000:  # branch to register
        opc = (w >> 21) & 0xF
        return {0: "jump", 1: "call", 2: "ret"}.get(opc, "other")
    if (w & 0x0A000000) == 0x08000000:  # loads and stores
        if (w & 0x3B000000) == 0x18000000:
            return "load"  # ldr literal
        return "load" if w & (1 << 22) else "store"
    return "other"


def elf_read(path):
    """(executable sections [(address, bytes)], function symbols sorted
    [(address, size, name)]) of an ELF64 LE file."""
    data = open(path, "rb").read()
    assert data[:4] == b"\x7fELF" and data[4] == 2 and data[5] == 1, f"{path}: not ELF64 LE"
    shoff, = struct.unpack_from("<Q", data, 0x28)
    shentsize, shnum = struct.unpack_from("<HH", data, 0x3A)
    hdrs = [struct.unpack_from("<IIQQQQII", data, shoff + i * shentsize) for i in range(shnum)]
    secs, syms = [], []
    for _, typ, flags, addr, off, size, link, _ in hdrs:
        if typ == 1 and flags & 0x4:  # PROGBITS, EXECINSTR
            secs.append((addr, data[off:off + size]))
        if typ == 2:  # SYMTAB
            stroff = hdrs[link][4]
            for o in range(off, off + size, 24):
                name, info, _, shndx, value, ssize = struct.unpack_from("<IBBHQQ", data, o)
                if info & 0xF == 2 and shndx and value:  # STT_FUNC, defined
                    end = data.index(b"\0", stroff + name)
                    syms.append((value, ssize, data[stroff + name:end].decode(errors="replace")))
    syms.sort()
    return secs, syms


class Text:
    """Instruction words and the function symbol of an address. Callgrind's
    own `fn=` context is not the containing symbol on this code (an address
    of `HtmlWriter::run` showed up under three other functions), so costs
    are charged by the ELF symbol table instead."""

    def __init__(self, path):
        self.secs, self.syms = elf_read(path)
        self.starts = [s[0] for s in self.syms]

    def sym(self, a):
        import bisect
        i = bisect.bisect_right(self.starts, a) - 1
        if i >= 0 and a < self.syms[i][0] + max(self.syms[i][1], 4):
            return self.syms[i][2]
        return "?"

    def word(self, a):
        for base, b in self.secs:
            if base <= a < base + len(b) - 3:
                return struct.unpack_from("<I", b, a - base)[0]
        return None


# --------------------------------------------------------------------------
# Callgrind (exact counts) of one build.

def callgrind_summary(prog, exe, out_json):
    """Run `exe` once under callgrind; write {total, classes, fns} to out_json."""
    irdiff = load(os.path.join(HELD, "irdiff.py"), "irdiff")
    irdiff.ensure_image()
    tmpd = os.path.join(OUT, "cg-tmp")
    os.makedirs(tmpd, exist_ok=True)
    name = os.path.basename(out_json).replace(".json", ".callgrind")
    raw = os.path.join(tmpd, name)
    if os.path.exists(raw):
        os.remove(raw)
    cmd = ["docker", "run", "--rm",
           "-v", f"{run.src_dir(prog)}:/src:ro", "-v", f"{os.path.dirname(exe)}:/exe:ro", "-v", f"{tmpd}:/out",
           "-w", "/src", irdiff.IMAGE,
           "valgrind", "--tool=callgrind", f"--callgrind-out-file=/out/{name}",
           "--dump-instr=yes", "--dump-line=no", "--compress-strings=no", "--compress-pos=no",
           f"/exe/{os.path.basename(exe)}"]
    r = subprocess.run(cmd, capture_output=True, text=True)
    if r.returncode != 0 or not os.path.exists(raw):
        sys.exit(f"placement: callgrind failed for {exe}:\n{r.stderr[-2000:]}")
    text = Text(exe)
    per_addr = collections.Counter()
    skip = False
    last = 0
    for l in open(raw, errors="replace"):
        c = l[:1]
        if c.isdigit() or c in "+-*":
            if skip:
                skip = False
                continue
            p = l.split()
            if len(p) < 2:
                continue
            a = p[0]
            if a.startswith("+"):
                addr = last + int(a[1:], 0)
            elif a.startswith("-"):
                addr = last - int(a[1:], 0)
            elif a == "*":
                addr = last
            else:
                addr = int(a, 0)
            last = addr
            per_addr[addr] += int(p[1])
        elif l.startswith("calls="):
            skip = True
    os.remove(raw)
    classes = collections.Counter()
    fns = collections.defaultdict(collections.Counter)
    unknown = 0
    for addr, n in per_addr.items():
        w = text.word(addr)
        k = klass(w) if w is not None else "other"
        if w is None:
            unknown += n
        classes[k] += n
        fns[text.sym(addr)][k] += n
    dm = irdiff.demangle(set(fns))
    merged = collections.defaultdict(collections.Counter)
    for f, cnt in fns.items():
        merged[irdiff.norm_fn(dm.get(f, f))].update(cnt)
    res = {"total": sum(classes.values()), "classes": dict(classes), "unmapped": unknown,
           "fns": {f: dict(c) for f, c in merged.items()}}
    json.dump(res, open(out_json, "w"))
    return res


def env_of(spec):
    return dict(kv.split("=", 1) for kv in spec.split(",") if kv)


def stamp_of(env, extra=""):
    st = os.stat(DYLIB)
    return hashlib.sha1(repr((sorted(env.items()), st.st_mtime, st.st_size, extra)).encode()).hexdigest()[:12]


def exact_copy(prog, binary, label, env, phase, keep):
    """Callgrind summary of one copy (cached)."""
    irdiff = load(os.path.join(HELD, "irdiff.py"), "irdiff")
    e = dict(env)
    if phase:
        e["PLIRON_PLACE_PHASE"] = str(phase)
    key = stamp_of(e, prog)
    os.makedirs(os.path.join(OUT, "exact"), exist_ok=True)
    out = os.path.join(OUT, "exact", f"{prog}.{label}.p{phase}.{key}.json")
    if os.path.exists(out):
        return json.load(open(out))
    tag = f"pl-{label}-p{phase}"
    t0 = time.time()
    exe = irdiff.build(prog, binary, "pliron", DYLIB, e, False, tag)
    log(f"  exact {prog} {label} p{phase}: built in {time.time() - t0:.0f}s")
    t0 = time.time()
    res = callgrind_summary(prog, exe, out)
    log(f"  exact {prog} {label} p{phase}: callgrind {time.time() - t0:.0f}s, {res['total']} instructions")
    if not keep:
        shutil.rmtree(os.path.join(run.OUT, f"ir-pliron-{tag}"), ignore_errors=True)
    return res


# --------------------------------------------------------------------------
# macOS builds and cycles.

def relink(prog, binary, d, env, seed):
    """The macOS build in target/heldout/<d>/<prog> relinked under function order `seed`."""
    tdir = os.path.join(run.OUT, d, prog)
    base = os.path.join(tdir, "release", binary)
    out_dir = os.path.join(tdir, f"order{seed}")
    exe = os.path.join(out_dir, binary)
    if os.path.exists(exe) and os.path.getmtime(exe) > os.path.getmtime(base):
        return exe
    src = os.path.join(run.src_dir(prog), run.CRATE)
    run.sh(["cargo", "clean", "-p", "benchmark", "--release", "--target-dir", tdir], cwd=src, env=env)
    r = run.sh(["cargo", "rustc", "--release", "--bin", binary, "--target-dir", tdir,
                "--config", f'patch.crates-io.sightglass-api.path="{run.SHIM}"',
                "--", "--print", "link-args", "-C", "save-temps"], cwd=src, env=env)
    cmd = [l for l in r.stdout.splitlines() if l.strip()]
    if r.returncode != 0 or not cmd:
        log(f"  {prog}: {d}: no link command; order {seed} skipped")
        return base
    args = shlex.split(cmd[-1])
    nm = run.sh(["nm", "-n", "--defined-only", base]).stdout.split("\n")
    syms = [l.split()[2] for l in nm if len(l.split()) == 3 and l.split()[1] in ("t", "T")]
    random.Random(seed).shuffle(syms)
    os.makedirs(out_dir, exist_ok=True)
    with open(os.path.join(out_dir, "order.txt"), "w") as f:
        f.write("\n".join(syms) + "\n")
    args[args.index("-o") + 1] = exe
    args.append(f"-Wl,-order_file,{os.path.join(out_dir, 'order.txt')}")
    if run.sh(args).returncode != 0:
        log(f"  {prog}: {d}: relink under order {seed} failed")
        return base
    return exe


def mac_copy(prog, binary, label, env, phase, idx):
    ab = load(os.path.join(HELD, "ab.py"), "ab")
    e = dict(env)
    if phase:
        e["PLIRON_PLACE_PHASE"] = str(phase)
    spec = f"pl{label}-p{phase}:" + ",".join(f"{k}={v}" for k, v in sorted(e.items()))
    d, envd, _ = ab.side_env(prog, spec, DYLIB)
    exe = ab.ensure(prog, binary, spec, DYLIB)
    if idx > 0 and sys.platform == "darwin":
        exe = relink(prog, binary, d, envd, idx)
    return exe


def ensure_src(prog):
    dst = run.src_dir(prog)
    if os.path.isdir(os.path.join(dst, run.CRATE)):
        return
    upd = next(u for n, _, u in run.BENCHES if n == prog)
    shutil.copytree(os.path.join(run.SG, prog), dst, ignore=shutil.ignore_patterns("target", "*.wasm"))
    env = dict(os.environ, RUSTUP_TOOLCHAIN=run.TOOLCHAIN)
    for c in upd:
        run.sh(["cargo", "update", "-q", "-p", c], cwd=os.path.join(dst, run.CRATE), env=env)


# --------------------------------------------------------------------------
# Reporting.

def rng(vals, ref):
    """min..max of vals as % of ref."""
    return f"[{100 * (min(vals) / ref - 1):+.2f}..{100 * (max(vals) / ref - 1):+.2f}]"


def verdict(base_vals, cand_vals):
    """Real only when each side's median lies outside the other's range
    over copies (a candidate median just past the base's few copies is
    not evidence on its own)."""
    bm, cm = statistics.median(base_vals), statistics.median(cand_vals)
    apart = not (min(base_vals) <= cm <= max(base_vals)) and not (min(cand_vals) <= bm <= max(cand_vals))
    return bm, cm, ("REAL loss" if cm > bm else "REAL win") if apart else "placement"


def report_exact(prog, labels, ex, phases, top):
    base = ex["base"]
    tot = [r["total"] for r in base]
    nop = [r["classes"].get("nop", 0) for r in base]
    real = [t - n for t, n in zip(tot, nop)]
    print(f"\n{prog}: exact instructions (callgrind), {len(base)} copies, phases {phases}")
    print(f"  base   total {tot[0]}  range over copies {rng(tot, statistics.median(tot))}"
          f" ({min(tot) - tot[0]:+d}..{max(tot) - tot[0]:+d} vs copy 0)  nops {min(nop)}..{max(nop)}"
          f"  non-nop spread {max(real) - min(real)}")
    out = {"base": {"total": tot, "nop": nop}}
    for lab in labels[1:]:
        c = ex[lab]
        ct = [r["total"] for r in c]
        cn = [r["classes"].get("nop", 0) for r in c]
        bm, cm, v = verdict(tot, ct)
        d0 = {k: c[0]["classes"].get(k, 0) - base[0]["classes"].get(k, 0) for k in CLASSES}
        creal = [a - b for a, b in zip(ct, cn)]
        realmed = statistics.median(creal) - statistics.median(real)
        # The non-nop count moves with placement only through data
        # addresses (a longer .text shifts .rodata: memcpy/memchr paths);
        # beyond the base's own spread it is the candidate's code.
        noise = max(real) - min(real)
        vr = "noise" if abs(realmed) <= noise else ("REAL loss" if realmed > 0 else "REAL win")
        print(f"  {lab:<10} copy-0 delta {ct[0] - tot[0]:+d} ({100 * (ct[0] / tot[0] - 1):+.3f}%), of which nop {d0['nop']:+d},"
              f" non-nop {ct[0] - tot[0] - d0['nop']:+d}")
        print(f"  {'':<10} by class: " + "  ".join(f"{k} {d0[k]:+d}" for k in CLASSES if d0[k]))
        print(f"  {'':<10} total: median over copies {100 * (cm / bm - 1):+.3f}% (base range {rng(tot, bm)}) -> {v}")
        print(f"  {'':<10} non-nop: median {realmed:+.0f} ({100 * realmed / bm:+.3f}%), base spread {noise} -> {vr}")
        # Where: functions by non-nop and by nop delta, copy 0.
        bf, cf = base[0]["fns"], c[0]["fns"]
        rows = []
        for f in set(bf) | set(cf):
            b, k = bf.get(f, {}), cf.get(f, {})
            dn = k.get("nop", 0) - b.get("nop", 0)
            dt = sum(k.values()) - sum(b.values())
            rows.append((dt - dn, dn, f))
        print(f"  {'':<10} functions, copy 0 (non-nop delta, nop delta):")
        for dr, dn, f in sorted(rows, key=lambda r: -abs(r[0]) - abs(r[1]))[:top]:
            print(f"  {'':<12}{dr:+10d} {dn:+9d}  {f[:100]}")
        out[lab] = {"total": ct, "nop": cn, "verdict": v, "verdict_nonnop": vr, "copy0_classes": d0,
                    "median_pct": 100 * (cm / bm - 1), "nonnop_median": realmed}
    return out


def report_cycles(prog, labels, cyc):
    print(f"\n{prog}: macOS cycles and retired instructions (medians per copy)")
    out = {}
    base_c = [m[2] for m in cyc["base"]]
    base_i = [m[1] for m in cyc["base"]]
    print(f"  base   cycles copy 0 {base_c[0] / 1e6:.1f}M  range over copies {rng(base_c, statistics.median(base_c))}"
          f"   instructions range {rng(base_i, statistics.median(base_i))}")
    out["base"] = {"cycles": base_c, "instructions": base_i}
    for lab in labels[1:]:
        cc = [m[2] for m in cyc[lab]]
        ci = [m[1] for m in cyc[lab]]
        bm, cm, v = verdict(base_c, cc)
        bi, cim, vi = verdict(base_i, ci)
        # Instructions: placement moves the count only through nops, so the
        # range decides; on cycles a loaded machine widens it.
        print(f"  {lab:<10} cycles: copy 0 {100 * (cc[0] / base_c[0] - 1):+.2f}%, median over copies {100 * (cm / bm - 1):+.2f}%"
              f" (own range {rng(cc, cm)}) -> {v};  instructions median {100 * (cim / bi - 1):+.3f}% -> {vi}")
        out[lab] = {"cycles": cc, "instructions": ci, "verdict": v, "median_pct": 100 * (cm / bm - 1)}
    return out


# --------------------------------------------------------------------------

def heldout(a):
    ab = load(os.path.join(HELD, "ab.py"), "ab")
    progs = [n for n, _, _ in run.BENCHES] if a.progs == "all" else a.progs.split(",")
    phases = PHASES[:a.copies]
    labels = ["base"] + [c.split(":", 1)[0] for c in a.cand]
    envs = {"base": env_of(a.base)}
    for c in a.cand:
        n, _, kv = c.partition(":")
        envs[n] = dict(envs["base"], **env_of(kv))
    results = {}
    for prog in progs:
        binary = next(b for n, b, _ in run.BENCHES if n == prog)
        ensure_src(prog)
        res = {}
        if a.exact:
            ex = {lab: [exact_copy(prog, binary, lab, envs[lab], p, a.keep) for p in phases] for lab in labels}
            res["exact"] = report_exact(prog, labels, ex, phases, a.top)
        if a.cycles:
            exes = {lab: [mac_copy(prog, binary, lab, envs[lab], p, i) for i, p in enumerate(phases)] for lab in labels}
            cells = [(lab, i) for lab in labels for i in range(len(phases))]
            data = collections.defaultdict(list)
            for r in range(a.rounds):
                order = cells if r % 2 == 0 else cells[::-1]
                for cell in order:
                    data[cell].append(ab.once(prog, exes[cell[0]][cell[1]]))
            cyc = {lab: [[STAT[a.stat]([x[j] for x in data[(lab, i)]]) for j in range(3)]
                         for i in range(len(phases))] for lab in labels}
            res["cycles"] = report_cycles(prog, labels, cyc)
        sys.stdout.flush()
        results[prog] = res
        os.makedirs(OUT, exist_ok=True)
        json.dump(res, open(os.path.join(OUT, f"{prog}.{'-'.join(labels)}.json"), "w"), indent=1)
    if a.clean:
        for d in os.listdir(run.OUT):
            if d.startswith("ab-pl"):
                shutil.rmtree(os.path.join(run.OUT, d), ignore_errors=True)


# --------------------------------------------------------------------------
# Kernels.

def kernel_bin(label, env, phase, seed):
    """Build the bench target with the backend under env (+ phase, + order seed)."""
    e = dict(env)
    if phase:
        e["PLIRON_PLACE_PHASE"] = str(phase)
    kdir = os.path.join(OUT, "kernels")
    os.makedirs(kdir, exist_ok=True)
    key = stamp_of(e, f"seed{seed}")
    final = os.path.join(kdir, f"{label}-p{phase}-s{seed}-{key}")
    if os.path.exists(final):
        return final
    bp = load(os.path.join(HERE, "backend_path.py"), "backend_path")
    benv = dict(os.environ, **e)
    be = bp.backend_path(dylib=DYLIB, env=benv)

    def build(extra):
        cmd = ["cargo", "rustc", "-p", "pliron-bench", "--bench", "kernels", "--profile", "bench",
               "--message-format", "json-render-diagnostics", "--", "-Z", f"codegen-backend={be}"] + extra
        r = subprocess.run(cmd, cwd=ROOT, env=benv, capture_output=True, text=True)
        exe = ""
        for line in r.stdout.splitlines():
            try:
                m = json.loads(line)
            except ValueError:
                continue
            if m.get("reason") == "compiler-artifact" and m.get("executable"):
                exe = m["executable"]
        if r.returncode != 0 or not exe:
            sys.exit(f"placement: kernel build failed ({label} p{phase}):\n{r.stderr[-3000:]}")
        return exe

    exe = build([])
    if seed:
        nm = run.sh(["nm", "-n", "--defined-only", exe]).stdout.split("\n")
        syms = [l.split()[2] for l in nm if len(l.split()) == 3 and l.split()[1] in ("t", "T")]
        random.Random(seed).shuffle(syms)
        order = os.path.join(kdir, f"order-{label}-p{phase}-s{seed}-{key}.txt")
        with open(order, "w") as f:
            f.write("\n".join(syms) + "\n")
        exe = build(["-C", f"link-arg=-Wl,-order_file,{order}"])
    shutil.copy2(exe, final)
    return final


def bench_epoch(exe, filt, benchtime):
    r = subprocess.run([exe] + ([] if filt == "all" else [filt]) + ["-benchtime", benchtime, "-count", "1", "-cpu", "1"],
                       capture_output=True, text=True, cwd=ROOT)
    res = {}
    for line in r.stdout.splitlines():
        f = line.split()
        if len(f) >= 4 and f[0].startswith("Benchmark") and f[3] == "ns/op":
            res[f[0][len("Benchmark"):].rsplit("-", 1)[0].lower()] = float(f[2])
    return res


def retired(exe, kernel, n):
    p = subprocess.run(["/usr/bin/time", "-l", exe, "=" + kernel, "-benchtime", f"{n}x", "-count", "1", "-cpu", "1"],
                       capture_output=True, text=True, cwd=ROOT)
    m = re.search(r"^\s*(\d+)\s+instructions retired", p.stderr, re.M)
    return int(m.group(1)) if m else 0


STAT = {"min": min, "median": statistics.median}


def kernels(a):
    phases = PHASES[:a.copies]
    labels = ["base"] + [c.split(":", 1)[0] for c in a.cand]
    envs = {"base": env_of(a.base)}
    for c in a.cand:
        n, _, kv = c.partition(":")
        envs[n] = dict(envs["base"], **env_of(kv))
    exes = {}
    for lab in labels:
        for i, p in enumerate(phases):
            t0 = time.time()
            exes[(lab, i)] = kernel_bin(lab, envs[lab], p, i)
            log(f"  kernels {lab} p{p} s{i}: {time.time() - t0:.0f}s")
    cells = list(exes)
    data = collections.defaultdict(lambda: collections.defaultdict(list))
    for r in range(a.rounds):
        for cell in (cells if r % 2 == 0 else cells[::-1]):
            for k, ns in bench_epoch(exes[cell], a.filter, a.benchtime).items():
                data[cell][k].append(ns)
    names = sorted({k for c in cells for k in data[c]})
    out = {}
    for k in names:
        med = {c: STAT[a.stat](data[c][k]) for c in cells if data[c][k]}
        # Exact instructions per op: (I(2N) - I(N)) / N, N ~ 0.3 s of work;
        # the smaller of two tries of each (a process's setup moves ~1M).
        n = max(1, int(0.3e9 / med[("base", 0)]))
        ipo = {c: (min(retired(exes[c], k, 2 * n) for _ in range(2))
                   - min(retired(exes[c], k, n) for _ in range(2))) / n for c in cells}
        base_t = [med[("base", i)] for i in range(len(phases))]
        base_i = [ipo[("base", i)] for i in range(len(phases))]
        c0 = data[("base", 0)][k]
        print(f"\n{k}: ns/op copy 0 {base_t[0]:.1f}  base range {rng(base_t, statistics.median(base_t))}"
              f" (copy 0's own rounds: median/min {100 * (statistics.median(c0) / min(c0) - 1):+.1f}%)"
              f";  instructions/op {base_i[0]:.0f}, range {rng(base_i, statistics.median(base_i))}")
        out[k] = {"base": {"ns": base_t, "ipo": base_i}}
        for lab in labels[1:]:
            ct = [med[(lab, i)] for i in range(len(phases))]
            ci = [ipo[(lab, i)] for i in range(len(phases))]
            bm, cm, v = verdict(base_t, ct)
            bi, cim, vi = verdict(base_i, ci)
            print(f"  {lab:<10} ns/op median {100 * (cm / bm - 1):+.2f}% (copy 0 {100 * (ct[0] / base_t[0] - 1):+.2f}%,"
                  f" own range {rng(ct, cm)}) -> {v};  instructions/op median {100 * (cim / bi - 1):+.3f}% -> {vi}")
            out[k][lab] = {"ns": ct, "ipo": ci, "verdict": v, "verdict_ipo": vi}
    os.makedirs(OUT, exist_ok=True)
    json.dump(out, open(os.path.join(OUT, f"kernels.{'-'.join(labels)}.json"), "w"), indent=1)


def main():
    global DYLIB
    ap = argparse.ArgumentParser(usage=__doc__.replace("%", "%%"))
    sub = ap.add_subparsers(dest="cmd", required=True)
    for name in ("heldout", "kernels"):
        p = sub.add_parser(name)
        p.add_argument("progs" if name == "heldout" else "filter")
        p.add_argument("--cand", action="append", default=[], help="NAME:K=V,.. (repeatable)")
        p.add_argument("--base", default="", help="K=V,.. for every side")
        p.add_argument("--copies", type=int, default=4)
        p.add_argument("--rounds", type=int, default=7 if name == "heldout" else 9)
        p.add_argument("--stat", choices=["min", "median"], default="median" if name == "heldout" else "min",
                       help="per-copy statistic over rounds (min resists a loaded machine)")
        p.add_argument("--backend", default=DYLIB)
        if name == "heldout":
            p.add_argument("--no-exact", dest="exact", action="store_false")
            p.add_argument("--no-cycles", dest="cycles", action="store_false")
            p.add_argument("--keep", action="store_true")
            p.add_argument("--clean", action="store_true")
            p.add_argument("--top", type=int, default=8)
        else:
            p.add_argument("--benchtime", default="500ms")
    a = ap.parse_args()
    DYLIB = os.path.abspath(a.backend)
    a.copies = max(1, min(len(PHASES), a.copies))
    # Timings are only as good as the machine is quiet: say how busy it is.
    print(f"load average {' '.join(f'{x:.1f}' for x in os.getloadavg())} on {os.cpu_count()} cpus", flush=True)
    {"heldout": heldout, "kernels": kernels}[a.cmd](a)


if __name__ == "__main__":
    main()
