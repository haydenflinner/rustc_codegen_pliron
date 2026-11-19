#!/usr/bin/env -S python3 -I
"""Register-allocation overhead, ours against LLVM's: the push's metric.

  harness/rastat.py dyn [--only a,b] [--env K=V ...] [--tag T] [--skip-build] [--top N] [--save F]
  harness/rastat.py static [--corpus std] [--every N] [--env K=V ...] [--clif-check BIN] [--top N] [--save F]
  harness/rastat.py cmp A.json B.json [--top N]
  harness/rastat.py selftest          the move-kind classifier on a hand-written function

Classes (one shared classifier, aarch64; docs/regalloc.md "Metric"):

  st     store addressed off sp/x29 (spill stores and locals; static: spill slots only in st/ld,
         explicit stack slots counted apart in `slot`)
  ld     load addressed off sp/x29, the epilogue's restores excluded
  mov    register-to-register copy in one bank (mov xA, xB / fmov dA, dB / mov vA.16b, vB.16b)
  csr    callee-saved register (x19-x28, d8-d15) saves and restores: a store of a callee-saved
         register no earlier instruction wrote, and the loads of the epilogue run before
         `ret` (or a tail branch)
  frame  the frame record (x29, x30) and sp moves around it: reported, not overhead
  overhead = st + ld + mov + csr

`dyn`: exact executed instructions. Both compilers build each held-out program for
aarch64-unknown-linux-musl (bench/heldout/irdiff.py's builds), each runs once under
callgrind (`--dump-instr=yes`, docker), every executed address is classified from
llvm-objdump. Prints per program ours/LLVM per class, then the functions with the most
excess overhead (by demangled name; inlining differs, so read it as where, not why).

`static`: the fast loop, no docker. `clif-check codegen --vcode` over a corpus (pipeline +
Cranelift, the backend's aarch64 flags), the same classes per function, each instruction
weighted 8^min(loop depth, 3). About a minute for std. `--env K=V` sets knobs for that run.

`dyn` also splits executed `mov`s by kind (`move_kinds`: edge, callarg, callret, ret,
inblock; docs/regalloc.md "Move kinds") and ends with the excess per kind.

`--save F` writes the numbers as JSON; `cmp` compares two saved runs of the same kind
(totals, and the functions that moved most).
"""
import argparse, collections, importlib.util, json, os, re, shutil, subprocess, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
HELD = os.path.join(ROOT, "bench", "heldout")
OUT = os.path.join(ROOT, "target", "rastat")
CLASSES = ["st", "ld", "mov", "csr"]

# ---------------------------------------------------------------- classifier

CSR = {f"x{i}" for i in range(19, 29)} | {f"v{i}" for i in range(8, 16)}
FRAMEREG = {"x29", "x30"}
NOWRITE = {"cmp", "cmn", "tst", "fcmp", "fcmpe", "ccmp", "ccmn", "fccmp", "fccmpe", "b", "bl", "br", "blr",
           "ret", "cbz", "cbnz", "tbz", "tbnz", "nop", "prfm", "hint", "dmb", "dsb", "isb", "brk", "udf",
           "svc", "bti", "paciasp", "autiasp", "pacibsp", "autibsp", "csdb", "yield"}
REG = re.compile(r"^(?:([xw])(\d+)|([bhsdqv])(\d+)(?:\.\w+)?|(fp|lr|sp|wsp|xzr|wzr))$")


def canon(r):
    """`w3` -> `x3`, `d9`/`q9`/`v9.16b` -> `v9`, `fp` -> `x29`; None if not a register."""
    m = REG.match(r.strip())
    if not m:
        return None
    if m.group(1):
        return "x" + m.group(2)
    if m.group(3):
        return "v" + m.group(4)
    return {"fp": "x29", "lr": "x30"}.get(m.group(5), m.group(5))


def split_ops(ops):
    """Operands, keeping `[...]` and `{...}` groups whole."""
    out, depth, cur = [], 0, ""
    for ch in ops:
        if ch in "[{":
            depth += 1
        elif ch in "]}":
            depth -= 1
        if ch == "," and depth == 0:
            out.append(cur.strip())
            cur = ""
        else:
            cur += ch
    if cur.strip():
        out.append(cur.strip())
    return out


