#!/usr/bin/env -S python3 -I
"""asm-vs: our machine code next to LLVM's, aligned by source line, by cause.

  bench/heldout/asmvs.py [--only a,b] [--runs N] [--top K]
                         [--fn SUBSTR] [--line FILE:LINE]
                         [--skip-build] [--skip-profile] [--json OUT]

`census.py` says which *function* loses time to LLVM. Inlining differs
between the backends, so function totals pair badly, and a function's
excess does not say what kind of instruction it is. This tool keys on what
both builds share: the source line of every instruction (the line table
row, i.e. the innermost inlined position), and classifies each instruction
(stack load/store, register copy, constant, compare, select, extend,
branch, ...). Self samples are attributed per instruction address, so

  * the class table says what the gap is made of, in time
    (e.g. "stack loads +310 samples, register copies +120"); on our side
    stack traffic is split into `spill-*` (regalloc spill slots) and
    `slot-*` (explicit stack slots the mid-end left in memory);
  * the line table ranks source lines by pliron-minus-LLVM samples,
    independent of which function the line was inlined into;
  * `--fn SUBSTR` / `--line FILE:LINE` print both sides' instructions for
    the matching lines side by side, with samples.

Both sides are built like `run.py` builds them, plus
`debug = "line-tables-only"` (target/heldout/{llvm,pliron}-g/<name>); the
machine code is the same as the plain release build's on both backends.
Our side is built with `PLIRON_REMARKS`, and `--fn` ends with the remarks
of the matching functions: why each call stayed a call, why a loop was not
vectorized. After relax's `asm <fn> vs=rustc`.
Writes target/heldout/asmvs.json.
"""
import argparse, bisect, collections, gzip, importlib.util, json, os, re, shutil, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))


def load(name):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, name + ".py"))
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


def load_harness(name):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, "..", "..", "harness", name + ".py"))
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


run = load("run")
census = load("census")
OUT = run.OUT

LOADS = ("ldr", "ldur", "ldp", "ldrb", "ldrh", "ldrsb", "ldrsh", "ldrsw", "ldurb", "ldurh",
         "ldursb", "ldursh", "ldursw", "ldpsw")
STORES = ("str", "stur", "stp", "strb", "strh", "sturb", "sturh")
CLASSES = ["spill-load", "spill-store", "slot-load", "slot-store", "stack-load", "stack-store", "load", "store", "mov-reg", "const", "cmp", "select",
           "extend", "cbranch", "jump", "call", "ret", "fp/vec", "alu", "other"]
FPREG = re.compile(r"(?<![\w.])(?:[vqdsh]\d{1,2})(?![\w])")


def classify(mn, ops):
    """Instruction class from an llvm-objdump mnemonic and operand string."""
    stack = "[sp" in ops or "[x29" in ops or "(%rsp" in ops or "(%rbp" in ops or "[rsp" in ops or "[rbp" in ops
    if mn in LOADS:
        return "stack-load" if stack else "load"
    if mn in STORES:
        return "stack-store" if stack else "store"
    if mn in ("mov", "fmov", "movq", "movl", "movaps", "movups"):
        a = [o.strip() for o in ops.split(",")]
        if len(a) == 2 and (a[1].startswith("#") or a[1].startswith("$")):
            return "const"
        if stack:
            return "stack-load" if ("[" in a[-1] or "(" in a[0]) else "stack-store"
        if "[" in ops or "(" in ops:
            return "load" if ("[" in a[-1] or "(" in a[0]) else "store"
        return "mov-reg"
    if mn in ("movz", "movk", "movn", "adrp", "adr", "movabsq"):
        return "const"
    if mn in ("cmp", "cmn", "tst", "ccmp", "ccmn", "fcmp", "fccmp", "cmpq", "cmpl", "testq", "testl",
              "cmpb", "cmpw", "testb"):
        return "cmp"
    if mn in ("cset", "csetm", "csel", "csinc", "csinv", "csneg", "cinc", "cinv", "cneg", "fcsel") \
            or mn.startswith(("cmov", "set")):
        return "select"
    if mn in ("uxtb", "uxth", "uxtw", "sxtb", "sxth", "sxtw", "movzbl", "movzwl", "movsbl", "movslq",
              "movzbq", "movsbq", "movswl"):
        return "extend"
    if mn == "and" and re.search(r"#0x(ff|ffff|ffffffff)$", ops):
        return "extend"
    if mn.startswith("b.") or mn in ("cbz", "cbnz", "tbz", "tbnz") or \
            (mn.startswith("j") and mn not in ("jmp", "jmpq")):
        return "cbranch"
    if mn in ("b", "br", "jmp", "jmpq"):
        return "jump"
    if mn in ("bl", "blr", "callq", "call"):
        return "call"
    if mn in ("ret", "retq"):
        return "ret"
    if FPREG.search(ops) or "xmm" in ops or "ymm" in ops:
        return "fp/vec"
    if mn in ("nop", "udf", "brk", "ud2", "hint", "bti", "paciasp", "autiasp"):
        return "other"
    return "alu"


