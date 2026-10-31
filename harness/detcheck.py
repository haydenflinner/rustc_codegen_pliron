#!/usr/bin/env -S python3 -I
# r[verify model.det]
# r[verify clif.lower.phases]
# r[verify wasm.emit.par]
# r[verify par.pool.throttle]
# r[verify par.pool.lower-threads]
# r[verify par.facts.prefix]
# r[verify par.det.schedule]
# r[verify par.det.strategy]
# r[verify par.det.throttle]
# r[verify par.det.order]
"""Output invariance across execution strategies (docs/determinism.md).

  harness/detcheck.py [--inputs regex-syntax,syn,smoke] [--config NAME=ENV ...]
                      [--variants again,par1,inflight1,serial,memfree,acap2] [--cgus 16]
                      [--self-test]

For each input and each configuration (a set of PLIRON_* flags), build the
objects once as the reference, then once per *variant*, a knob that must
only change how the work is scheduled, never what comes out, and require
byte-identical objects. Variants:

  again      the same command again (plain run-to-run determinism)
  par1       PLIRON_PAR=1: serial per-function CLIF lowering inside a CGU
  inflight1  PLIRON_CGU_INFLIGHT=1: one CGU in the pool at a time
  serial     PLIRON_CGUPAR=0: no CGU pool at all (rustc's own serial order)
  memfree    PLIRON_LOWERFREE=1 PLIRON_JEARENA=1: free each CGU's IR after
             lowering, purge freed pages at once (memory knobs only)
  acap2      PLIRON_CGU_ACAP=2: at most two CGUs past the start of stage A

A mismatch names the differing objects and, per object, the functions whose
disassembly differs. A known exception is reported as `expected`, not a
failure: `serial` under PLIRON_XFACTS_ALL, whose two-phase facts exist only in
the pool (the serial path compiles as if the flag were off).

Inputs: `regex-syntax`/`syn`/`kernels-wasm`/`regex-wasm` (harness/ctime.py's
commands) and `smoke` (each harness smoke program). Default configs: `default` (no flags) and, with
--config, any NAME=ENV ("all=PLIRON_XFACTS_ALL=1 PLIRON_SM_STARTS=1 ...").
Exit status 1 on any unexpected mismatch. Builds into target/detcheck/.
"""
import argparse, hashlib, importlib.util, os, re, shutil, subprocess, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "target", "detcheck")
OD = shutil.which("llvm-objdump") or "/opt/homebrew/opt/llvm/bin/llvm-objdump"

VARIANTS = {
    "again": {},
    "par1": {"PLIRON_PAR": "1"},
    "inflight1": {"PLIRON_CGU_INFLIGHT": "1"},
    "serial": {"PLIRON_CGUPAR": "0"},
    # Memory knobs (R3): they only free earlier or purge, never change bytes.
    "memfree": {"PLIRON_LOWERFREE": "1", "PLIRON_JEARENA": "1"},
    "acap2": {"PLIRON_CGU_ACAP": "2"},
}


def expected_diff(variant, env):
    """Known, documented strategy-dependent outputs (not failures)."""
    if variant == "serial" and env.get("PLIRON_XFACTS_ALL") == "1":
        return "XFACTS_ALL's two-phase facts run only in the CGU pool"
    return None


def load(name):
    spec = importlib.util.spec_from_file_location(name, os.path.join(ROOT, "harness", f"{name}.py"))
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


def inputs(names):
    out = {}
    want = set(names)
    ct_names = ("regex-syntax", "syn", "kernels-wasm", "regex-wasm")
    if want & set(ct_names):
        ct = load("ctime").inputs()
        for k in ct_names:
            if k in want and k in ct:
                out[k] = ct[k]
    if "smoke" in want:
        run = load("run")
        for name, src, flags in run.smoke_cases():
            if name.endswith("@O"):
                out["smoke:" + name[:-2]] = [os.path.join(ROOT, src), "--crate-type", "lib"] + [
                    f for f in flags if f != "-O"
                ]
    return out


def build(args, env, d, backend, cgus):
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(d)
    cmd = ["rustc", "-O", "--emit=obj", f"-Ccodegen-units={cgus}", "--out-dir", d] + args + [
        f"-Zcodegen-backend={backend}"
    ]
    full = dict(os.environ)
    full.update(env)
    r = subprocess.run(cmd, env=full, capture_output=True, text=True)
    if r.returncode != 0:
        return None, r.stderr[-400:]
    objs = {}
    for f in sorted(os.listdir(d)):
        if f.endswith(".o"):
            objs[f] = open(os.path.join(d, f), "rb").read()
    return objs, ""