def parse(text):
    """'ldp x19, x20, [sp, #16]' -> (mnemonic, [operands])."""
    t = text.split(";")[0].split("//")[0].strip()
    if not t:
        return None, []
    p = t.split(None, 1)
    return p[0].lower(), split_ops(p[1]) if len(p) > 1 else []


STACK_BASE = re.compile(r"^\[\s*(sp|x29|fp)\b")


def mem_info(mn, ops):
    """(is_load, is_store, data regs, base is sp/fp, writeback) for a memory instruction, else None."""
    if not (mn.startswith("ld") or mn.startswith("st")) or mn.startswith(("stlr", "ldar")) and False:
        return None
    mem = next((i for i, o in enumerate(ops) if o.startswith("[")), None)
    if mem is None:
        return None
    data = [canon(o) for o in ops[:mem]]
    if mn.startswith(("stxr", "stlxr", "stxp", "stlxp")):
        data = data[1:]  # status register first
    data = [d for d in data if d]
    m = STACK_BASE.match(ops[mem])
    wb = ops[mem].endswith("!") or len(ops) > mem + 1
    return mn.startswith("ld"), mn.startswith("st"), data, bool(m), wb


def writes(mn, ops):
    mi = mem_info(mn, ops)
    if mi:
        is_ld, _, data, _, wb = mi
        w = list(data) if is_ld else []
        if wb:
            b = canon(ops[next(i for i, o in enumerate(ops) if o.startswith("["))].strip("[]!").split(",")[0])
            if b:
                w.append(b)
        return w
    if mn in NOWRITE or mn.startswith("b."):
        return []
    r = canon(ops[0]) if ops else None
    return [r] if r else []


def is_move(mn, ops):
    if len(ops) != 2:
        return False
    a, b = canon(ops[0]), canon(ops[1])
    if not a or not b or a in ("sp", "wsp", "xzr", "wzr") or b in ("sp", "wsp", "xzr", "wzr"):
        return False
    if mn == "mov":
        # `mov vA.16b, vB.16b` or general registers; not `mov v0.s[1], ...` lane inserts
        if "[" in ops[0] or "[" in ops[1]:
            return False
        return a[0] == b[0]
    if mn == "fmov":
        return a[0] == "v" and b[0] == "v"
    return False


def epilogue_set(insts):
    """Indices of the epilogue's restore loads: scanning back from each `ret` (or a tail
    branch, `tail(i)`) over sp/fp loads into callee-saved or frame registers and sp moves."""
    out = set()
    for i, (mn, ops, tail) in enumerate(insts):
        if mn not in ("ret", "retaa", "retab") and not tail:
            continue
        j = i - 1
        while j >= 0:
            m2, o2, _ = insts[j]
            mi = mem_info(m2, o2)
            if mi and mi[0] and mi[3] and mi[2] and all(r in CSR or r in FRAMEREG for r in mi[2]):
                out.add(j)
            elif m2 in ("add", "sub", "mov") and o2 and canon(o2[0]) == "sp":
                pass
            elif m2 in ("autiasp", "autibsp", "hint", "nop"):
                pass
            else:
                break
            j -= 1
    return out


def classify_fn(insts, spill_from=None):
    """insts: [(mnemonic, operands, is_tail_branch)] in address/emission order.
    -> class per instruction: st, ld, mov, csr, frame, slot (static: explicit stack slot), None."""
    epi = epilogue_set(insts)
    written = set()
    cls = []
    for i, (mn, ops, _) in enumerate(insts):
        c = None
        mi = mem_info(mn, ops)
        if mi and mi[3]:
            is_ld, is_st, data, _, wb = mi
            if i in epi:
                c = "csr" if any(r in CSR for r in data) else "frame"
            elif is_st and data and all(r in CSR or r in FRAMEREG for r in data) \
                    and not any(r in written for r in data if r in CSR):
                c = "csr" if any(r in CSR for r in data) else "frame"
            elif is_st and data and all(r in FRAMEREG for r in data) and wb:
                c = "frame"
            else:
                c = "st" if is_st else "ld"
                if spill_from is not None and c in ("st", "ld"):
                    m = re.match(r"^\[\s*(sp|fp|x29)\s*(?:,\s*#(-?(?:0x)?[0-9a-f]+))?", ops[[k for k, o in enumerate(ops) if o.startswith("[")][0]])
                    base, off = m.group(1), m.group(2)
                    off = int(off, 16) if off and off.lstrip("-").startswith("0x") else int(off or 0)
                    if base == "sp" and off < spill_from:
                        c = "slot"
                    elif base != "sp" and off > 0:
                        c = "slot"  # incoming stack arguments
        elif is_move(mn, ops):
            c = "mov"
        elif mn in ("add", "sub", "mov") and ops and canon(ops[0]) in ("sp", "x29") :
            c = "frame"
        cls.append(c)
        written.update(writes(mn, ops))
    return cls

