#!/usr/bin/env -S python3 -I
"""Check PLIRON_FASTCC's recorded register sets against the machine code.

  harness/fastcc_check.py BUILD-LOG BINARY-OR-OBJECT...

A caller compiled under `PLIRON_FASTCC=1` assumes a direct call destroys
only the registers in the callee's recorded set (plus x16/x17/x30). The
set comes from the register allocator's result, so a register the
emitter writes on its own (a scratch, a prologue temporary, a late
peephole) would be missing from it, and the bug would only show when
some caller happens to keep a value in exactly that register. This
looks at the code instead: it disassembles every function that has a
recorded set (aarch64, `llvm-objdump`) and reports each instruction that
writes a caller-saved register outside the set.

BUILD-LOG is the stderr of a build with

  PLIRON_FASTCC=1 PLIRON_FASTCC_NORET=0 PLIRON_TRACE=pliron::fastcc=debug

(`fastcc-set` lines: symbol, integer mask, vector mask). `NORET=0`
makes the set cover every block; with the default, writes on paths that
never return are left out of the set on purpose and would be reported
here. Jump tables are data inside the code: after a `br`, `b`, `ret` or
trap the disassembly is skipped up to the next address a direct branch
of the function targets, so a block reached only through a jump table
or by the unwinder (a landing pad) is not checked. Calls are not
followed: a `bl`'s effect is the callee's own set
(checked at the callee) or the whole ABI set.

Pass the objects the backend wrote (the `.rlib`s under the target dir,
and the `.rcgu.o` files of a binary built with `-Csave-temps`), not only
the linked binary: a linked binary also holds bodies other compilers made under the
same name (the prebuilt std's own copy of a cross-crate-inlinable
function next to the local copy this backend compiled and calls), and
those are checked against a set that is not theirs. Local symbols of the
same name in several codegen units are checked against the union of
their sets (the disassembly cannot tell the copies apart). Exit status 1
when a violation is found.
"""
import collections, os, re, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
# Written by the call sequence, the prologue/epilogue or veneers
# whatever the body does: added back at every narrowed call site.
ALWAYS_INT = {16, 17, 29, 30}
CALLEE_SAVED_INT = set(range(19, 29))
# First operand is not a destination.
NO_DEST = {
    "b", "br", "ret", "nop", "cmp", "cmn", "tst", "ccmp", "ccmn", "fcmp", "fcmpe", "fccmp", "fccmpe",
    "str", "strb", "strh", "stur", "sturb", "sturh", "stp", "stnp", "udf", "brk", "prfm", "prfum",
    "msr", "svc", "hint", "bti", "paciasp", "autiasp", "pacibsp", "autibsp", "dmb", "dsb", "isb",
    "cbz", "cbnz", "tbz", "tbnz", "bl", "blr", "st1", "st2", "st3", "st4", "stlr", "stlrb", "stlrh",
    "retaa", "retab", "braa", "brab", "clrex", "wfi", "yield",
}
TWO_DEST = {"ldp", "ldpsw", "ldnp", "ldxp", "ldaxp"}
# Branches whose operand is a target address inside the function.
DIRECT = {"b", "cbz", "cbnz", "tbz", "tbnz"}
# The status register comes first, the stored registers after it.
STATUS_FIRST = {"stxr", "stxrb", "stxrh", "stlxr", "stlxrb", "stlxrh", "stxp", "stlxp"}
VEC_LIST_LOAD = {"ld1", "ld2", "ld3", "ld4", "ld1r", "ld2r", "ld3r", "ld4r"}
REG = re.compile(r"^(?:([xw])(\d+)|([vqdshbz])(\d+))(?:\.\w+)?(?:\[\d+\])?$")


def reg(tok):
    """('i'|'v', n) for a register operand, else None (sp, zr, immediates)."""
    tok = tok.strip()
    if tok in ("fp", "x29"):
        return ("i", 29)
    if tok in ("lr", "x30"):
        return ("i", 30)
    m = REG.match(tok)
    if not m:
        return None
    if m.group(1):
        return ("i", int(m.group(2)))
    return ("v", int(m.group(4)))


def split_ops(s):
    """Top-level comma split (brackets and braces kept together)."""
    out, depth, cur = [], 0, ""
    for c in s:
        if c in "[{":
            depth += 1
        elif c in "]}":
            depth -= 1
        if c == "," and depth == 0:
            out.append(cur.strip())
            cur = ""
        else:
            cur += c
    if cur.strip():
        out.append(cur.strip())
    return out