def norm_path(p):
    """A source path both builds spell alike."""
    for mark, keep in (("/library/", "library/"), ("/registry/src/", None), ("/heldout/src/", None)):
        k = p.find(mark)
        if k >= 0:
            rest = p[k + len(mark):]
            if keep:
                return keep + rest
            return rest.split("/", 1)[1] if "/" in rest else rest
    return p


class Side:
    """One build: every instruction with its function, source line and class."""

    def __init__(self, exe):
        self.exe = exe
        self.addr = []     # sorted instruction addresses
        self.inst = []     # (fn key, line key, class, text)
        self.fn_start = {}  # fn key -> first address (objdump)
        dsym = exe + ".dSYM"
        cmd = [census.objdump(), "-d", "-l", "-C", "--no-show-raw-insn"]
        if os.path.isdir(dsym):
            cmd.append("--dsym=" + dsym)
        txt = subprocess.run(cmd + [exe], capture_output=True, text=True, errors="replace").stdout
        fn, line = None, "?"
        for l in txt.splitlines():
            if not l:
                continue
            if l[0] == ";":
                m = re.match(r"^; (.+):(\d+)$", l)
                if m:
                    line = f"{norm_path(m.group(1))}:{m.group(2)}"
                continue
            m = re.match(r"^([0-9a-f]+) <(.+)>:$", l)
            if m:
                fn = census.demangle_key(m.group(2))
                self.fn_start.setdefault(fn, int(m.group(1), 16))
                line = "?"
                continue
            m = re.match(r"^\s*([0-9a-f]+):\s+(\S+)\s*(.*)$", l)
            if m and fn is not None:
                mn, ops = m.group(2), m.group(3).strip()
                self.addr.append(int(m.group(1), 16))
                self.inst.append((fn, line, classify(mn, ops), f"{mn}\t{ops}".rstrip()))
        self._framed = set()  # functions seen allocating a fixed frame
        self.samples = collections.Counter()  # instruction index -> self samples
        self.total = 0

    def split_stack(self, frames):
        """Our side: `stack-*` -> `spill-*` (regalloc spill slots) or `slot-*`
        (explicit stack slots: allocas the mid-end left in memory), by the
        `[sp, #off]` offset against the function's frame regions (`frame`
        remarks: sp + outgoing args, then slots, then spills)."""
        # Offsets only mean frame regions while the fixed frame is open:
        # the prologue's callee-save pushes and the epilogue's pops address
        # `[sp, #off]` too, before `sub sp, sp` / after `add sp, sp`.
        cur, is_open = None, False
        for k, (fn, line, cls, text) in enumerate(self.inst):
            if fn != cur:
                cur, is_open = fn, False
            mn = text.split("\t", 1)[0]
            if text.startswith("sub\tsp, sp"):
                is_open = True
            elif text.startswith("add\tsp, sp"):
                is_open = False
            elif mn in ("ret", "b", "br") and not is_open:
                # Past an epilogue: the next block runs with the frame set up
                # (if this function has one at all).
                is_open = fn in self._framed
            if text.startswith("sub\tsp, sp"):
                self._framed.add(fn)
            fr = frames.get(fn)
            if not is_open or not cls.startswith("stack-") or fr is None:
                continue
            if "]!" in text or re.search(r"\], #", text):
                continue
            m = re.search(r"\[sp(?:, #(-?(?:0x)?[0-9a-f]+))?\]", text)
            if not m:
                continue
            off = int(m.group(1), 0) if m.group(1) else 0
            out, slots, spills = fr
            if out <= off < out + slots:
                self.inst[k] = (fn, line, "slot-" + cls[6:], text)
            elif out + slots <= off < out + slots + spills:
                self.inst[k] = (fn, line, "spill-" + cls[6:], text)

    def add_profile(self, path, proc_substr="benchmark"):
        """Attribute the profile's self samples to instructions."""
        p = json.load(gzip.open(path))
        sy = json.load(open(path.replace(".json.gz", ".json.syms.json")))
        st = sy["string_table"]
        libs = p["libs"]
        # objdump prints image addresses, the profile library-relative ones:
        # take the difference from any symbol both know.
        base = {}
        for e in sy["data"]:
            for r in e["symbol_table"]:
                k = census.demangle_key(st[r["symbol"]])
                if k in self.fn_start:
                    base[e["debug_name"]] = self.fn_start[k] - r["rva"]
                    break
        for t in p["threads"]:
            if proc_substr not in (t.get("processName") or t.get("name") or ""):
                continue
            ft, fu, rt, sk = t["frameTable"], t["funcTable"], t["resourceTable"], t["stackTable"]
            for s in t["samples"]["stack"]:
                if s is None:
                    continue
                self.total += 1
                f = sk["frame"][s]
                res = fu["resource"][ft["func"][f]]
                lib = rt["lib"][res] if res is not None and res >= 0 else None
                b = base.get(libs[lib]["debugName"]) if lib is not None else None
                if b is None:
                    continue
                a = ft["address"][f] + b
                k = bisect.bisect_right(self.addr, a) - 1
                if k >= 0 and a - self.addr[k] < 16:
                    self.samples[k] += 1

    def by(self, what):
        """Samples summed per class (2), line (1) or function (0)."""
        c = collections.Counter()
        for k, n in self.samples.items():
            c[self.inst[k][what]] += n
        return c

    def line_classes(self):
        d = collections.defaultdict(collections.Counter)
        for k, n in self.samples.items():
            d[self.inst[k][1]][self.inst[k][2]] += n
        return d

    def listing(self, pred):
        """line -> [(addr, samples, class, text, fn)] for instructions `pred` accepts."""
        d = collections.defaultdict(list)
        for k, (fn, line, cls, text) in enumerate(self.inst):
            if pred(fn, line):
                d[line].append((self.addr[k], self.samples.get(k, 0), cls, text, fn))
        return d