# ---------------------------------------------------------------- move kinds

MOVE_KINDS = ["edge", "callarg", "callret", "ret", "inblock"]
ARGREG = {f"x{i}" for i in range(8)} | {f"v{i}" for i in range(8)}
BRANCH = re.compile(r"^(b|b\..+|cbz|cbnz|tbz|tbnz|br|ret|retaa|retab)$")
CALL = ("bl", "blr")


def move_kinds(fn_start, insts, cls):
    """Kind of each `mov`-class instruction from its context (docs/regalloc.md
    "Move kinds"): `callarg` (into an argument register, a call follows within the
    same straight line), `callret` (out of x0-x7/v0-v7 right after a call), `ret`
    (into a return register ahead of `ret` or a tail branch), `edge` (a run of moves
    that ends in a jump or falls into a branch target, or starts at one: where
    regalloc2 puts block-parameter moves), `inblock` (the rest)."""
    n = len(insts)
    targets = set()
    for addr, mn, ops, tail in insts:
        if BRANCH.match(mn) and mn not in ("br", "ret", "retaa", "retab") and not tail:
            m = re.match(r"^(?:0x)?([0-9a-f]+)$", ops[-1].split()[0]) if ops else None
            if m:
                targets.add(int(m.group(1), 16))
    starts = set(targets)
    for i, (addr, mn, ops, tail) in enumerate(insts[:-1]):
        if BRANCH.match(mn) or mn in CALL:
            starts.add(insts[i + 1][0])
    kinds = [None] * n
    i = 0
    while i < n:
        if cls[i] != "mov":
            i += 1
            continue
        j = i
        while j + 1 < n and cls[j + 1] == "mov" and insts[j + 1][0] not in starts:
            j += 1
        prev = insts[i - 1] if i > 0 else None
        nxt = insts[j + 1] if j + 1 < n else None
        run_starts_block = insts[i][0] in starts
        nxt_starts_block = nxt is not None and nxt[0] in starts
        for k in range(i, j + 1):
            _, mn, ops, _ = insts[k]
            dst, src = canon(ops[0]), canon(ops[1])
            kind = None
            if prev is not None and prev[1] in CALL and src in ARGREG:
                kind = "callret"
            if kind is None and dst in ARGREG:
                # a call within the next 8 instructions of the same block
                for m in range(j + 1, min(n, j + 9)):
                    a2, m2, o2, t2 = insts[m]
                    if m2 in CALL:
                        kind = "callarg"
                        break
                    if BRANCH.match(m2) or t2 or a2 in starts:
                        break
            if kind is None and dst in ARGREG:
                # only the epilogue (restores, sp moves) between it and the return
                for m in range(j + 1, min(n, j + 12)):
                    m2, o2, t2 = insts[m][1], insts[m][2], insts[m][3]
                    if m2 in ("ret", "retaa", "retab") or t2:
                        kind = "ret"
                        break
                    mi = mem_info(m2, o2)
                    epi = (mi and mi[0] and mi[3] and all(r in CSR or r in FRAMEREG for r in mi[2])) \
                        or (m2 in ("add", "sub") and o2 and canon(o2[0]) == "sp") \
                        or m2 in ("autiasp", "autibsp", "hint", "nop")
                    if not epi or insts[m][0] in starts:
                        break
            if kind is None:
                ends_jump = nxt is not None and nxt[1] == "b" and not nxt[3]
                if ends_jump or nxt_starts_block or run_starts_block:
                    kind = "edge"
                else:
                    kind = "inblock"
            kinds[k] = kind
        i = j + 1
    return kinds


