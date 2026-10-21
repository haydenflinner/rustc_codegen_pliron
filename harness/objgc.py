#!/usr/bin/env -S python3 -I
"""Delete stale codegen-unit objects from cargo build units.

macOS dev builds keep `*.rcgu.o` next to each unit's library (debug info
stays in the objects, `split-debuginfo=unpacked`), and every incremental
session writes fresh CGU names without removing the old ones. They pile
up: on 2026-10-21 `target/debug/build` held 54 GB of `.rcgu.o`, 45 GB of
it older than the unit's current library. An object older than the
newest `.rlib`/`.rmeta`/`.dylib` in its `out/` dir (by `--slack` s) was
written by an earlier session and nothing current references it.

  harness/objgc.py [--dry-run] [--slack SECS] [BUILD_DIR ...]
Default BUILD_DIRs: target/*/build.
"""
import glob, os, sys

args = sys.argv[1:]
dry = "--dry-run" in args
slack = 120.0
if "--slack" in args:
    slack = float(args[args.index("--slack") + 1])
dirs = [a for a in args if not a.startswith("--") and not a.replace(".", "").isdigit()]
os.chdir(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
dirs = dirs or glob.glob("target/*/build")
freed = n = 0
for bd in dirs:
    for out in glob.glob(os.path.join(bd, "*", "*", "out")):
        try:
            ents = list(os.scandir(out))
        except OSError:
            continue
        libs = [e.stat().st_mtime for e in ents if e.name.endswith((".rlib", ".rmeta", ".dylib", ".so", ".a"))]
        if not libs:
            continue
        cut = max(libs) - slack
        for e in ents:
            if e.name.endswith(".rcgu.o"):
                st = e.stat()
                if st.st_mtime < cut:
                    freed += st.st_size
                    n += 1
                    if not dry:
                        os.unlink(e.path)
print(f"{'would free' if dry else 'freed'} {freed / 1e9:.1f} GB in {n} stale .rcgu.o")