def remarks_prefix(prog):
    return os.path.join(OUT, "remarks", prog, "r")


def frames(prog):
    """Demangled function -> (outgoing, slots, spills) bytes, from `frame` remarks."""
    rm = load_harness("remarks")
    out = {}
    for p, f, s, why, n in rm.load(remarks_prefix(prog)):
        if p == "frame":
            d = dict(kv.split("=") for kv in why.split())
            out[census.demangle_key(f)] = (int(d["outgoing"]), int(d["slots"]), int(d["spills"]))
    return out


def show_remarks(prog, fn):
    """Why the calls of our functions matching `fn` stayed calls, and their loops scalar."""
    rm = load_harness("remarks")
    rows = [r for r in rm.load(remarks_prefix(prog)) if fn in r[1] and "external" not in r[3] and r[0] != "frame"]
    if rows:
        print(f"\n-- remarks (harness/remarks.py {remarks_prefix(prog)} --fn ...)")
    for p, f, s, why, n in rows:
        print(f"  {p}: {s[:110]}\n      {why}" + (f"  (x{n})" if n > 1 else ""))


def build_sides(prog, binary, backend, skip, extra=()):
    base = dict(os.environ, RUSTUP_TOOLCHAIN=run.TOOLCHAIN, CARGO_TERM_COLOR="never",
                CARGO_PROFILE_RELEASE_DEBUG="line-tables-only",
                CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO="packed")
    base.pop("RUSTFLAGS", None)
    envs = {"llvm-g": base, "pliron-g": dict(base, RUSTFLAGS=f"-Zcodegen-backend={backend}",
                                             PLIRON_REMARKS=remarks_prefix(prog),
                                             **dict(kv.split("=", 1) for kv in extra))}
    exes = {}
    for side, env in envs.items():
        exe = os.path.join(OUT, side, prog, "release", binary)
        if not (skip and os.path.exists(exe)):
            if side == "pliron-g":
                shutil.rmtree(os.path.dirname(remarks_prefix(prog)), ignore_errors=True)
                os.makedirs(os.path.dirname(remarks_prefix(prog)))
            exe = run.build(prog, binary, side, env)
            if exe is None:
                sys.exit(f"asmvs: {side} build of {prog} failed")
        exes[side] = exe
    return exes