# ---------------------------------------------------------------- static


def load_mod(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


def corpus_root(name):
    if os.path.isdir(name):
        return name
    d = os.path.join(ROOT, "target/corpus", name)
    if os.path.isdir(d):
        return d
    common = subprocess.run(["git", "rev-parse", "--path-format=absolute", "--git-common-dir"], cwd=ROOT,
                            capture_output=True, text=True).stdout.strip()
    d2 = os.path.join(os.path.dirname(common), "target/corpus", name)
    return d2 if os.path.isdir(d2) else None


def score_vcode_file(path):
    """`--vcode` file -> {function file: {class: weighted count, 'n_<class>': static count, 'insts', 'winsts'}}."""
    fns = {}
    cur, lines = None, []

    def flush():
        if cur is None:
            return
        spill_from = None
        depth, insts, weights = 0, [], []
        for l in lines:
            t = l.strip()
            if t.startswith("; frame"):
                kv = dict(x.split("=") for x in t.split()[2:])
                spill_from = int(kv["out"]) + int(kv["slots"])
                continue
            if not t or t.startswith("unwind"):
                continue
            if re.match(r"^block\d+:", t):
                m = re.search(r"depth=(\d+)", t)
                depth = int(m.group(1)) if m else 0
                continue
            mn, ops = parse(t)
            if not mn:
                continue
            insts.append((mn, ops, mn.startswith("return_call")))
            weights.append(8 ** min(depth, 3))
        cls = classify_fn(insts, spill_from)
        row = collections.Counter()
        for c, w in zip(cls, weights):
            row["insts"] += 1
            row["winsts"] += w
            if c:
                row[c] += w
                row["n_" + c] += 1
        fns[cur] = dict(row)

    for l in open(path, errors="replace"):
        if l.startswith("== "):
            flush()
            cur, lines = l[3:].strip(), []
        else:
            lines.append(l)
    flush()
    return fns


def cmd_static(a):
    os.makedirs(OUT, exist_ok=True)
    d = corpus_root(a.corpus)
    if not d:
        sys.exit(f"rastat: no corpus {a.corpus}")
    cc = a.clif_check or os.path.join(ROOT, "target", os.environ.get("PLIRON_PROFILE", "release"), "clif-check")
    env = dict(os.environ, **dict(kv.split("=", 1) for kv in a.env))
    tag = re.sub(r"[^A-Za-z0-9_.=-]", "_", "_".join(a.env))[:80] or "base"
    vc = os.path.join(OUT, f"static-{a.corpus.replace('/', '_')}-{tag}.vcode")
    js = vc[:-6] + ".json"
    cmd = [cc, "codegen", d, js, "--vcode", vc, "--every", str(a.every)]
    r = subprocess.run(cmd, env=env, capture_output=True, text=True)
    if not os.path.exists(vc):
        sys.exit(f"rastat: clif-check failed:\n{r.stdout[-2000:]}{r.stderr[-2000:]}")
    fns = score_vcode_file(vc)
    os.remove(vc)
    tot = collections.Counter()
    for row in fns.values():
        tot.update(row)
    res = {"kind": "static", "corpus": a.corpus, "every": a.every, "env": a.env, "total": dict(tot), "functions": fns}
    print_static(res, a.top)
    if a.save:
        json.dump(res, open(a.save, "w"))
        print(f"saved {a.save}")


def overhead(row, w=True):
    p = "" if w else "n_"
    return sum(row.get(p + c, 0) for c in CLASSES)


def print_static(res, top):
    t = res["total"]
    print(f"static {res['corpus']} every {res['every']} {' '.join(res['env']) or '(defaults)'}: {len(res['functions'])} functions, "
          f"{t.get('insts', 0)} insts")
    print(f"  {'class':<8}{'static':>10}{'weighted':>14}")
    for c in CLASSES + ["slot", "frame"]:
        print(f"  {c:<8}{t.get('n_' + c, 0):>10}{t.get(c, 0):>14}")
    print(f"  {'overhead':<8}{overhead(t, False):>10}{overhead(t):>14}   (st+ld+mov+csr; weighted 8^depth)")
    print(f"\n  top {top} functions by weighted overhead")
    for f, row in sorted(res["functions"].items(), key=lambda kv: -overhead(kv[1]))[:top]:
        print(f"  {overhead(row):>10}  " + " ".join(f"{c} {row.get(c, 0)}" for c in CLASSES) + f"  {f[:90]}")

# ---------------------------------------------------------------- dynamic


def prepare_src(run, prog, update):
    dst = run.src_dir(prog)
    if os.path.isdir(dst):
        return
    sg = run.SG
    if not os.path.isdir(sg):
        alt = os.path.join(os.path.dirname(os.path.dirname(subprocess.run(
            ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"], cwd=ROOT, capture_output=True,
            text=True).stdout.strip())), "are-we-fast-yet", "sightglass", "benchmarks")
        sg = alt
    shutil.copytree(os.path.join(sg, prog), dst, ignore=shutil.ignore_patterns("target", "*.wasm"))
    env = dict(os.environ, RUSTUP_TOOLCHAIN=run.TOOLCHAIN)
    for c in update:
        subprocess.run(["cargo", "update", "-q", "-p", c], cwd=os.path.join(dst, run.CRATE), env=env)


def callgrind_instr(run, irdiff, prog, exe, tag):
    out_dir = os.path.join(run.OUT, "ir-out")
    os.makedirs(out_dir, exist_ok=True)
    out = os.path.join(out_dir, f"{prog}.{tag}.instr.callgrind")
    if os.path.exists(out) and os.path.getmtime(out) > os.path.getmtime(exe) and os.path.getsize(out) > 0:
        return out
    if os.path.exists(out):
        os.remove(out)
    cmd = ["docker", "run", "--rm",
           "-v", f"{run.src_dir(prog)}:/src:ro", "-v", f"{os.path.dirname(exe)}:/exe:ro", "-v", f"{out_dir}:/out",
           "-w", "/src", irdiff.IMAGE,
           "valgrind", "--tool=callgrind", f"--callgrind-out-file=/out/{os.path.basename(out)}",
           "--dump-instr=yes", "--dump-line=no", "--compress-strings=no", "--compress-pos=no",
           f"/exe/{os.path.basename(exe)}"]
    r = subprocess.run(cmd, capture_output=True, text=True)
    if r.returncode != 0 or not os.path.exists(out):
        sys.exit(f"rastat: callgrind failed for {prog} {tag}:\n{r.stderr[-2000:]}")
    return out


def parse_instr(path):
    """callgrind --dump-instr out -> ({address: self Ir}, {fn name: min address})."""
    ir, fmin = collections.Counter(), {}
    fn, skip = None, False
    for l in open(path, errors="replace"):
        c = l[:1]
        if c == "0" or c.isdigit() or c in "+-*":
            if skip:
                skip = False
                continue
            p = l.split()
            if len(p) >= 2:
                a = int(p[0], 16) if p[0].startswith("0x") else int(p[0])
                ir[a] += int(p[1])
                if fn is not None and (fn not in fmin or a < fmin[fn]):
                    fmin[fn] = a
            continue
        if l.startswith("fn="):
            fn = l[3:].strip()
        elif l.startswith("calls="):
            skip = True
    return ir, fmin


def objdump_fns(run, exe):
    """[(name, start, [(addr, mnemonic, ops, tail)])] from llvm-objdump."""
    tool = run.llvm_tool("llvm-objdump") or shutil.which("llvm-objdump") or "/opt/homebrew/opt/llvm/bin/llvm-objdump"
    txt = subprocess.run([tool, "-d", "--no-show-raw-insn", exe], capture_output=True, text=True).stdout
    fns, cur = [], None
    for line in txt.splitlines():
        m = re.match(r"^([0-9a-f]+) <(.+)>:$", line)
        if m:
            cur = (m.group(2), int(m.group(1), 16), [])
            fns.append(cur)
            continue
        m = re.match(r"^\s*([0-9a-f]+):\s+(\S.*)$", line)
        if m and cur:
            mn, ops = parse(m.group(2))
            if not mn:
                continue
            tail = False
            if mn == "b":
                t = re.search(r"<([^>+]+)(\+0x[0-9a-f]+)?>", m.group(2))
                tail = bool(t) and t.group(1) != cur[0] and not t.group(2)
            cur[2].append((int(m.group(1), 16), mn, ops, tail))
    return fns


def dyn_side(run, irdiff, prog, exe, tag):
    cg = callgrind_instr(run, irdiff, prog, exe, tag)
    ir, fmin = parse_instr(cg)
    fns = objdump_fns(run, exe)
    # Load offset (static-pie runs at a base valgrind picks): the most common
    # difference between a function's lowest executed address and its symbol.
    # Offset 0 when the executed addresses are the binary's own (our static
    # musl builds): callgrind's `fn=` names are not the containing
    # functions on our code (harness/elftext.py), so they only break ties.
    addrs = {addr for _, _, insts in fns for addr, *_ in insts}
    hit = sum(n for a, n in ir.items() if a in addrs)
    if hit >= 0.99 * sum(ir.values()):
        off = 0
    else:
        starts = {n: s for n, s, _ in fns}
        diffs = collections.Counter((fmin[n] - starts[n]) for n in fmin if n in starts)
        off = diffs.most_common(1)[0][0] if diffs else 0
    tot = collections.Counter()
    per = collections.defaultdict(collections.Counter)
    for name, _, insts in fns:
        cls = classify_fn([(mn, ops, t) for _, mn, ops, t in insts])
        mk = move_kinds(_, insts, cls)
        for (addr, *_), c, k in zip(insts, cls, mk):
            n = ir.get(addr + off, 0)
            if not n:
                continue
            tot["insts"] += n
            per[name]["insts"] += n
            if c:
                tot[c] += n
                per[name][c] += n
            if k:
                tot["mov:" + k] += n
                per[name]["mov:" + k] += n
    covered = tot["insts"]
    return {"total": dict(tot), "functions": {k: dict(v) for k, v in per.items()}, "callgrind_total": sum(ir.values()),
            "covered": covered, "offset": off}


def cmd_dyn(a):
    run = load_mod("run", os.path.join(HELD, "run.py"))
    irdiff = load_mod("irdiff", os.path.join(HELD, "irdiff.py"))
    env = dict(kv.split("=", 1) for kv in a.env)
    irdiff.ensure_image()
    progs = [b for b in run.BENCHES if not a.only or b[0] in a.only.split(",")]
    res = {"kind": "dyn", "env": a.env, "programs": {}}
    for prog, binary, update in progs:
        prepare_src(run, prog, update)
        sides = {}
        for side in ("llvm", "pliron"):
            exe = irdiff.build(prog, binary, side, a.backend, env, a.skip_build, a.tag)
            sides[side] = dyn_side(run, irdiff, prog, exe, side if side == "llvm" or not a.tag else f"{side}-{a.tag}")
        res["programs"][prog] = sides
        print_prog(prog, sides, a.top)
        sys.stdout.flush()
    print_summary(res)
    print_kind_summary(res)
    if a.save:
        json.dump(res, open(a.save, "w"))
        print(f"saved {a.save}")


def norm(run_irdiff, names):
    return names


def canon_fn(n):
    return re.sub(r"\.llvm\.\d+$|17h[0-9a-f]{16}E$|::h[0-9a-f]{16}$", "", n)


def classify_gap(lrows, prows):
    """Split excess overhead into both-sides vs inline_only (LLVM has ~no body)."""
    both = inline_only = 0
    for k in set(lrows) | set(prows):
        d = overhead(prows[k]) - overhead(lrows[k])
        if d <= 0:
            continue
        lo = overhead(lrows[k])
        if lo * 20 < overhead(prows[k]) and lo < 1000:
            inline_only += d
        else:
            both += d
    return both, inline_only


def print_prog(prog, sides, top):
    L, P = sides["llvm"]["total"], sides["pliron"]["total"]
    print(f"\n{prog}: insts llvm {L.get('insts', 0)} pliron {P.get('insts', 0)} "
          f"(coverage {sides['pliron']['covered'] / max(1, sides['pliron']['callgrind_total']):.3f})")
    for c in CLASSES + ["frame"]:
        l, p = L.get(c, 0), P.get(c, 0)
        print(f"  {c:<8} {l:>12} {p:>12}  x{p / l if l else float('inf'):.2f}  {p - l:+d}")
    lo, po = overhead(L), overhead(P)
    print(f"  {'overhead':<8} {lo:>12} {po:>12}  x{po / lo if lo else float('inf'):.2f}  {po - lo:+d}")
    if any(k.startswith("mov:") for k in P):
        print("  moves by kind (llvm, pliron, excess)")
        for k in MOVE_KINDS:
            l, p = L.get("mov:" + k, 0), P.get("mov:" + k, 0)
            print(f"    {k:<8} {l:>12} {p:>12}  {p - l:+d}")
    lrows = collections.defaultdict(collections.Counter)
    for n, row in sides["llvm"]["functions"].items():
        lrows[canon_fn(n)].update(row)
    prows = collections.defaultdict(collections.Counter)
    for n, row in sides["pliron"]["functions"].items():
        prows[canon_fn(n)].update(row)
    both, inl = classify_gap(lrows, prows)
    po_both = max(0, po - inl)
    print(f"  {'gap':<8} both {both:+d}  inline_only {inl:+d}  "
          f"both-only overhead x{po_both / lo if lo else float('inf'):.3f}")
    keys = sorted(set(lrows) | set(prows), key=lambda k: overhead(lrows[k]) - overhead(prows[k]))
    print(f"  top {top} functions by excess overhead (pliron - llvm; classes pliron/llvm)")
    for k in keys[:top]:
        d = overhead(prows[k]) - overhead(lrows[k])
        if d <= 0:
            break
        lo_k = overhead(lrows[k])
        tag = "inline_only" if lo_k * 20 < overhead(prows[k]) and lo_k < 1000 else "both"
        det = " ".join(f"{c} {prows[k].get(c, 0)}/{lrows[k].get(c, 0)}" for c in CLASSES)
        print(f"  {d:>+11}  [{tag}]  {det}  {k[:90]}")


def print_summary(res):
    print("\nsummary: overhead (st+ld+mov+csr) executed, pliron / llvm")
    print(f"  {'program':<20}" + "".join(f"{c:>14}" for c in CLASSES) + f"{'overhead':>14}{'all insts':>12}")
    for prog, s in res["programs"].items():
        L, P = s["llvm"]["total"], s["pliron"]["total"]
        r = lambda c: f"{P.get(c, 0) / L[c]:.2f}" if L.get(c) else "-"
        lo, po = overhead(L), overhead(P)
        print(f"  {prog:<20}" + "".join(f"{r(c):>14}" for c in CLASSES)
              + f"{po / lo if lo else 0:>14.3f}{P.get('insts', 0) / max(1, L.get('insts', 1)):>12.3f}")


def print_kind_summary(res):
    print("\nsummary: excess register moves by kind (pliron - llvm)")
    print(f"  {'program':<20}" + "".join(f"{k:>11}" for k in MOVE_KINDS) + f"{'mov total':>11}")
    for prog, s in res["programs"].items():
        L, P = s["llvm"]["total"], s["pliron"]["total"]
        print(f"  {prog:<20}" + "".join(f"{P.get('mov:' + k, 0) - L.get('mov:' + k, 0):>+11d}" for k in MOVE_KINDS)
              + f"{P.get('mov', 0) - L.get('mov', 0):>+11d}")


def cmd_cmp(a):
    A, B = json.load(open(a.a)), json.load(open(a.b))
    if A["kind"] != B["kind"]:
        sys.exit("rastat: cmp needs two runs of one kind")
    if A["kind"] == "static":
        ta, tb = A["total"], B["total"]
        print(f"A: {' '.join(A['env']) or '(defaults)'}   B: {' '.join(B['env']) or '(defaults)'}  ({A['corpus']})")
        for c in CLASSES + ["slot", "frame", "insts", "winsts"]:
            x, y = ta.get(c, 0), tb.get(c, 0)
            print(f"  {c:<8} {x:>14} {y:>14}  {100 * (y - x) / x if x else 0:+7.2f}%")
        x, y = overhead(ta), overhead(tb)
        print(f"  {'overhead':<8} {x:>14} {y:>14}  {100 * (y - x) / x if x else 0:+7.2f}%")
        fa, fb = A["functions"], B["functions"]
        ks = sorted(set(fa) & set(fb), key=lambda k: overhead(fb[k]) - overhead(fa[k]))
        print(f"\n  most improved")
        for k in ks[:a.top]:
            print(f"  {overhead(fb[k]) - overhead(fa[k]):>+10}  {k[:100]}")
        print(f"  most worsened")
        for k in reversed(ks[-a.top:]):
            print(f"  {overhead(fb[k]) - overhead(fa[k]):>+10}  {k[:100]}")
    else:
        print(f"A: {' '.join(A['env']) or '(defaults)'}   B: {' '.join(B['env']) or '(defaults)'}  (pliron side, executed)")
        for prog in A["programs"]:
            if prog not in B["programs"]:
                continue
            pa, pb = A["programs"][prog]["pliron"]["total"], B["programs"][prog]["pliron"]["total"]
            det = " ".join(f"{c} {pb.get(c, 0) - pa.get(c, 0):+d}" for c in CLASSES)
            print(f"  {prog:<20} overhead {overhead(pb) - overhead(pa):+10d}  insts {pb.get('insts', 0) - pa.get('insts', 0):+10d}  {det}")


SELFTEST = [
    ("stp x29, x30, [sp, #-16]!", None), ("mov x19, x0", "inblock"), ("add x2, x19, #1", None),
    ("mov x0, x2", "callarg"), ("bl 0x100 <f>", None), ("mov x20, x0", "callret"),
    ("cbz x20, 0x28 <g+0x28>", None), ("add x3, x20, x19", None), ("mov x4, x3", "edge"),
    ("b 0x2c <g+0x2c>", None), ("mov x4, x19", "edge"), ("add x5, x4, #2", None), ("mov x0, x5", "ret"),
    ("ldp x29, x30, [sp], #16", None), ("ret", None), ("mov x1, x2", "ret"), ("mov x0, x3", "ret"),
    ("b 0x200 <h>", None),
]


def cmd_selftest():
    insts = []
    for k, (t, _) in enumerate(SELFTEST):
        mn, ops = parse(t)
        insts.append((4 * k, mn, ops, mn == "b" and "+0x" not in t))
    cls = classify_fn([(mn, ops, t) for _, mn, ops, t in insts])
    got = move_kinds(0, insts, cls)
    bad = [(t, w, g) for (t, w), g in zip(SELFTEST, got) if w != g]
    for t, w, g in bad:
        print(f"selftest: {t!r}: want {w}, got {g}")
    print("selftest: ok" if not bad else f"selftest: {len(bad)} wrong")
    return 1 if bad else 0


def main():
    ap = argparse.ArgumentParser(usage=__doc__)
    ap.add_argument("cmd", choices=["dyn", "static", "cmp", "selftest"])
    ap.add_argument("a", nargs="?")
    ap.add_argument("b", nargs="?")
    ap.add_argument("--only", default="")
    ap.add_argument("--env", action="append", default=[])
    ap.add_argument("--tag", default="")
    ap.add_argument("--skip-build", action="store_true")
    ap.add_argument("--top", type=int, default=12)
    ap.add_argument("--save", default="")
    ap.add_argument("--corpus", default="std")
    ap.add_argument("--every", type=int, default=1)
    ap.add_argument("--clif-check", default="")
    ap.add_argument("--backend", default="")
    a = ap.parse_args()
    if a.cmd == "dyn":
        run = load_mod("run", os.path.join(HELD, "run.py"))
        a.backend = a.backend or os.path.join(ROOT, "target", "release", run.DYLIB)
        return cmd_dyn(a)
    if a.cmd == "static":
        return cmd_static(a)
    if a.cmd == "selftest":
        return cmd_selftest()
    return cmd_cmp(a)


if __name__ == "__main__":
    sys.exit(main())
