#!/usr/bin/env python3
"""Poor man's profiler for Windows: samples the main thread's RIP of a
child process with SuspendThread + GetThreadContext. No admin needed for
a process we spawned ourselves.

  python harness/ripsample.py -- <exe> [args...] [-benchtime Ns]

Prints the hottest instruction addresses (RVAs); pair with llvm-objdump
output to attribute. Intended for the bench kernels' steady state.
"""
import ctypes, struct, subprocess, sys, threading, time, collections

k32 = ctypes.windll.kernel32
THREAD_SUSPEND_RESUME = 0x0002
THREAD_GET_CONTEXT = 0x0008
THREAD_QUERY_INFORMATION = 0x0040
CONTEXT_AMD64, CONTEXT_CONTROL, CONTEXT_INTEGER = 0x100000, 0x2, 0x1


class CONTEXT(ctypes.Structure):
    _fields_ = [
        ("P1Home", ctypes.c_uint64), ("P2Home", ctypes.c_uint64),
        ("P3Home", ctypes.c_uint64), ("P4Home", ctypes.c_uint64),
        ("P5Home", ctypes.c_uint64), ("P6Home", ctypes.c_uint64),
        ("ContextFlags", ctypes.c_uint32), ("MxCsr", ctypes.c_uint32),
        ("SegCs", ctypes.c_uint16), ("SegDs", ctypes.c_uint16),
        ("SegEs", ctypes.c_uint16), ("SegFs", ctypes.c_uint16),
        ("SegGs", ctypes.c_uint16), ("SegSs", ctypes.c_uint16),
        ("EFlags", ctypes.c_uint32),
        ("Dr0", ctypes.c_uint64), ("Dr1", ctypes.c_uint64),
        ("Dr2", ctypes.c_uint64), ("Dr3", ctypes.c_uint64),
        ("Dr6", ctypes.c_uint64), ("Dr7", ctypes.c_uint64),
        ("Rax", ctypes.c_uint64), ("Rcx", ctypes.c_uint64),
        ("Rdx", ctypes.c_uint64), ("Rbx", ctypes.c_uint64),
        ("Rsp", ctypes.c_uint64), ("Rbp", ctypes.c_uint64),
        ("Rsi", ctypes.c_uint64), ("Rdi", ctypes.c_uint64),
        ("R8", ctypes.c_uint64), ("R9", ctypes.c_uint64),
        ("R10", ctypes.c_uint64), ("R11", ctypes.c_uint64),
        ("R12", ctypes.c_uint64), ("R13", ctypes.c_uint64),
        ("R14", ctypes.c_uint64), ("R15", ctypes.c_uint64),
        ("Rip", ctypes.c_uint64),
        ("FltSave", ctypes.c_byte * 512),
        ("VectorRegister", ctypes.c_byte * (26 * 16)),
        ("VectorControl", ctypes.c_uint64),
        ("DebugControl", ctypes.c_uint64),
        ("LastBranchToRip", ctypes.c_uint64),
        ("LastBranchFromRip", ctypes.c_uint64),
        ("LastExceptionToRip", ctypes.c_uint64),
        ("LastExceptionFromRip", ctypes.c_uint64),
    ]


def main():
    args = sys.argv[1:]
    if args and args[0] == "--":
        args = args[1:]
    secs = float(next((a.split("=", 1)[1] for a in args if a.startswith("--secs=")), 6.0))
    warmup = float(next((a.split("=", 1)[1] for a in args if a.startswith("--warmup=")), 2.0))
    args = [a for a in args if not a.startswith("--secs=") and not a.startswith("--warmup=")]
    if args and args[0] == "--":
        args = args[1:]

    p = subprocess.Popen(args)
    time.sleep(warmup)

    # Exe image base (first module) for RVA attribution.
    base = 0
    hmods = (ctypes.c_void_p * 8)()
    need = ctypes.c_uint32()
    psapi = ctypes.windll.psapi
    hproc = k32.OpenProcess(0x400 | 0x10, False, p.pid)  # QUERY_INFORMATION|VM_READ
    if hproc and psapi.EnumProcessModules(hproc, hmods, ctypes.sizeof(hmods), ctypes.byref(need)):
        base = hmods[0] or 0
    if hproc:
        k32.CloseHandle(hproc)

    # All threads of the child (the benchmark usually runs on a worker).
    THREADENTRY32_SIZE = 28
    snap = k32.CreateToolhelp32Snapshot(0x4, 0)  # TH32CS_SNAPTHREAD
    te = ctypes.create_string_buffer(THREADENTRY32_SIZE)
    struct.pack_into("<I", te, 0, THREADENTRY32_SIZE)
    tids = []
    ok = k32.Thread32First(snap, ctypes.byref(te))
    while ok:
        (dwSize, cntUsage, tid, owner, basePri, deltaPri, flags) = struct.unpack("<IIIIllI", te.raw)
        if owner == p.pid:
            tids.append(tid)
        ok = k32.Thread32Next(snap, ctypes.byref(te))
    k32.CloseHandle(snap)
    ths = [k32.OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION,
                          False, t) for t in tids]
    ths = [t for t in ths if t]
    if not ths:
        sys.exit("OpenThread failed for all threads")

    counts = collections.Counter()
    deadline = time.time() + secs
    n = 0
    while time.time() < deadline:
        for th in ths:
            if k32.SuspendThread(th) == 0xFFFFFFFF:
                continue
            ctx = CONTEXT()
            ctx.ContextFlags = CONTEXT_AMD64 | CONTEXT_CONTROL | CONTEXT_INTEGER
            if k32.GetThreadContext(th, ctypes.byref(ctx)):
                counts[ctx.Rip] += 1
                n += 1
            k32.ResumeThread(th)
        time.sleep(0.0004)
    for th in ths:
        k32.CloseHandle(th)
    try:
        p.terminate()
    except Exception:
        pass
    print(f"{n} samples, base={base:#x}")
    for rip, c in counts.most_common(40):
        tag = f"rva={rip - base:#x}" if base and 0 <= rip - base < 0x1000000 else "sys"
        print(f"{c:6d}  {rip:#x}  {tag}")


if __name__ == "__main__":
    main()
