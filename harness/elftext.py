"""ELF64 (aarch64) text and symbols, and callgrind per-instruction costs.

Shared by harness/placement.py, bench/heldout/irdiff.py and
harness/rastat.py. Callgrind's own `fn=` context is not the function that
contains an address on our code: in a pliron build of pulldown-cmark an
address of `HtmlWriter::run` was charged to `escape_html_scalar`, its
`'2` recursion context and `Parser::next'2`, and a function table built
from `fn=` showed +-1.2M swaps between unrelated functions. Charge every
executed address to the ELF symbol that contains it instead.
"""
import bisect, collections, struct

CLASSES = ["nop", "jump", "cbranch", "call", "ret", "load", "store", "other"]


def klass(w):
    """Instruction class of one aarch64 word."""
    if w == 0xD503201F:
        return "nop"
    top6 = w & 0xFC000000
    if top6 == 0x14000000:
        return "jump"  # b
    if top6 == 0x94000000:
        return "call"  # bl
    if (w & 0xFF000010) == 0x54000000 or (w & 0x7E000000) in (0x34000000, 0x36000000):
        return "cbranch"  # b.cond, cbz/cbnz, tbz/tbnz
    if (w & 0xFE000000) == 0xD6000000:  # branch to register
        opc = (w >> 21) & 0xF
        return {0: "jump", 1: "call", 2: "ret"}.get(opc, "other")
    if (w & 0x0A000000) == 0x08000000:  # loads and stores
        if (w & 0x3B000000) == 0x18000000:
            return "load"  # ldr literal
        return "load" if w & (1 << 22) else "store"
    return "other"


def elf_read(path):
    """(executable sections [(address, bytes)], function symbols sorted
    [(address, size, name)]) of an ELF64 LE file."""
    data = open(path, "rb").read()
    assert data[:4] == b"\x7fELF" and data[4] == 2 and data[5] == 1, f"{path}: not ELF64 LE"
    shoff, = struct.unpack_from("<Q", data, 0x28)
    shentsize, shnum = struct.unpack_from("<HH", data, 0x3A)
    hdrs = [struct.unpack_from("<IIQQQQII", data, shoff + i * shentsize) for i in range(shnum)]
    secs, syms = [], []
    for _, typ, flags, addr, off, size, link, _ in hdrs:
        if typ == 1 and flags & 0x4:  # PROGBITS, EXECINSTR
            secs.append((addr, data[off:off + size]))
        if typ == 2:  # SYMTAB
            stroff = hdrs[link][4]
            for o in range(off, off + size, 24):
                name, info, _, shndx, value, ssize = struct.unpack_from("<IBBHQQ", data, o)
                if info & 0xF == 2 and shndx and value:  # STT_FUNC, defined
                    end = data.index(b"\0", stroff + name)
                    syms.append((value, ssize, data[stroff + name:end].decode(errors="replace")))
    syms.sort()
    return secs, syms


class Text:
    """Instruction words and the function symbol of an address."""

    def __init__(self, path):
        self.secs, self.syms = elf_read(path)
        self.starts = [s[0] for s in self.syms]

    def sym(self, a):
        i = bisect.bisect_right(self.starts, a) - 1
        if i >= 0 and a < self.syms[i][0] + max(self.syms[i][1], 4):
            return self.syms[i][2]
        return "?"

    def word(self, a):
        for base, b in self.secs:
            if base <= a < base + len(b) - 3:
                return struct.unpack_from("<I", b, a - base)[0]
        return None


def callgrind_instr(path):
    """A callgrind out file written with `--dump-instr=yes`
    `--compress-pos=no` (`--dump-line` either way) ->
    ({address: self Ir}, {(address, file, line): self Ir} when lines are dumped).
    The inclusive cost line after each `calls=` is skipped."""
    ir = collections.Counter()
    lines = collections.Counter()
    nlines = False
    fl = cur = None
    skip = False
    last = 0
    for l in open(path, errors="replace"):
        c = l[:1]
        if c.isdigit() or c in "+-*":
            if skip:
                skip = False
                continue
            p = l.split()
            if len(p) < 2:
                continue
            a = p[0]
            if a[0] == "+":
                addr = last + int(a[1:], 0)
            elif a[0] == "-":
                addr = last - int(a[1:], 0)
            elif a == "*":
                addr = last
            else:
                addr = int(a, 0)
            last = addr
            cost = int(p[2]) if nlines and len(p) >= 3 else int(p[1])
            ir[addr] += cost
            if nlines and len(p) >= 3 and p[1].isdigit():
                lines[(addr, cur, int(p[1]))] += cost
            continue
        if l.startswith("positions:"):
            nlines = "line" in l
        elif l.startswith("fl="):
            fl = cur = l[3:].strip()
        elif l.startswith(("fi=", "fe=")):
            cur = l[3:].strip()
        elif l.startswith("fn="):
            cur = fl
        elif l.startswith("calls="):
            skip = True
    return ir, lines
