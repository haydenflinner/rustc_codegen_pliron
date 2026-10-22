#!/usr/bin/env -S python3 -I
"""Run popular crates' own test suites under stock rustc and pliron; report
pliron-only failures.

  harness/crates.py [--tier 1] [--only a,b] [--debug] [--jobs N] [--timeout S]

Each pinned crate's published source is fetched once from static.crates.io
into target/crates/src/<name>-<ver>/ (a `[workspace]` stanza keeps cargo
from adopting it into ours), then `cargo test --release --no-fail-fast` runs
twice: stock, and with the pliron backend (RUSTFLAGS *and* RUSTDOCFLAGS, so
doc-tests go through pliron too). Each side shares one target dir across
crates; cargo doesn't track the backend dylib, so the pliron side is wiped
when the dylib changes. `--release` by default: the optimizer is where
miscompiles live (bevy_math's rect_union_pt, 2026-10-21).

Outputs target/crates/report.md (+ report.json, per-crate logs). For a
pliron-only failure, narrow it with harness/fnbisect.py using the printed
repro (touch the crate's src/lib.rs so cargo rebuilds just that crate), then
PLIRON_DIFFCHECK=step PLIRON_DIFFCHECK_FN=<fn> to name the pass.
"""
import argparse, hashlib, json, os, re, shutil, subprocess, sys, tarfile, time, urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "target", "crates")
TOOLCHAIN = "nightly-2026-10-06"

# (name, version, extra cargo-test args, tier[, git]). Pinned 2026-10-21;
# bump deliberately so a new failure means a compiler change, not a crate
# one. `git = (url, tag)`: the published package lacks its test data or
# test-only path deps, so build from the tagged repo (`-p name` from its
# root; the repo's own workspace stays intact).
REGEX_GIT = "https://github.com/rust-lang/regex"
# git spec flag: the args select packages themselves — no `-p name`.
WS = "ws"
CRATES = [
    ("ryu", "1.0.23", [], 1),
    ("itoa", "1.0.18", [], 1),
    ("lexical-core", "1.0.6", [], 1),
    ("memchr", "2.8.3", [], 1),
    ("aho-corasick", "1.1.5", [], 1),
    ("regex-syntax", "0.8.11", [], 1),
    ("regex-automata", "0.4.18", [], 1, (REGEX_GIT, "regex-automata-0.4.18")),
    ("regex", "1.13.1", [], 1, (REGEX_GIT, "1.13.1")),
    ("hashbrown", "0.17.1", [], 1),
    ("indexmap", "2.14.2", [], 1),
    ("smallvec", "1.16.3", [], 1),
    ("bytes", "1.12.1", [], 1),
    ("num-traits", "0.2.19", [], 1),
    ("num-bigint", "0.5.1", [], 1),
    ("libm", "0.2.16", [], 1),
    ("sha2", "0.11.1", [], 1),
    ("blake3", "1.8.7", [], 1, ("https://github.com/BLAKE3-team/BLAKE3", "1.8.7")),
    ("crc32fast", "1.5.3", [], 1),
    ("miniz_oxide", "0.9.1", [], 1),
    ("png", "0.18.1", [], 1),
    # Tier 2: atomics, threads, async, heavier generics.
    ("crossbeam-utils", "0.8.23", [], 2),
    ("crossbeam-epoch", "0.9.21", [], 2),
    ("crossbeam-deque", "0.8.8", [], 2),
    ("crossbeam-queue", "0.3.14", [], 2),
    ("crossbeam-channel", "0.5.17", [], 2),
    ("parking_lot", "0.13.0", [], 2),
    ("rayon-core", "1.13.0", [], 2),
    ("rayon", "1.12.0", [], 2),
    # The published package's tests don't build (tokio-test pulls a second
    # tokio); the repo's workspace patches them to the path crate.
    ("tokio", "1.54.1", ["--features", "full,test-util"], 2,
     ("https://github.com/tokio-rs/tokio", "tokio-1.54.1")),
    ("serde_json", "1.0.154", [], 2),
    ("toml", "1.1.8+spec-1.1.0", [], 2),
    # Tier 3: whole applications, cloned from the local checkouts at a
    # pinned commit (never the working tree: it may hold uncommitted
    # edits). Test commands follow each project's CI; `ver` is the commit.
    ("uiua", "637789911eae", ["--lib"], 3,
     (os.path.expanduser("~/code/uiua"), "637789911eae71316cd818fa81b25213724a437d", WS)),
    ("nushell", "4c175fa99ad9", ["--workspace", "--all-targets"], 3,
     (os.path.expanduser("~/code/nushell"), "4c175fa99ad90fc3f60b20c5935137fd17a1e7b5", WS)),
    ("rapier", "a306004de244", [], 3,
     (os.path.expanduser("~/code/rapier"), "a306004de2445e0f955b8568c62724ef2313f712", WS)),
    ("prql", "28bbf13c0482", ["-p", "prqlc", "-p", "prqlc-parser"], 3,
     (os.path.expanduser("~/code/prql"), "28bbf13c0482bd1acfa18211a1808822077c21f5", WS)),
    ("rune", "f67a45eac99e", ["--all-targets", "--all-features"], 3,
     (os.path.expanduser("~/code/rune"), "f67a45eac99ed609facc2b11d75352e556ffa0a0", WS)),
    # makepad's own tests live in its vendored libs and draw/platform crates
    # (not the ttf-parser/rustybuzz forks: their test targets still name the
    # upstream crates and don't build under stock either).
    ("makepad", "e72677c88036", ["-p", "makepad-zune-jpeg", "-p", "makepad-zune-png",
                                 "-p", "makepad-miniz", "-p", "makepad-vector",
                                 "-p", "makepad-live-compiler", "-p", "makepad-shader-compiler",
                                 "-p", "makepad-draw", "-p", "makepad-script-test"], 3,
     (os.path.expanduser("~/code/makepad-upstream"), "e72677c8803646971526bfc0caf0998c743d1efb", WS)),
]
# Per-side timeouts for the applications (seconds); the rest use --timeout.
TIMEOUTS = {"nushell": 7200, "rune": 5400, "makepad": 5400}

