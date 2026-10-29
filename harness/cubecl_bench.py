#!/usr/bin/env -S python3 -I
"""Measure CubeCL's CPU runtime with LLVM vs pliron -> Cranelift (CUBECL_PLIRON_CLIF=1).

Usage (run under the machine's heavy lock; timings at load > ~8 are noise):
  lockf <heavy.lock> harness/cubecl_bench.py [--cubecl DIR] [--reps 5]
        [--cubek-bench 'NAME=EXE|ENV=V;ENV=V' ...] [--cubek-samples 5]

Per repetition it runs every workload once per path, alternating which path
goes first (interleaved), and keeps the median over repetitions:

  throughput  the cubecl `throughput` examples (compute_direct f32/f16/bf16,
              memory copy/read/write), parsed from their tables; plus any
              cubek benchmark binaries given with --cubek-bench (median kernel
              time of every `---- strategy / problem ----` row);
  JIT time    one `[cubecl-pliron] path=.. kernel=.. obj_bytes=.. codegen_us=..
              total_us=..` line per compiled kernel (CUBECL_PLIRON_LOG=<file>):
              codegen = scan/IR-opt/lowering or LLVM passes+codegen, total adds
              the OrcJit load. `cold` is the first kernel of a fresh process,
              `warm` every later one. CubeCL keeps no cross-process kernel
              cache unless `[compilation] cache = true` in cubecl.toml; this
              script runs from a directory with no cubecl.toml (and a
              throughput one with `disable_cache = true`);
  RSS         `/usr/bin/time -l` peak resident set of a compile-dominated run
              (the whole cubecl-cpu f32 test subset on one thread) and of each
              example;
  size        object bytes per kernel from the same log lines.

Writes target/parity/cubecl-measure.json for harness/cubecl_card.py.
"""
import argparse, glob, json, math, os, re, statistics, subprocess, sys, tempfile, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "target/parity")
EXAMPLES = ["compute_direct", "memory", "memory_read", "memory_write"]
LOG = re.compile(r"\[cubecl-pliron\] path=(\w+) kernel=(\S+) obj_bytes=(\d+) codegen_us=(\d+) total_us=(\d+)")
ROW = re.compile(r"^\s+(\S+)\s+(.+?)\s+([\d.]+)\s*(GOPS/s|Gbytes/s|GB/s|TOPS/s|MOPS/s)\s*$")
RSS = re.compile(r"(\d+)\s+maximum resident set size")


def run(cmd, cwd, clif, extra_env=None):
    """One process under /usr/bin/time -l; returns (stdout+stderr, kernels, rss_kb, seconds)."""
    fd, logf = tempfile.mkstemp(prefix="cubecl-pliron-", suffix=".log")
    os.close(fd)
    env = {**os.environ, "CUBECL_PLIRON_LOG": logf, "RUST_BACKTRACE": "0", **(extra_env or {})}
    env.pop("CUBECL_PLIRON_CLIF", None)
    if clif:
        env["CUBECL_PLIRON_CLIF"] = "1"
    t0 = time.time()
    p = subprocess.run(["/usr/bin/time", "-l"] + cmd, cwd=cwd, env=env, capture_output=True, text=True,
                       errors="replace")
    dt = time.time() - t0
    txt = open(logf).read()
    os.unlink(logf)
    path = "clif" if clif else "llvm"
    kernels = []
    for m in LOG.finditer(txt):
        if m.group(1) != path:
            sys.exit(f"{cmd[0]}: kernel compiled on path {m.group(1)}, expected {path}")
        kernels.append((m.group(2), int(m.group(3)), int(m.group(4)), int(m.group(5))))
    rss = RSS.search(p.stderr)
    rss_kb = int(rss.group(1)) // 1024 if rss else None
    return p.stdout + p.stderr, kernels, rss_kb, dt, p.returncode


def med(xs):
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else None


