# Prints an object's relocations in a form two assemblers can agree on.
#
#   llvm-readobj --symbols o.o > syms
#   llvm-readobj --relocs --expand-relocs o.o | awk -f relocs.awk syms -
#
# One line per relocation, in table order:
#
#   .rela.text 0x4 R_RISCV_PCREL_LO12_I @.text+0x0 +0
#
# The target is written by what the linker will make of it, not by how the
# symbol table happens to spell it. A global or undefined symbol is its name
# plus the addend. A local one is its section plus the offset symbol and
# addend add up to: GNU as and rsasm relocate against `.text+0x40`, llvm-mc
# against a label at 0x40, and every linker reads those the same.
#
# Except where the linker looks the symbol itself up. RISC-V's
# `R_RISCV_PCREL_LO12_*` names the `auipc` carrying the high half, and lld
# finds it by the symbol's value alone, ignoring the addend; a GOT or TLS
# reference names the symbol that owns the slot. For those the symbol must be
# a label, written `@section+value` with the addend kept apart, so a section
# symbol plus an addend shows up as a difference.
#
# Plain POSIX awk: no strtonum, so hex is converted by hand.

function hex(h, width,    i, c, v, n, neg) {
    sub(/^0x/, "", h)
    h = toupper(h)
    n = length(h)
    # Addends are signed; readobj writes a negative one as its two's
    # complement at the file's address width.
    neg = (n == width / 4 && index("89ABCDEF", substr(h, 1, 1)) > 0)
    v = 0
    for (i = 1; i <= n; i++) {
        c = index("0123456789ABCDEF", substr(h, i, 1)) - 1
        if (neg)
            c = 15 - c
        v = v * 16 + c
    }
    return neg ? -(v + 1) : v
}

function num(v) {
    return v < 0 ? sprintf("-0x%x", -v) : sprintf("0x%x", v)
}

# `Name: foo (12)` -> `foo`; the trailing group is a string-table offset or
# an index.
function field(line,    s) {
    s = line
    sub(/^[^:]*: ?/, "", s)
    sub(/ ?\([0-9a-fA-Fx]+\)$/, "", s)
    return s
}

function paren(line,    s) {
    s = line
    sub(/.*\(/, "", s)
    sub(/\).*/, "", s)
    return s
}

/AddressSize:/ {
    width = $2
    sub(/bit/, "", width)
}

# ---- the first file: the symbol table ----
FNR == NR {
    if ($1 == "Symbol" && $2 == "{") {
        idx = nsyms++
    } else if ($1 == "Name:") {
        name[idx] = field($0)
    } else if ($1 == "Value:") {
        value[idx] = hex($2, 0)
    } else if ($1 == "Binding:") {
        bind[idx] = $2
    } else if ($1 == "Type:") {
        type[idx] = $2
    } else if ($1 == "Section:") {
        sect[idx] = field($0)
    }
    next
}

# ---- the second file: the relocations ----
$1 == "Section" && $NF == "{" {
    relsec = $3
}
$1 == "Offset:" {
    offset = $2
    # A REL entry has no addend line: its addend is in the section's bytes.
    addend = 0
}
$1 == "Type:" {
    rtype = $2
    # A type readobj has no name for, as for RX and RL78, by its number.
    if (rtype == "Unknown")
        rtype = "R_" paren($0)
}
$1 == "Symbol:" {
    sym = paren($0) + 0
}
$1 == "Addend:" {
    addend = hex($2, width)
}
$1 == "}" && rtype != "" {
    if (sym == 0) {
        target = "*ABS*" (addend < 0 ? "" : "+") num(addend)
    } else if (bind[sym] != "Local" || sect[sym] == "Undefined") {
        target = name[sym] (addend < 0 ? "" : "+") num(addend)
    } else if (rtype ~ /PCREL_LO12|GOT_HI20|TLS_GD_HI20|TLSDESC/ && type[sym] != "Section") {
        target = "@" sect[sym] "+" num(value[sym]) " " (addend < 0 ? "" : "+") num(addend)
    } else {
        target = sect[sym] "+" num(value[sym] + addend)
    }
    print relsec, offset, rtype, target
    rtype = ""
}