TEST_LINE = re.compile(r"^test (.+?) (?:- should panic )?\.\.\. (ok|FAILED|ignored)$")
RESULT = re.compile(r"^test result: \w+\. (\d+) passed; (\d+) failed")
# A test binary killed by a signal prints no `FAILED` line for the test it
# was running (serde_json's SIGILL), only cargo's exit report.
CRASH = re.compile(r"process didn't exit successfully: .*\((signal: \d+[^)]*)\)")


def fetch_git(name, ver, url, tag, *_):
    d = os.path.join(OUT, "src", f"{name}-{ver}-git")
    if not os.path.exists(os.path.join(d, "Cargo.toml")):
        os.makedirs(os.path.dirname(d), exist_ok=True)
        if re.fullmatch(r"[0-9a-f]{40}", tag):
            # A commit (local checkout): clone, then detach at it.
            subprocess.run(["git", "clone", "-q", "--no-checkout", url, d], check=True)
            subprocess.run(["git", "-c", "advice.detachedHead=false", "-C", d, "checkout", "-q",
                            tag], check=True)
        else:
            subprocess.run(["git", "-c", "advice.detachedHead=false", "clone", "-q", "--depth",
                            "1", "--branch", tag, url, d], check=True)
        # A single-crate repo has no workspace table: without one, cargo
        # adopts it into ours (it sits under our target/).
        manifest = os.path.join(d, "Cargo.toml")
        if not re.search(r"^(\[workspace\]|workspace\.)", open(manifest).read(), re.M):
            with open(manifest, "a") as f:
                f.write("\n[workspace]\n")
    return d


def fetch(name, ver):
    d = os.path.join(OUT, "src", f"{name}-{ver}")
    if os.path.exists(os.path.join(d, "Cargo.toml")):
        return d
    os.makedirs(os.path.dirname(d), exist_ok=True)
    url = f"https://static.crates.io/crates/{name}/{name}-{ver}.crate"
    req = urllib.request.Request(url, headers={"User-Agent": "pliron-crates-harness"})
    tgz = d + ".crate"
    with urllib.request.urlopen(req, timeout=60) as r, open(tgz, "wb") as f:
        f.write(r.read())
    with tarfile.open(tgz) as t:
        t.extractall(os.path.dirname(d), filter="data")
    os.remove(tgz)
    with open(os.path.join(d, "Cargo.toml"), "a") as f:
        f.write("\n[workspace]\n")
    return d


def dylib(profile):
    ext = "dylib" if sys.platform == "darwin" else "so"
    return os.path.join(ROOT, "target", profile, f"librustc_codegen_pliron.{ext}")


