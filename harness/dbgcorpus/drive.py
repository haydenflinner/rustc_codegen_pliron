"""lldb driver for harness/dbgcorpus.py (imported by `command script import`).

Env: DBGC_EXE, DBGC_SRC (file name), DBGC_LINES (comma list), DBGC_OUT
(json), DBGC_HITS (max stops per breakpoint), DBGC_ETC (the toolchain's
lib/rustlib/etc, whose formatters rust-lldb loads). At each stop records `bt`'s
frame descriptions (function + file:line) and `frame variable`'s
top-level variables as {name: rendered text}.
"""
import json, os, re

import lldb


def frame_vars(ci):
    r = lldb.SBCommandReturnObject()
    ci.HandleCommand("frame variable", r)
    out = r.GetOutput() or ""
    vars_, cur = {}, None
    for line in out.splitlines():
        m = re.match(r"^\((.*?)\) (\S+) =(?: (.*))?$", line)
        if m and not line.startswith(" "):
            cur = m.group(2)
            # Shadowed names: keep the first (innermost) one, as lldb prints it.
            if cur in vars_:
                cur = None
                continue
            vars_[cur] = (m.group(3) or "").strip()
        elif cur is not None:
            vars_[cur] += " " + line.strip()
    return vars_


def frames(thread, n=6):
    out = []
    for i in range(min(n, thread.GetNumFrames())):
        f = thread.GetFrameAtIndex(i)
        le = f.GetLineEntry()
        name = f.GetDisplayFunctionName() or f.GetFunctionName() or "?"
        name = re.sub(r"<.*>", "", name)
        out.append(f"{name} {os.path.basename(str(le.GetFileSpec()))}:{le.GetLine()}")
    return out


def __lldb_init_module(debugger, _dict):
    debugger.SetAsync(False)
    # rust-lldb's setup: the toolchain's Rust formatters.
    etc = os.environ.get("DBGC_ETC")
    if etc:
        debugger.HandleCommand(f"command script import {etc}/lldb_lookup.py")
    exe, src = os.environ["DBGC_EXE"], os.environ["DBGC_SRC"]
    lines = [int(x) for x in os.environ["DBGC_LINES"].split(",") if x]
    hits = int(os.environ.get("DBGC_HITS", "3"))
    t = debugger.CreateTarget(exe)
    bps = {}
    for l in lines:
        b = t.BreakpointCreateByLocation(src, l)
        bps[b.GetID()] = l
    li = t.GetLaunchInfo()
    li.SetWorkingDirectory(os.path.dirname(exe))
    err = lldb.SBError()
    p = t.Launch(li, err)
    stops, seen = [], {}
    ci = debugger.GetCommandInterpreter()
    while p and p.GetState() == lldb.eStateStopped and len(stops) < 60:
        th = p.GetSelectedThread()
        if th.GetStopReason() != lldb.eStopReasonBreakpoint:
            th = next((x for x in p if x.GetStopReason() == lldb.eStopReasonBreakpoint), None)
            if th is None:
                break
            p.SetSelectedThread(th)
        bid = th.GetStopReasonDataAtIndex(0)
        line = bps.get(bid, 0)
        k = seen.get(line, 0)
        seen[line] = k + 1
        if k < hits:
            stops.append(dict(line=line, hit=k, frames=frames(th), vars=frame_vars(ci)))
        elif k >= hits:
            t.FindBreakpointByID(bid).SetEnabled(False)
        p.Continue()
    json.dump(dict(stops=stops), open(os.environ["DBGC_OUT"], "w"), indent=1)
    if p:
        p.Kill()