def written(mn, ops_text):
    """Registers an instruction writes, as a set of ('i'|'v', n)."""
    ops = split_ops(ops_text)
    w = set()
    base = mn.split(".")[0]
    if base.startswith("b.") or mn.startswith("b."):
        return w
    if base in VEC_LIST_LOAD and ops:
        for t in re.split(r"[,\s{}]+", ops[0]):
            # `{ v0.16b - v3.16b }` ranges are not printed by llvm for
            # these; each register is listed.
            r = reg(t)
            if r:
                w.add(r)
    elif base in STATUS_FIRST or base not in NO_DEST:
        n = 2 if base in TWO_DEST else 1
        for t in ops[:n]:
            r = reg(t)
            if r:
                w.add(r)
    # Writeback: `[xN, #imm]!` or `[xN], #imm` / `[xN], xM`.
    for k, t in enumerate(ops):
        if t.startswith("[") and (t.endswith("]!") or (t.endswith("]") and k + 1 < len(ops) and k == len(ops) - 2)):
            # The second form needs the bracket to hold the base alone.
            inner = t.strip("[]!")
            if t.endswith("]!") or "," not in inner:
                r = reg(inner.split(",")[0])
                if r:
                    w.add(r)
    return w


def objdump():
    try:
        root = subprocess.run(["rustc", "--print", "sysroot"], capture_output=True, text=True, cwd=HERE).stdout.strip()
        for d, _, fs in os.walk(os.path.join(root, "lib", "rustlib")):
            if "llvm-objdump" in fs:
                return os.path.join(d, "llvm-objdump")
    except OSError:
        pass
    return "llvm-objdump"


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    sets = {}
    for l in open(sys.argv[1], errors="replace"):
        i = l.find("fastcc-set\t")
        if i < 0:
            continue
        p = l[i:].rstrip("\n").split("\t")
        if len(p) != 4:
            continue
        im, vm = int(p[2], 16), int(p[3], 16)
        a, b = sets.get(p[1], (0, 0))
        sets[p[1]] = (a | im, b | vm)
    if not sets:
        sys.exit("fastcc_check: no fastcc-set lines in the log (build with PLIRON_TRACE=pliron::fastcc=debug)")
    od = objdump()
    bad = collections.defaultdict(list)
    seen, insts, unknown = set(), 0, collections.Counter()
    skipped = 0

    def check(cur, body):
        """One function: `body` is [(address, mnemonic, operands, line)]."""
        nonlocal insts, skipped
        # A jump table sits in the code after its `br`, and its entries
        # disassemble as whatever they happen to encode. After a transfer
        # that does not fall through, skip to the next address a direct
        # branch of this function targets. (A block reached only through
        # the table, or a landing pad, is skipped with it.)
        targets = set()
        for _, mn, ops, _ in body:
            if mn in DIRECT or mn.startswith("b."):
                m = re.search(r"0x([0-9a-f]+)", ops)
                if m:
                    targets.add(int(m.group(1), 16))
        im, vm = sets[cur]
        dead = False
        for addr, mn, ops, l in body:
            if dead and addr not in targets:
                skipped += 1
                continue
            dead = mn in ("b", "br", "ret", "udf", "brk", "retaa", "retab", "braa", "brab")
            insts += 1
            for kind, n in written(mn, ops):
                if kind == "i":
                    if n in ALWAYS_INT or n in CALLEE_SAVED_INT or n == 18 or im >> n & 1:
                        continue
                elif vm >> n & 1:
                    continue
                bad[cur].append(f"{'x' if kind == 'i' else 'v'}{n}: {l.strip()}")

    for path in sys.argv[2:]:
        dis = subprocess.run([od, "-d", "--no-show-raw-insn", path], capture_output=True, text=True,
                             errors="replace").stdout
        cur, body = None, []
        for l in dis.splitlines() + ["0 <end>:"]:
            m = re.match(r"^[0-9a-f]+ <(.+)>:$", l)
            if m:
                if cur:
                    check(cur, body)
                name = m.group(1)
                # Mach-O symbols carry one more leading underscore.
                cur = name if name in sets else name[1:] if name[1:] in sets else None
                body = []
                if cur:
                    seen.add(cur)
                continue
            if cur is None:
                continue
            m = re.match(r"^\s*([0-9a-f]+):\s+(\S+)\s*(.*)$", l)
            if not m:
                continue
            mn, ops = m.group(2), m.group(3).split("//")[0].split(";")[0].strip()
            if mn in (".word", ".long", "<unknown>"):
                unknown[mn] += 1
                continue
            body.append((int(m.group(1), 16), mn, ops, l))
    print(f"fastcc_check: {len(sets)} recorded sets, {len(seen)} found in the code, {insts} instructions checked"
          + (f", {sum(unknown.values())} undecoded words and {skipped} words after a non-returning transfer skipped"
             if unknown or skipped else ""))
    for name, ls in bad.items():
        print(f"VIOLATION {name}: {len(ls)} write(s) outside the set")
        for x in ls[:8]:
            print("   ", x)
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
