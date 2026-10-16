"""Generate MSVC-free link inputs for `x86_64-pc-windows-msvc`.

Builds import libraries straight from the system DLLs in
C:\Windows\System32 (no Visual Studio / Windows SDK required), so a plain
`rust-lld` link resolves kernel32/ntdll/CRT imports.

Usage:
    uv run --no-project tools/mkwinlibs/mkwinlibs.py [OUTDIR]

Then compile with:
    PLIRON_CRTFREE=1 rustc -O foo.rs \
        -Zcodegen-backend=.../rustc_codegen_pliron.dll \
        -Clinker=rust-lld -Lnative=<OUTDIR>

`PLIRON_CRTFREE` makes the backend emit the static-CRT pieces
(mainCRTStartup, _tls_used/_tls_start/_tls_end/_tls_index, _fltused, the
type_info vftable, GS-cookie shims) into symbols.o, plus .drectve
/DEFAULTLIB: entries for ucrtbase + vcruntime140.
"""
import os, re, subprocess, sys

SR = subprocess.check_output(
    ["rustup", "run", "nightly-2026-10-06", "rustc", "--print", "sysroot"]
    if os.name == "nt" else
    ["rustc", "+nightly-2026-10-06", "--print", "sysroot"]
).decode().strip()
BINDIR = os.path.join(SR, "lib/rustlib/x86_64-pc-windows-msvc/bin")
RO = os.path.join(BINDIR, "llvm-readobj.exe")
LLD = os.path.join(BINDIR, "rust-lld.exe")
SYS32 = os.environ.get("SystemRoot", r"C:\Windows") + r"\System32"


def exports(dll):
    out = subprocess.check_output([RO, "--coff-exports", dll]).decode()
    names, fwd, cur = [], {}, None
    for line in out.splitlines():
        line = line.strip()
        if line.startswith("Name:"):
            cur = line[5:].strip()
            names.append(cur)
        elif line.startswith("ForwardedTo:"):
            fwd[cur] = line[12:].strip()
    return names, fwd


def gen(out, lib, dll, out_dll=None):
    names, fwd = exports(dll)
    deff = os.path.join(out, f"{lib}.def")
    with open(deff, "w") as f:
        f.write("LIBRARY\nEXPORTS\n")
        for n in names:
            # `.def` can't express ordinal-forwards like `=SHUNIMPL.#477`
            # or names containing '='/spaces — rustc objects never import them.
            if not re.match(r"^[A-Za-z_?@$][A-Za-z0-9_?@$#.]*$", n):
                continue
            f.write(f"  {n}={fwd[n]}\n" if n in fwd else f"  {n}\n")
    out_dll = out_dll or os.path.basename(dll)
    subprocess.check_call(
        [
            LLD, "-flavor", "link", "-nologo", "-machine:x64",
            f"-def:{deff}", f"-implib:{os.path.join(out, lib + '.lib')}",
            f"-out:{out_dll}", "-dll",
        ]
    )
    if os.path.exists(out_dll):
        os.remove(out_dll)
    print(f"{lib}.lib: {len(names)} exports")


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else "implibs"
    os.makedirs(out, exist_ok=True)
    for lib, dll in [
        ("kernel32", "kernel32.dll"),
        ("ntdll", "ntdll.dll"),
        ("userenv", "userenv.dll"),
        ("ws2_32", "ws2_32.dll"),
        ("dbghelp", "dbghelp.dll"),
        ("msvcrt", "msvcrt.dll"),
        ("ucrtbase", "ucrtbase.dll"),
        ("vcruntime140", "vcruntime140.dll"),
        ("ole32", "ole32.dll"),
        ("advapi32", "advapi32.dll"),
        ("shell32", "shell32.dll"),
        ("user32", "user32.dll"),
        ("bcrypt", "bcrypt.dll"),
        ("bcryptprimitives", "bcryptprimitives.dll"),
    ]:
        gen(out, lib, os.path.join(SYS32, dll))

    # synchronization.lib â€” the WinSDK lib mapping WaitOnAddress etc. onto
    # api-ms-win-core-synch-l1-2-0.dll (no real file; forwarders only).
    api = "API-MS-WIN-CORE-SYNCH-L1-2-0"
    sync = os.path.join(out, "synchronization.def")
    with open(sync, "w") as f:
        f.write(f"LIBRARY {api}.DLL\nEXPORTS\n")
        for n in [
            "WaitOnAddress", "WakeByAddressAll", "WakeByAddressSingle",
            "WakeAllOnAddress", "WakeByAddressSingleNoAlign",
        ]:
            f.write(f"  {n}={api}.{n}\n")
    subprocess.check_call(
        [
            LLD, "-flavor", "link", "-nologo", "-machine:x64",
            f"-def:{sync}", f"-implib:{os.path.join(out, 'synchronization.lib')}",
            f"-out:{api}.dll", "-dll",
        ]
    )
    if os.path.exists(f"{api}.dll"):
        os.remove(f"{api}.dll")
    print("synchronization.lib: 5 exports")
    print(f"\nDone. Link with: -Clinker=rust-lld -Lnative={out} and PLIRON_CRTFREE=1")


if __name__ == "__main__":
    main()