def stamp_ok(tdir, lib):
    """Wipe `tdir` when the backend dylib changed since its last use."""
    h = hashlib.sha256()
    with open(lib, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    want = h.hexdigest()
    sp = os.path.join(tdir, ".pliron-backend")
    if os.path.exists(sp) and open(sp).read() == want:
        return
    if os.path.exists(tdir):
        shutil.rmtree(tdir)
    os.makedirs(tdir, exist_ok=True)
    open(sp, "w").write(want)


def run_tests(src, side, args, lib, jobs, timeout, release):
    tdir = os.path.join(OUT, f"target-{side}")
    env = dict(os.environ)
    env["CARGO_INCREMENTAL"] = "0"
    # The backend has no LTO (rustc would skip upstream objects and expect
    # their bitcode), so a workspace profile's `lto = true` (prql) is turned
    # off -- for both sides, so they still build the same configuration.
    env["CARGO_PROFILE_RELEASE_LTO"] = "false"
    env.pop("RUSTFLAGS", None)
    env.pop("RUSTDOCFLAGS", None)
    if side == "pliron":
        flag = f"-Zcodegen-backend={lib}"
        env["RUSTFLAGS"] = flag
        env["RUSTDOCFLAGS"] = flag
        env["RUSTC_ICE"] = os.path.join(OUT, "ices")
        os.makedirs(env["RUSTC_ICE"], exist_ok=True)
    cmd = ["cargo", f"+{TOOLCHAIN}", "test", "--no-fail-fast", "--target-dir", tdir, "-j", str(jobs)]
    if release:
        cmd.append("--release")
    cmd += args
    t = time.time()
    try:
        p = subprocess.run(cmd, cwd=src, env=env, capture_output=True, text=True, timeout=timeout)
        log, rc = p.stdout + p.stderr, p.returncode
    except subprocess.TimeoutExpired as e:
        log = (e.stdout or b"").decode(errors="replace") + (e.stderr or b"").decode(errors="replace")
        rc = "timeout"
    res = {"rc": rc, "secs": round(time.time() - t, 1), "passed": 0, "failed": 0, "fails": [], "cmd": cmd}
    suite = ""
    for line in log.splitlines():
        if line.strip().startswith(("Running ", "Doc-tests ")):
            suite = line.strip().split(" (")[0]
        m = TEST_LINE.match(line)
        if m and m.group(2) == "FAILED":
            res["fails"].append(f"{suite} :: {m.group(1)}")
        m = CRASH.search(line)
        if m:
            res["fails"].append(f"{suite} :: <{m.group(1)}>")
        m = RESULT.match(line)
        if m:
            res["passed"] += int(m.group(1))
            res["failed"] += int(m.group(2))
    res["build_error"] = "could not compile" in log
    res["ice"] = any(s in log for s in ("internal compiler error", "pipeline broke", "error: the compiler unexpectedly panicked"))
    res["fails"] = sorted(set(res["fails"]))
    return res, log


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tier", type=int, default=1)
    ap.add_argument("--only", default="")
    ap.add_argument("--debug", action="store_true", help="test profile instead of --release")
    ap.add_argument("--jobs", type=int, default=6)
    ap.add_argument("--timeout", type=int, default=1800)
    ap.add_argument("--profile", default="release", help="backend dylib profile (release|debug)")
    a = ap.parse_args()
    only = set(filter(None, a.only.split(",")))
    lib = dylib(a.profile)
    if not os.path.exists(lib):
        sys.exit(f"no backend dylib at {lib}: cargo build --{a.profile}")
    os.makedirs(os.path.join(OUT, "logs"), exist_ok=True)
    stamp_ok(os.path.join(OUT, "target-pliron"), lib)
    rows = []
    for name, ver, args, tier, *git in CRATES:
        if tier > a.tier or (only and name not in only):
            continue
        try:
            if git:
                src = fetch_git(name, ver, *git[0])
                if WS not in git[0][2:]:
                    args = ["-p", name] + args
            else:
                src = fetch(name, ver)
        except Exception as e:
            print(f"{name}: fetch failed: {e}")
            continue
        row = {"crate": f"{name}-{ver}"}
        for side in ("stock", "pliron"):
            r, log = run_tests(src, side, args, lib, a.jobs, TIMEOUTS.get(name, a.timeout),
                               not a.debug)
            open(os.path.join(OUT, "logs", f"{name}.{side}.log"), "w").write(log)
            row[side] = r
        only_p = sorted(set(row["pliron"]["fails"]) - set(row["stock"]["fails"]))
        row["pliron_only"] = only_p
        s, p = row["stock"], row["pliron"]
        status = (
            "ICE" if p["ice"] else
            "BUILD" if p["build_error"] and not s["build_error"] else
            "TIMEOUT" if p["rc"] == "timeout" and s["rc"] != "timeout" else
            "FAIL" if only_p or p["passed"] < s["passed"] or p["failed"] > s["failed"] else
            "skip" if s["build_error"] else "ok"
        )
        row["status"] = status
        rows.append(row)
        print(f"{status:8} {row['crate']:24} stock {s['passed']}/{s['failed']} ({s['secs']}s)  "
              f"pliron {p['passed']}/{p['failed']} ({p['secs']}s)  pliron-only {len(only_p)}", flush=True)
        for f in only_p[:10]:
            print(f"           {f}")
    json.dump(rows, open(os.path.join(OUT, "report.json"), "w"), indent=1)
    with open(os.path.join(OUT, "report.md"), "w") as f:
        f.write("| status | crate | stock pass/fail | pliron pass/fail | pliron-only |\n|---|---|---|---|---|\n")
        for r in rows:
            f.write(f"| {r['status']} | {r['crate']} | {r['stock']['passed']}/{r['stock']['failed']} | "
                    f"{r['pliron']['passed']}/{r['pliron']['failed']} | {', '.join(r['pliron_only'][:5])} |\n")
        f.write("\nRepro (pliron side): cd target/crates/src/<crate> && "
                f"RUSTFLAGS=-Zcodegen-backend={lib} RUSTDOCFLAGS=-Zcodegen-backend={lib} "
                f"cargo +{TOOLCHAIN} test --release --target-dir {os.path.join(OUT, 'target-pliron')}\n")
    bad = [r for r in rows if r["status"] in ("ICE", "BUILD", "FAIL", "TIMEOUT")]
    print(f"\n{len(rows)} crates, {len(bad)} with pliron-only problems -> {os.path.join(OUT, 'report.md')}")
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
