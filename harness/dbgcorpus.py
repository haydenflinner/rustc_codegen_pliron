#!/usr/bin/env -S python3 -I
# r[verify native.dbg.vars]
# r[verify native.dbg.types]
# r[verify verify.dbg.vs-stock]
"""lldb on optimized and unoptimized code: stock rustc vs pliron, variable by variable.

  harness/dbgcorpus.py [--opt 0,2] [--only a,b] [--lldb PATH] [--be DYLIB]
                       [--target TRIPLE] [-v]

Each program in harness/dbgcorpus/*.rs is built with `-g -Copt-level=N` by
stock rustc and by pliron, then run under lldb (harness/dbgcorpus/drive.py)
with a breakpoint on every `// BREAK` line and rust-lldb's formatters
loaded; each stop (up to 3 per line)
records the backtrace (function + file:line, 6 frames) and `frame
variable`. Stops are paired by (line, hit). Per program and in total:

  stops   breakpoint stops stock reached / pliron reached too
  bt      paired stops whose backtraces are identical
  vars    variables stock shows a value for (not `<variable not available>`,
          nor one lldb failed to read: `BROKEN`)
  match   ... that pliron shows with the same value (addresses masked)
  wrong   ... that pliron shows with a different value
  extra   values pliron shows that stock does not

The score to watch is match/vars. rustc's own tests/debuginfo suite is the
other half of the picture (harness/dbgsuite.py).
"""
import argparse, json, os, re, shutil, subprocess, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC = os.path.join(ROOT, "harness", "dbgcorpus")
OUT = os.path.join(ROOT, "target", "dbgcorpus")
TOOLCHAIN = "nightly-2026-10-22"
XCODE_LLDB = "/Applications/Xcode.app/Contents/Developer/usr/bin/lldb"
UNAVAIL = ("<variable not available>", "<unavailable>")


def build(a, prog, side, opt, d):
    exe = os.path.join(d, prog)
    cmd = ["rustup", "run", TOOLCHAIN, "rustc", "-g", f"-Copt-level={opt}", "--edition", "2021",
           os.path.join(SRC, prog + ".rs"), "-o", exe]
    if side == "pliron":
        cmd.insert(4, f"-Zcodegen-backend={a.be}")
    if a.target:
        cmd += ["--target", a.target]
    env = dict(os.environ, RUSTC_ICE=os.path.join(d, "ices"))
    r = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, env=env, text=True)
    open(os.path.join(d, prog + ".build.log"), "w").write(r.stdout)
    return exe if r.returncode == 0 else None


def run(a, prog, exe, d):
    src = os.path.join(SRC, prog + ".rs")
    lines = [str(i) for i, l in enumerate(open(src), 1) if "// BREAK" in l]
    js = os.path.join(d, prog + ".json")
    if os.path.exists(js):
        os.remove(js)
    env = dict(os.environ, DBGC_EXE=exe, DBGC_SRC=prog + ".rs", DBGC_LINES=",".join(lines), DBGC_OUT=js,
               DBGC_ETC=a.etc)
    subprocess.run([a.lldb, "--batch", "-o", f"command script import {os.path.join(SRC, 'drive.py')}"],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env, timeout=120)
    try:
        return json.load(open(js))["stops"]
    except Exception:
        return None


def norm(v):
    v = re.sub(r"0x[0-9a-fA-F]+", "0x_", v)
    return re.sub(r"\s+", " ", v).strip()


# lldb's renderings of a value it could not actually read (stock has its
# share: entry values with no call-site info, dangling slice pointers).
BROKEN = ("<parent is NULL>", "read memory from", "memory read failed", "could not evaluate",
          "<no location", "<unavailable>")


def avail(v):
    return v is not None and not any(v.startswith(u) for u in UNAVAIL) and not any(b in v for b in BROKEN)


def compare(st, pl):
    c = dict(stops=0, reached=0, bt=0, vars=0, match=0, wrong=0, extra=0, det=[])
    pk = {(s["line"], s["hit"]): s for s in pl or []}
    for s in st or []:
        c["stops"] += 1
        p = pk.get((s["line"], s["hit"]))
        if p is None:
            c["vars"] += sum(avail(v) for v in s["vars"].values())
            c["det"].append(f"L{s['line']}#{s['hit']}: pliron never stopped here")
            continue
        c["reached"] += 1
        if s["frames"] == p["frames"]:
            c["bt"] += 1
        else:
            c["det"].append(f"L{s['line']}#{s['hit']}: bt stock {s['frames']} pliron {p['frames']}")
        for n, v in s["vars"].items():
            if not avail(v):
                continue
            c["vars"] += 1
            pv = p["vars"].get(n)
            if avail(pv) and norm(pv) == norm(v):
                c["match"] += 1
            elif avail(pv):
                c["wrong"] += 1
                c["det"].append(f"L{s['line']}#{s['hit']}: {n}: stock {norm(v)!r} pliron {norm(pv)!r}")
            else:
                c["det"].append(f"L{s['line']}#{s['hit']}: {n}: stock {norm(v)!r} pliron {pv!r}")
        for n, pv in p["vars"].items():
            if avail(pv) and not avail(s["vars"].get(n)):
                c["extra"] += 1
    return c


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--opt", default="0,2")
    ap.add_argument("--only", default="")
    ap.add_argument("--lldb", default=XCODE_LLDB if os.path.exists(XCODE_LLDB) else (shutil.which("lldb") or "lldb"))
    ap.add_argument("--be", default=os.path.join(ROOT, "target", "debug", "librustc_codegen_pliron.dylib"))
    ap.add_argument("--target", default="", help="e.g. x86_64-apple-darwin (runs under Rosetta; "
                    "the dylib must be built with cranelift-codegen's `x86` feature)")
    ap.add_argument("-v", action="store_true", help="print every difference")
    a = ap.parse_args()
    sysroot = subprocess.run(["rustup", "run", TOOLCHAIN, "rustc", "--print", "sysroot"],
                             stdout=subprocess.PIPE, text=True).stdout.strip()
    a.etc = os.path.join(sysroot, "lib", "rustlib", "etc")
    progs = sorted(f[:-3] for f in os.listdir(SRC) if f.endswith(".rs"))
    if a.only:
        progs = [p for p in progs if p in a.only.split(",")]
    keys = ["stops", "reached", "bt", "vars", "match", "wrong", "extra"]
    summary = {}
    for opt in [int(o) for o in a.opt.split(",")]:
        tot = dict.fromkeys(keys, 0)
        print(f"== O{opt}        " + " ".join(f"{k:>7}" for k in keys))
        for prog in progs:
            res = {}
            for side in ("stock", "pliron"):
                d = os.path.join(OUT, a.target or "host", f"O{opt}", side)
                os.makedirs(d, exist_ok=True)
                exe = build(a, prog, side, opt, d)
                res[side] = run(a, prog, exe, d) if exe else None
            if res["stock"] is None:
                print(f"{prog:14} stock build/run failed")
                continue
            c = compare(res["stock"], res["pliron"])
            if res["pliron"] is None:
                c["det"].insert(0, "pliron build/run failed")
            for k in keys:
                tot[k] += c[k]
            print(f"{prog:14} " + " ".join(f"{c[k]:>7}" for k in keys))
            if a.v:
                for x in c["det"]:
                    print("    " + x)
        print(f"{'total':14} " + " ".join(f"{tot[k]:>7}" for k in keys))
        summary[f"O{opt}"] = tot
    json.dump(summary, open(os.path.join(OUT, a.target or "host", "summary.json"), "w"), indent=1)


if __name__ == "__main__":
    main()