def show_lines(sides, pred_fn, top):
    """Both sides' instructions for the lines `pred_fn` selects on our side."""
    ours, llvm = sides["pliron-g"], sides["llvm-g"]
    mine = ours.listing(pred_fn)
    lines = sorted(mine, key=lambda ln: -sum(i[1] for i in mine[ln]))[:top]
    want = set(lines)
    theirs = llvm.listing(lambda fn, line: line in want)
    def hottest(v):
        per_fn = collections.defaultdict(list)
        for i in v:
            per_fn[i[4]].append(i)
        return max(per_fn.values(), key=lambda c: (sum(i[1] for i in c), -len(c)), default=[]), len(per_fn)

    for ln in lines:
        ps = sum(i[1] for i in mine[ln])
        mine[ln], ncopies = hottest(mine[ln])
        # A line may sit in several functions (inlined copies): show the copy
        # that ran most on each side, and say how many there are.
        best, lcopies = hottest(theirs.get(ln, []))
        ls = sum(i[1] for i in theirs.get(ln, []))
        cp = lambda n: f" (hottest of {n} copies)" if n > 1 else ""
        print(f"\n-- {ln}: pliron {ps} samples / {len(mine[ln])} insts{cp(ncopies)},"
              f" LLVM {ls} samples / {len(best)} insts{cp(lcopies)}")
        for k in range(max(len(mine[ln]), len(best))):
            def cell(v):
                if k >= len(v):
                    return ""
                _, n, cls, text, _ = v[k]
                return f"{n or '':>5} {cls:<11} {text.expandtabs(6)}"
            print(f"  {cell(mine[ln]):<58} | {cell(best)}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--only")
    ap.add_argument("--runs", type=int, default=100)
    ap.add_argument("--top", type=int, default=15)
    ap.add_argument("--fn", help="print the lines of our functions whose name contains this")
    ap.add_argument("--line", help="print one source line (a suffix of FILE:LINE as the tables spell it)")
    ap.add_argument("--env", action="append", default=[], metavar="K=V",
                    help="extra backend environment for our side (an opt-in knob to look at)")
    ap.add_argument("--skip-build", action="store_true")
    ap.add_argument("--skip-profile", action="store_true")
    ap.add_argument("--backend", default=os.path.join(run.ROOT, "target", "release", run.DYLIB))
    ap.add_argument("--json", default=os.path.join(OUT, "asmvs.json"))
    a = ap.parse_args()
    report = {}
    for prog, binary, _ in run.BENCHES:
        if a.only and prog not in a.only.split(","):
            continue
        if not os.path.isdir(run.src_dir(prog)):
            sys.exit(f"asmvs: {run.src_dir(prog)} missing; run bench/heldout/run.py once first")
        exes = build_sides(prog, binary, a.backend, a.skip_build, a.env)
        sides = {}
        for side, exe in exes.items():
            prof = os.path.join(OUT, f"asmvs-{side}-{prog}.json.gz")
            if not (a.skip_profile and os.path.exists(prof)):
                census.profile(exe, run.src_dir(prog), a.runs, prof)
            s = Side(exe)
            if side == "pliron-g":
                s.split_stack(frames(prog))
            s.add_profile(prof)
            sides[side] = s
        ours, llvm = sides["pliron-g"], sides["llvm-g"]
        gap = ours.total - llvm.total
        print(f"\n== {prog}: pliron {ours.total} / LLVM {llvm.total} samples"
              f" ({ours.total / max(llvm.total, 1):.3f}x), gap {gap}")
        if a.fn or a.line:
            if a.fn:
                show_lines(sides, lambda fn, line: a.fn in fn, a.top)
                show_remarks(prog, a.fn)
            else:
                show_lines(sides, lambda fn, line: line.endswith(a.line), a.top)
            continue
        pc, lc = ours.by(2), llvm.by(2)
        print(f"\n{'class':<12} {'pliron':>7} {'llvm':>7} {'gap':>6} {'of gap':>7}")
        classes = []
        for cls in sorted(CLASSES, key=lambda c: lc[c] - pc[c]):
            d = pc[cls] - lc[cls]
            if pc[cls] or lc[cls]:
                print(f"{cls:<12} {pc[cls]:7d} {lc[cls]:7d} {d:+6d} {100 * d / gap if gap else 0:6.1f}%")
            classes.append({"class": cls, "pliron": pc[cls], "llvm": lc[cls]})
        pl, ll = ours.by(1), llvm.by(1)
        pk, lk = ours.line_classes(), llvm.line_classes()
        rows = sorted(((pl[ln] - ll[ln], ln) for ln in set(pl) | set(ll)), reverse=True)
        print(f"\n{'gap':>6} {'pliron':>7} {'llvm':>6}  line  [classes that grew]")
        lines = []
        for d, ln in rows[:a.top]:
            delta = {c: pk[ln][c] - lk[ln][c] for c in set(pk[ln]) | set(lk[ln])}
            grew = " ".join(f"{c}{v:+d}" for c, v in sorted(delta.items(), key=lambda kv: -kv[1])[:3] if v > 0)
            print(f"{d:+6d} {pl[ln]:7d} {ll[ln]:6d}  {ln}  [{grew}]")
            lines.append({"line": ln, "gap": d, "pliron": pl[ln], "llvm": ll[ln], "classes": delta})
        for d, ln in rows[-3:]:
            if d < 0:
                print(f"{d:+6d} {pl[ln]:7d} {ll[ln]:6d}  {ln}  (LLVM-heavy)")
        report[prog] = {"pliron_samples": ours.total, "llvm_samples": llvm.total,
                        "classes": classes, "lines": lines}
    if not (a.fn or a.line):
        with open(a.json, "w") as f:
            json.dump(report, f, indent=1)


if __name__ == "__main__":
    main()