def main():
    ap = argparse.ArgumentParser(usage=__doc__)
    ap.add_argument("--cubecl", default="/Users/wow/code/cubecl")
    ap.add_argument("--reps", type=int, default=5)
    ap.add_argument("--cubek-bench", action="append", default=[], metavar="NAME=EXE")
    ap.add_argument("--cubek-samples", default="5")
    ap.add_argument("--only-cubek", action="store_true", help="skip the examples and the test-binary RSS run")
    ap.add_argument("--update", action="store_true", help="merge into an existing cubecl-measure.json")
    a = ap.parse_args()
    ex_dir = os.path.join(a.cubecl, "target/release/examples")
    work = tempfile.mkdtemp(prefix="cubecl-bench-")
    open(os.path.join(work, "cubecl.toml"), "w").write("[throughput]\ndisable_cache = true\n")
    tests = [f for f in glob.glob(os.path.join(a.cubecl, "target/release/deps/cubecl_cpu-*"))
             if "." not in os.path.basename(f)]
    tests = max(tests, key=os.path.getmtime) if tests else None

    workloads = [] if a.only_cubek else [(n, [os.path.join(ex_dir, n)], "example", None) for n in EXAMPLES]
    if tests and not a.only_cubek:
        workloads.append(("cubecl-cpu f32 tests (1 thread)", [tests, "tests::f32_ty", "--test-threads", "1"], "rss", None))
    for spec in a.cubek_bench:
        name, exe = spec.split("=", 1)
        exe, _, envs = exe.partition("|")  # EXE|CUBEK_BENCH_STRATEGIES=unit,CUBEK_BENCH_PROBLEMS=sum
        workloads.append((f"cubek {name}", [exe], "cubek", dict(kv.split("=", 1) for kv in envs.split(";") if kv)))

    load0 = os.getloadavg()[0]
    spd = {}      # name -> path -> [values]
    jit = {}      # kernel -> path -> [total_us]
    codegen = {}  # kernel -> path -> [codegen_us]
    cold = {}     # workload -> path -> [first-kernel total_us]
    size = {}     # kernel -> path -> bytes
    rss = {}      # workload -> path -> [kb]
    for rep in range(a.reps):
        order = ["llvm", "clif"] if rep % 2 == 0 else ["clif", "llvm"]
        for wname, cmd, kind, wenv in workloads:
            for path in order:
                extra = {"CUBEK_BENCH_SAMPLES": a.cubek_samples, **wenv} if kind == "cubek" else None
                cwd = work if kind != "rss" else os.path.join(a.cubecl, "crates/cubecl-cpu")
                out, kernels, kb, dt, rc = run(cmd, cwd, path == "clif", extra)
                print(f"rep {rep} {path:4} {wname}: rc {rc}, {len(kernels)} kernels, {dt:.1f} s, rss {kb} KiB, "
                      f"load {os.getloadavg()[0]:.1f}", flush=True)
                if rc != 0 and kind != "rss":
                    print("  FAILED:", out[-400:].replace("\n", " | "))
                    continue
                rss.setdefault(wname, {}).setdefault(path, []).append(kb)
                for i, (k, ob, cg, tot) in enumerate(kernels):
                    jit.setdefault(k, {}).setdefault(path, []).append(tot)
                    codegen.setdefault(k, {}).setdefault(path, []).append(cg)
                    size.setdefault(k, {})[path] = ob
                    if i == 0:
                        cold.setdefault(wname, {}).setdefault(path, []).append(tot)
                if kind == "example":
                    for line in out.splitlines():
                        m = ROW.match(line)
                        if m:
                            key = f"{wname}: {m.group(1)} {m.group(2).strip()}"
                            spd.setdefault(key, {}).setdefault(path, []).append((float(m.group(3)), m.group(4)))
                elif kind == "cubek":
                    cur = None
                    for line in out.splitlines():
                        if line.startswith("---- "):
                            cur = line.strip("- ").strip()
                        m = re.search(r"^\s*Median\s+([\d.]+)(ns|µs|us|ms|s)\b", line)
                        if cur and m:
                            t = float(m.group(1)) * {"ns": 1e-9, "µs": 1e-6, "us": 1e-6, "ms": 1e-3, "s": 1.0}[m.group(2)]
                            spd.setdefault(f"{wname}: {cur}", {}).setdefault(path, []).append((1.0 / t, "1/s"))
    res = {"host": subprocess.run(["sysctl", "-n", "machdep.cpu.brand_string"], capture_output=True,
                                  text=True).stdout.strip() or os.uname().machine,
           "load": {"start": round(load0, 1), "end": round(os.getloadavg()[0], 1)},
           "reps": a.reps, "spd": {}, "cmp": {}, "cmp_codegen": {}, "cold": {}, "mem": {}, "size": {}}
    for k, v in spd.items():
        if "llvm" in v and "clif" in v:
            res["spd"][k] = {"llvm": med([x for x, _ in v["llvm"]]), "clif": med([x for x, _ in v["clif"]]),
                             "unit": v["llvm"][0][1]}
    for k, v in jit.items():
        if "llvm" in v and "clif" in v:
            res["cmp"][k] = {"llvm_us": med(v["llvm"]), "clif_us": med(v["clif"])}
            res["cmp_codegen"][k] = {"llvm_us": med(codegen[k]["llvm"]), "clif_us": med(codegen[k]["clif"])}
            res["size"][k] = {"llvm_bytes": size[k].get("llvm"), "clif_bytes": size[k].get("clif")}
    for k, v in cold.items():
        if "llvm" in v and "clif" in v:
            res["cold"][k] = {"llvm_us": med(v["llvm"]), "clif_us": med(v["clif"])}
    for k, v in rss.items():
        if "llvm" in v and "clif" in v:
            res["mem"][k] = {"llvm_kb": med(v["llvm"]), "clif_kb": med(v["clif"])}
    os.makedirs(OUT, exist_ok=True)
    path = os.path.join(OUT, "cubecl-measure.json")
    if a.update and os.path.exists(path):
        old = json.load(open(path))
        for k in ("spd", "cmp", "cmp_codegen", "cold", "mem", "size"):
            old[k].update(res[k])
        old["load"]["end"] = res["load"]["end"]
        res = old
    json.dump(res, open(path, "w"), indent=1)
    print(f"wrote {os.path.join(OUT, 'cubecl-measure.json')}: {len(res['spd'])} throughput rows, "
          f"{len(res['cmp'])} kernels, load {res['load']['start']} -> {res['load']['end']}")


if __name__ == "__main__":
    main()