def fn_hashes(path):
    """Symbol -> hash of its disassembly (addresses stripped)."""
    # `-r`: relocations name the real targets; objdump's `<sym+off>` guesses
    # for unrelocated operands shift with unrelated code, so they go.
    txt = subprocess.run([OD, "-dr", "--no-show-raw-insn", "--no-leading-addr", path],
                         capture_output=True, text=True).stdout
    out, cur, buf = {}, None, []
    for line in txt.splitlines():
        m = re.match(r"^<(.+)>:$", line.strip())
        if m:
            if cur:
                out[cur] = hashlib.sha1("\n".join(buf).encode()).hexdigest()
            cur, buf = m.group(1), []
        elif cur:
            buf.append(re.sub(r"<[^>]*>|0x[0-9a-f]+|\b[0-9a-f]{8,}\b", "#", line))
    if cur:
        out[cur] = hashlib.sha1("\n".join(buf).encode()).hexdigest()
    return out


def explain(da, db, a, b):
    lines = []
    for f in sorted(set(a) | set(b)):
        if a.get(f) == b.get(f):
            continue
        if f not in a or f not in b:
            lines.append(f"    {f}: only in {'variant' if f not in a else 'reference'}")
            continue
        ha, hb = fn_hashes(os.path.join(da, f)), fn_hashes(os.path.join(db, f))
        changed = [s for s in sorted(set(ha) & set(hb)) if ha[s] != hb[s]]
        only_a, only_b = sorted(set(ha) - set(hb)), sorted(set(hb) - set(ha))
        lines.append(f"    {f}: {len(changed)} changed, {len(only_a)} only in reference, {len(only_b)} only in variant"
                     + "".join(f"\n      {s}" for s in (changed or only_a or only_b)[:8]))
    return "\n".join(lines)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--inputs", default="regex-syntax,syn,smoke")
    ap.add_argument("--config", action="append", default=[], help="NAME=ENV (space-separated K=V)")
    ap.add_argument("--variants", default=",".join(VARIANTS))
    ap.add_argument("--cgus", type=int, default=16)
    ap.add_argument("--backend", default=os.path.join(ROOT, "target/release/librustc_codegen_pliron.dylib"))
    ap.add_argument("--self-test", action="store_true",
                    help="check the checker: a variant known to change output must be caught")
    a = ap.parse_args()
    if a.self_test:
        # PLIRON_DEADFREE drops dead bodies: the objects must differ.
        VARIANTS["selftest"] = {"PLIRON_DEADFREE": "1"}
        a.variants = "selftest"
    configs = {"default": {}}
    for c in a.config:
        name, _, env = c.partition("=")
        configs[name] = dict(kv.split("=", 1) for kv in env.split())
    bad = 0
    for iname, iargs in inputs(a.inputs.split(",")).items():
        for cname, cenv in configs.items():
            tag = f"{iname} [{cname}]"
            dref = os.path.join(OUT, re.sub(r"\W", "_", f"{iname}-{cname}-ref"))
            ref, err = build(iargs, cenv, dref, a.backend, a.cgus)
            if ref is None:
                print(f"BUILD  {tag}: {err.strip().splitlines()[-1] if err.strip() else 'failed'}")
                bad += 1
                continue
            for v in a.variants.split(","):
                env = dict(cenv)
                env.update(VARIANTS[v])
                dv = os.path.join(OUT, re.sub(r"\W", "_", f"{iname}-{cname}-{v}"))
                got, err = build(iargs, env, dv, a.backend, a.cgus)
                if got is None:
                    print(f"BUILD  {tag} {v}: {err.strip().splitlines()[-1] if err.strip() else 'failed'}")
                    bad += 1
                elif got == ref:
                    print(f"ok     {tag} {v}")
                elif why := expected_diff(v, env):
                    print(f"expect {tag} {v}: differs ({why})")
                else:
                    print(f"DIFF   {tag} {v}:\n{explain(dref, dv, ref, got)}")
                    bad += 1
    if a.self_test:
        # Every comparison above must have been a DIFF.
        sys.exit(0 if bad else 1)
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
