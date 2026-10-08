//! Instruction encoding: prefixes, REX/VEX/EVEX, opcode, ModRM/SIB,
//! displacement and immediate.

use super::insn::{
    ADDR16, ADDR32, DEF64, DISTINCT_DEST, Def, EVEX_ER, EVEX_SAE, Enc, IMM64, ModRm, NEEDS_MASK,
    NO_REX_W, NO64, NO66, NOMASK, ONLY64, Op, PLUSREG, R_IN_RM, SIBMEM, Vk, WAIT,
};
use super::operand::{Decor, Mem, Operand, OperandKind, RoundCtl};
use super::reg::{self, Reg, RegClass};
use super::reloc;
use crate::arch::AsmCtx;
use crate::expr::{ExprKind, ExprRef};
use crate::reloc::RelocClass;
use crate::section::{Fixup, FixupKind, Variant};
use crate::source::Span;

/// Legacy prefixes contributed by `lock`, `rep` and friends.
#[derive(Clone, Copy, Default, Debug)]
pub struct Prefixes {
    pub lock: bool,
    /// 0xF3 (`rep`/`repe`) or 0xF2 (`repne`), whichever was written.
    pub rep: Option<u8>,
    /// A segment override written as a standalone prefix, as in `fs movq ...`.
    pub seg: Option<u8>,
    /// `data16`/`data32` and `addr16`/`addr32`, which ask for the operand or
    /// address size override whether or not the operands need it.
    pub data: bool,
    pub addr: bool,
    /// A `{vex}`, `{vex3}` or `{evex}` pseudo-prefix.
    pub(crate) encoding: Option<EncodingPrefix>,
}

/// The encoding a pseudo-prefix asks for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum EncodingPrefix {
    /// `{vex}` (or `{vex2}`): VEX, in whichever length fits.
    Vex,
    /// `{vex3}`: VEX in its three-byte form even where two bytes would do.
    Vex3,
    /// `{evex}`: EVEX even where VEX would do.
    Evex,
}

/// Which operand fills which encoding slot, worked out from the pattern.
struct Roles<'o> {
    rm: Option<&'o Operand>,
    reg: Option<Reg>,
    /// The non-destructive source VEX and EVEX carry in `vvvv`.
    nds: Option<Reg>,
    /// A register named by the top nibble of a trailing immediate byte.
    is4: Option<Reg>,
    /// (expression, encoded width in bytes)
    imm: Option<(ExprRef, u8)>,
    imm2: Option<(ExprRef, u8)>,
    rel: Option<(ExprRef, u8)>,
    /// The absolute address of a `moffs` form.
    moffs: Option<&'o Operand>,
    /// The operands a string instruction was written with, if any.
    str_src: Option<&'o Operand>,
    str_dst: Option<&'o Operand>,
    /// The segment and offset of a direct far pointer.
    far: Option<(ExprRef, ExprRef)>,
    /// The decorators found on the operands, merged.
    decor: Decor,
}

pub fn segment_prefix(r: Reg) -> Option<u8> {
    Some(match r.num {
        0 => 0x26, // es
        1 => 0x2e, // cs
        2 => 0x36, // ss
        3 => 0x3e, // ds
        4 => 0x64, // fs
        5 => 0x65, // gs
        _ => return None,
    })
}

/// Extracts the operand expression a `Rel` slot refers to. A bare label parses
/// as a displacement-only memory operand, and a bare number as one in AT&T
/// syntax and as an immediate in Intel syntax, so all of those are accepted
/// here; an address in parentheses or brackets, or with a size, is not.
pub fn rel_expr(o: &Operand) -> Option<ExprRef> {
    match &o.kind {
        OperandKind::Imm(e) => Some(*e),
        OperandKind::Mem(m)
            if m.base.is_none()
                && m.index.is_none()
                && !m.rip_relative
                && !m.bracketed
                && m.seg.is_none()
                && o.size_hint.is_none() =>
        {
            m.disp
        }
        _ => None,
    }
}

/// The register or memory operand behind an indirect branch target.
pub fn indirect_inner(o: &Operand) -> Option<Operand> {
    match &o.kind {
        OperandKind::Indirect(inner) => Some(Operand {
            kind: (**inner).clone(),
            size_hint: o.size_hint,
            decor: o.decor,
            span: o.span,
        }),
        // Intel syntax writes indirect branches without a sigil.
        OperandKind::Reg(_) | OperandKind::Mem(_) => Some(o.clone()),
        _ => None,
    }
}

/// The register behind an r/m operand, if it is a register rather than memory.
fn rm_register(o: &Operand) -> Option<Reg> {
    match &o.kind {
        OperandKind::Reg(r) => Some(*r),
        OperandKind::Indirect(inner) => match &**inner {
            OperandKind::Reg(r) => Some(*r),
            _ => None,
        },
        _ => None,
    }
}

/// Works out which operand fills which encoding slot.
///
/// The pattern says everything needed: the first r/m-capable operand goes to
/// ModRM.rm, a register operand goes to ModRM.reg (or the low bits of a `+r`
/// opcode), and immediates and branch targets go to their own fields.
fn assign_roles<'o>(def: &Def, ops: &'o [Operand]) -> Roles<'o> {
    let mut roles = Roles {
        rm: None,
        reg: None,
        nds: None,
        is4: None,
        imm: None,
        imm2: None,
        rel: None,
        moffs: None,
        str_src: None,
        str_dst: None,
        far: None,
        decor: Decor::default(),
    };
    let takes_reg_field = def.modrm == ModRm::Reg || def.flags & PLUSREG != 0;
    // A segment, control or debug register can only be in the reg field, so
    // a general register beside one goes to r/m whatever order they are in.
    let special_reg = def
        .ops
        .iter()
        .any(|p| matches!(p, Op::Seg | Op::Cr | Op::Dr));
    for (pat, o) in def.ops.iter().zip(ops) {
        // Decorators are written on whichever operand they qualify, but they
        // all end up in the one EVEX prefix, so they are merged here.
        if !o.decor.is_empty() {
            let d = &o.decor;
            roles.decor.mask = roles.decor.mask.or(d.mask);
            roles.decor.zeroing |= d.zeroing;
            roles.decor.broadcast = roles.decor.broadcast.or(d.broadcast);
            roles.decor.span = if roles.decor.span.is_dummy() {
                d.span
            } else {
                roles.decor.span
            };
        }
        match *pat {
            Op::Rm(_) | Op::M(_) | Op::IndirectRm(_) | Op::FarM | Op::Fword | Op::FarDword
                if roles.rm.is_none() =>
            {
                roles.rm = Some(o)
            }
            Op::Seg | Op::Cr | Op::Dr | Op::St if takes_reg_field && roles.reg.is_none() => {
                roles.reg = o.reg()
            }
            Op::Moffs(_) => roles.moffs = Some(o),
            Op::StrSrc(_) => roles.str_src = Some(o),
            Op::StrDst(_) => roles.str_dst = Some(o),
            Op::Far => {
                if let OperandKind::FarPtr { seg, off } = o.kind {
                    roles.far = Some((seg, off));
                }
            }
            Op::Vm(..) | Op::Vsib(_) if roles.rm.is_none() => roles.rm = Some(o),
            Op::V(_) if takes_reg_field && roles.reg.is_none() => roles.reg = o.reg(),
            // A `/digit` encoding has no reg field, so its register operand
            // goes to r/m instead: that is how the shift-by-immediate forms
            // of `psllw` and friends are built.
            Op::V(_) if roles.rm.is_none() => roles.rm = Some(o),
            Op::Nds(_) | Op::NdsR(_) => roles.nds = o.reg(),
            Op::Is4(_) => roles.is4 = o.reg(),
            Op::R(_)
                if takes_reg_field
                    && !special_reg
                    && def.flags & R_IN_RM == 0
                    && roles.reg.is_none() =>
            {
                roles.reg = o.reg()
            }
            // An encoding with no reg field puts its register in r/m instead.
            Op::R(_) if roles.rm.is_none() => roles.rm = Some(o),
            // `enter` has two immediates, written in the order they are encoded.
            Op::Imm(w) => {
                if let OperandKind::Imm(e) = &o.kind {
                    match roles.imm {
                        None => roles.imm = Some((*e, w)),
                        Some(_) => roles.imm2 = Some((*e, w)),
                    }
                }
            }
            Op::Imm8s => {
                if let OperandKind::Imm(e) = &o.kind {
                    roles.imm = Some((*e, 1));
                }
            }
            Op::Rel(w) => {
                if let Some(e) = rel_expr(o) {
                    roles.rel = Some((e, w));
                }
            }
            _ => {}
        }
    }
    roles
}

/// The `pp` field VEX and EVEX use in place of a mandatory legacy prefix.
fn pp_bits(pfx: u8) -> u8 {
    match pfx {
        0x66 => 1,
        0xf3 => 2,
        0xf2 => 3,
        _ => 0,
    }
}

/// The `L'L` field: 0 = 128-bit, 1 = 256-bit, 2 = 512-bit.
fn len_bits(vlen: u16) -> u8 {
    match vlen {
        256 => 1,
        512 => 2,
        _ => 0,
    }
}

/// What an encoding depends on besides the instruction: the current mode, and
/// the relocation numbering of the object being written. They differ for a
/// `.code32` stretch inside an x86-64 object.
#[derive(Copy, Clone, Debug)]
pub struct Target {
    pub bits: u8,
    pub abi: reloc::Abi,
}

pub fn encode(
    cx: &mut AsmCtx<'_>,
    target: Target,
    def: &Def,
    ops: &[Operand],
    prefixes: Prefixes,
    rounding: Option<(RoundCtl, Span)>,
    span: Span,
) -> Option<Variant> {
    let Target { bits, abi } = target;
    if bits == 64 && def.flags & NO64 != 0 {
        cx.error(span, "this instruction is not encodable in 64-bit mode");
        return None;
    }
    if bits != 64 && def.flags & ONLY64 != 0 {
        cx.error(span, "this instruction is only encodable in 64-bit mode");
        return None;
    }

    let roles = assign_roles(def, ops);
    let mut bytes: Vec<u8> = Vec::with_capacity(8);
    let mut fixups: Vec<Fixup> = Vec::new();

    // ---- legacy prefixes --------------------------------------------------
    // In the order GNU as writes them, which is by kind rather than as
    // written: `fwait`, segment, address size, operand size, repeat, lock.
    // llvm-mc orders some of them differently; the CPU does not care.
    if def.flags & WAIT != 0 {
        bytes.push(0x9b);
    }

    let mem_of = |o: &Operand| match &o.kind {
        OperandKind::Mem(m) => Some(m.clone()),
        OperandKind::Indirect(inner) => match &**inner {
            OperandKind::Mem(m) => Some(m.clone()),
            _ => None,
        },
        _ => None,
    };
    // A string instruction's operands only say what its implicit ones are:
    // their address size, and a segment for the source. The destination is
    // always in `es`.
    let str_src = roles.str_src.and_then(mem_of);
    let str_dst = roles.str_dst.and_then(mem_of);
    if let Some(d) = &str_dst
        && let Some(seg) = d.seg.filter(|s| s.num != 0)
    {
        cx.error(
            d.span,
            format!(
                "a string destination is always in `es`, and cannot be in `{}`",
                reg::name_of(seg)
            ),
        );
        return None;
    }
    if let (Some(s), Some(d)) = (&str_src, &str_dst)
        && s.base.or(s.index).is_some()
        && d.base.or(d.index).is_some()
        && s.addr_size != d.addr_size
    {
        cx.error(span, "the two string operands have different address sizes");
        return None;
    }
    let is_string = str_src.is_some() || str_dst.is_some();
    let mem = roles
        .rm
        .or(roles.moffs)
        .and_then(mem_of)
        .or(str_src)
        .or(str_dst.map(|mut d| {
            d.seg = None;
            d
        }));

    check_decorators(cx, def, &roles, rounding, mem.is_some(), span)?;
    check_vsib(cx, def, mem.as_ref(), span)?;

    // Only EVEX has the fifth register-number bit, so `xmm16` and above are
    // unreachable from any other encoding even though they parse fine.
    if def.enc != Enc::Evex {
        let high = [
            roles.reg,
            roles.nds,
            roles.is4,
            roles.rm.and_then(rm_register),
        ]
        .into_iter()
        .flatten()
        .chain(mem.iter().flat_map(|m| [m.base, m.index]).flatten())
        .find(|r| r.needs_evex_ext());
        if let Some(r) = high {
            cx.error(
                span,
                format!(
                    "`{}` is only reachable through an EVEX-encoded instruction",
                    reg::name_of(r)
                ),
            );
            return None;
        }
    }

    let seg_override = match mem.as_ref().and_then(|m| m.seg.map(|s| (m, s))) {
        // An override naming the segment the address uses anyway is left
        // out, as GNU as does: `ss` for a `bp` or `sp` base, `ds` otherwise.
        // llvm-mc keeps it.
        Some((m, seg)) if seg.num == if is_string { 3 } else { default_segment(m) } => prefixes.seg,
        Some((_, seg)) => match segment_prefix(seg) {
            Some(p) => Some(p),
            None => {
                cx.error(
                    span,
                    format!("`{}` is not a valid segment override", reg::name_of(seg)),
                );
                return None;
            }
        },
        // GNU as drops a segment prefix written before a direct far branch
        // or `call`, which have no memory operand for it to apply to. On a
        // direct `jmp` it keeps `cs` and `ds`, which double as branch hints,
        // and it keeps everything before a conditional jump.
        None if matches!(def.ops.as_slice(), [Op::Far])
            || matches!(def.ops.as_slice(), [Op::Rel(_)]) && def.opcode == [0xe8]
            || prefixes.seg.is_some_and(|p| p != 0x2e && p != 0x3e)
                && matches!(def.ops.as_slice(), [Op::Rel(_)])
                && matches!(def.opcode.as_slice(), [0xe9 | 0xeb]) =>
        {
            None
        }
        None => prefixes.seg,
    };
    if let Some(p) = seg_override {
        bytes.push(p);
    }

    // Address-size override: the other address size the mode can reach, as
    // 32-bit addressing in 64-bit mode, or an implicit counter register of
    // the other size.
    let mut addr_override = match bits {
        16 => def.flags & ADDR32 != 0,
        32 => def.flags & ADDR16 != 0,
        _ => def.flags & ADDR32 != 0,
    };
    if let Some(m) = &mem {
        let uses_regs = m.base.is_some() || m.index.is_some();
        let native = bits / 8;
        if uses_regs && m.addr_size != native {
            if matches!((bits, m.addr_size), (64, 4) | (32, 2) | (16, 4)) {
                addr_override = true;
            } else {
                cx.error(
                    m.span,
                    format!(
                        "{}-bit addressing is not available in {bits}-bit mode",
                        m.addr_size * 8
                    ),
                );
                return None;
            }
        }
    }
    if addr_override && prefixes.addr {
        cx.error(
            span,
            "the address size prefix is already implied by the operands",
        );
        return None;
    }
    if addr_override || prefixes.addr {
        bytes.push(0x67);
    }

    let rm_reg = roles.rm.and_then(rm_register);
    let plus_reg = def.flags & PLUSREG != 0;

    if def.flags & DISTINCT_DEST != 0
        && let Some(dst) = roles.reg
        && [roles.nds, rm_reg].contains(&Some(dst))
    {
        cx.error(
            span,
            "the destination register must differ from both sources",
        );
        return None;
    }
    // AMX's tile arithmetic reads and writes whole tiles in place, so no two
    // of its three tiles may be the same.
    if let (Some(a), Some(b), Some(c)) = (roles.reg, roles.nds, rm_reg)
        && a.class == RegClass::Tmm
        && (a == b || a == c || b == c)
    {
        cx.error(span, "all three tile registers must be different");
        return None;
    }

    // VEX and EVEX store their extension bits inverted so that outside 64-bit
    // mode an unextended prefix still decodes, there, as the `LES`/`LDS`/
    // `BOUND` opcode it overlays. Setting one would produce a different
    // instruction, so the registers that need one simply do not exist.
    // The legacy path makes the same check when it builds a REX byte.
    if bits != 64 && def.enc != Enc::Legacy {
        let all = [roles.reg, roles.nds, roles.is4, rm_reg]
            .into_iter()
            .flatten()
            .chain(mem.iter().flat_map(|m| [m.base, m.index]).flatten());
        for r in all {
            if r.num >= 8 || (r.is_gpr() && r.size == 8) {
                cx.error(
                    span,
                    format!("`{}` is only available in 64-bit mode", reg::name_of(r)),
                );
                return None;
            }
        }
    }

    // The REX-style register extension bits, worked out before deciding which
    // prefix will carry them. `X` doubles as the fifth bit of a register-direct r/m
    // operand under EVEX, which is how `xmm16`-`xmm31` are reached there.
    let base_reg = mem.as_ref().and_then(|m| m.base);
    let index_reg = mem.as_ref().and_then(|m| m.index);
    let ext_r = !plus_reg && roles.reg.is_some_and(|r| r.num & 8 != 0);
    let ext_b = rm_reg.is_some_and(|r| r.num & 8 != 0)
        || base_reg.is_some_and(|r| r.num & 8 != 0)
        || (plus_reg && roles.reg.is_some_and(|r| r.num & 8 != 0));
    let ext_x = match (&index_reg, rm_reg) {
        (Some(i), _) => i.num & 8 != 0,
        (None, Some(r)) if def.enc == Enc::Evex => r.num & 16 != 0,
        _ => false,
    };

    match def.enc {
        Enc::Legacy => {
            // Operand-size override.
            let wants_66 = def.flags & NO66 == 0
                && match def.opsize {
                    16 => bits != 16,
                    32 => bits == 16,
                    _ => false,
                };
            if wants_66 && prefixes.data {
                cx.error(
                    span,
                    "the operand size prefix is already implied by the operands",
                );
                return None;
            }
            if wants_66 || prefixes.data {
                bytes.push(0x66);
            }
            push_rep_lock(&mut bytes, prefixes);
            if def.pfx != 0 {
                bytes.push(def.pfx);
            }

            let rex_w = def.opsize == 64
                && def.flags & NO_REX_W == 0
                && !(bits == 64 && def.flags & DEF64 != 0);
            if def.opsize == 64 && bits != 64 && def.flags & DEF64 == 0 {
                cx.error(span, "64-bit operands require 64-bit mode");
                return None;
            }

            // spl/bpl/sil/dil only exist with a REX prefix present, even an
            // empty one.
            let forced_rex =
                roles.reg.is_some_and(|r| r.rex_required) || rm_reg.is_some_and(|r| r.rex_required);
            // ah/ch/dh/bh cannot coexist with REX.
            let has_high_byte = roles.reg.is_some_and(|r| r.class == RegClass::GprHigh)
                || rm_reg.is_some_and(|r| r.class == RegClass::GprHigh);

            let need_rex = rex_w || ext_r || ext_b || ext_x || forced_rex;
            if need_rex {
                if has_high_byte {
                    cx.error(
                        span,
                        "`ah`, `ch`, `dh` and `bh` cannot be used in an instruction that needs a REX prefix",
                    );
                    return None;
                }
                if bits != 64 {
                    cx.error(span, "this operand combination requires 64-bit mode");
                    return None;
                }
                let rex = 0x40
                    | ((rex_w as u8) << 3)
                    | ((ext_r as u8) << 2)
                    | ((ext_x as u8) << 1)
                    | (ext_b as u8);
                bytes.push(rex);
            }
        }
        Enc::Vex => {
            push_rep_lock(&mut bytes, prefixes);
            let vvvv = roles.nds.map_or(0, |r| r.num);
            let l = len_bits(def.vlen);
            let pp = pp_bits(def.pfx);
            let w = def.vex_w();
            // The two-byte form has no room for X, B or W, and only reaches
            // the `0F` map; anything else has to spell the prefix out, and so
            // does `{vex3}`.
            let short = prefixes.encoding != Some(EncodingPrefix::Vex3);
            if short && def.map == 1 && !w && !ext_x && !ext_b {
                bytes.push(0xc5);
                bytes.push(((!ext_r as u8) << 7) | ((!vvvv & 0xf) << 3) | (l << 2) | pp);
            } else {
                // XOP is the three-byte VEX layout behind `8F`, which stays
                // `POP r/m` because its maps start at 8: a real `POP` has a
                // zero reg field where XOP's inverted `RXB` and map sit.
                bytes.push(if def.map >= 8 { 0x8f } else { 0xc4 });
                bytes.push(
                    ((!ext_r as u8) << 7)
                        | ((!ext_x as u8) << 6)
                        | ((!ext_b as u8) << 5)
                        | (def.map & 0x1f),
                );
                bytes.push(((w as u8) << 7) | ((!vvvv & 0xf) << 3) | (l << 2) | pp);
            }
        }
        Enc::Evex => {
            push_rep_lock(&mut bytes, prefixes);
            let vvvv = roles.nds.map_or(0, |r| r.num);
            let pp = pp_bits(def.pfx);
            let w = def.vex_w();
            let ext_r2 = roles.reg.is_some_and(|r| r.num & 16 != 0);
            // `V'` extends `vvvv`, except with a VSIB memory operand, where it
            // is the fifth bit of the vector index instead.
            let ext_v2 = match index_reg {
                Some(i) if i.is_vector() => i.num & 16 != 0,
                _ => vvvv & 16 != 0,
            };
            let broadcast = roles.decor.broadcast.is_some();
            // Embedded rounding replaces the vector length with the rounding
            // mode and sets `b`, which is why it only exists on register-only
            // forms: there is no memory operand left to broadcast or scale.
            let (ll, b_bit) = match rounding {
                Some((ctl, _)) => (ctl.ll(), true),
                None => (len_bits(def.vlen), broadcast),
            };
            let aaa = roles.decor.mask.map_or(0, |r| r.num);
            bytes.push(0x62);
            bytes.push(
                ((!ext_r as u8) << 7)
                    | ((!ext_x as u8) << 6)
                    | ((!ext_b as u8) << 5)
                    | ((!ext_r2 as u8) << 4)
                    | (def.map & 7),
            );
            bytes.push(((w as u8) << 7) | ((!vvvv & 0xf) << 3) | (1 << 2) | pp);
            bytes.push(
                ((roles.decor.zeroing as u8) << 7)
                    | (ll << 5)
                    | ((b_bit as u8) << 4)
                    | ((!ext_v2 as u8) << 3)
                    | (aaa & 7),
            );
        }
    }

    // ---- opcode -----------------------------------------------------------
    bytes.extend_from_slice(&def.opcode);
    if plus_reg {
        let Some(r) = roles.reg else {
            cx.error(span, "internal: `+r` encoding without a register operand");
            return None;
        };
        let last = bytes.len() - 1;
        bytes[last] += r.num & 7;
    }

    // ---- ModRM / SIB / displacement ---------------------------------------
    // A RIP-relative displacement is measured from the end of the whole
    // instruction, so its fixup is built after the immediate has been emitted.
    let mut disp_fixup: Option<DispFixup> = None;

    // EVEX scales an 8-bit displacement by the size of the memory access, so
    // one byte still spans a 512-bit stride. See `Tuple`.
    let disp_scale = if def.enc == Enc::Evex {
        let vbytes = def.vlen as u32 / 8;
        match def
            .tuple
            .scale(vbytes, def.vex_w(), roles.decor.broadcast.is_some())
        {
            Some(n) => n,
            None => {
                if mem.is_some() {
                    cx.error(span, "internal: EVEX memory form with no tuple type");
                    return None;
                }
                1
            }
        }
    } else {
        1
    };

    match def.modrm {
        ModRm::None => {}
        ModRm::Reg | ModRm::Ext(_) => {
            let reg_field = match def.modrm {
                ModRm::Ext(e) => e,
                _ => match roles.reg {
                    Some(r) => r.num & 7,
                    None => {
                        cx.error(span, "internal: `/r` encoding without a register operand");
                        return None;
                    }
                },
            };
            match roles.rm {
                Some(rm_operand) => encode_rm(
                    cx,
                    bits,
                    &mut bytes,
                    &mut disp_fixup,
                    reg_field,
                    rm_operand,
                    mem.as_ref(),
                    disp_scale,
                    def.flags & SIBMEM != 0,
                )?,
                // A lone register in ModRM.reg, with r/m unused and zero, as
                // AMX's `tilezero` is encoded.
                None if def.enc == Enc::Vex && def.ops.len() == 1 && roles.reg.is_some() => {
                    bytes.push(0xc0 | (reg_field << 3));
                }
                None => {
                    cx.error(span, "internal: encoding needs an r/m operand");
                    return None;
                }
            }
        }
    }

    // ---- absolute address and far pointer ---------------------------------
    if let (Some(m), Some(_)) = (&mem, roles.moffs) {
        let width = m.addr_size;
        let offset = bytes.len();
        bytes.extend(std::iter::repeat_n(0u8, width as usize));
        if let Some(e) = m.disp {
            match cx.constant(e) {
                Some(v) => bytes[offset..].copy_from_slice(&v.to_le_bytes()[..width as usize]),
                None => disp_fixup = Some((offset, e, m.span, false, width)),
            }
        }
    }
    if let Some((seg, off)) = roles.far {
        let width = if def.opsize == 16 { 2 } else { 4 };
        for (e, width) in [(off, width), (seg, 2)] {
            let offset = bytes.len() as u32;
            match cx.constant(e) {
                Some(v) => bytes.extend_from_slice(&v.to_le_bytes()[..width as usize]),
                None => {
                    bytes.extend(std::iter::repeat_n(0u8, width as usize));
                    fixups.push(Fixup {
                        offset,
                        expr: e,
                        kind: FixupKind::data(width).with_reloc(abi.abs(width).unwrap_or(0)),
                        span: cx.exprs.span(e),
                    });
                }
            }
        }
    }

    // ---- immediate --------------------------------------------------------
    // An `is4` register and a small immediate share one byte (XOP's
    // `vpermil2ps`): the register in the top nibble, the value in the bottom.
    let mut imm = roles.imm;
    if let (Some(r), Some((e, 1))) = (roles.is4, roles.imm) {
        let Some(v) = cx.constant(e).filter(|v| (0..=15).contains(v)) else {
            cx.error(
                cx.exprs.span(e),
                "this immediate shares its byte with a register and must be 0 to 15",
            );
            return None;
        };
        bytes.push(((r.num & 0xf) << 4) | v as u8);
        imm = None;
    }
    for (e, width) in imm.into_iter().chain(roles.imm2) {
        let offset = bytes.len() as u32;
        let folded = cx.constant(e);
        match folded {
            Some(v) => bytes.extend_from_slice(&v.to_le_bytes()[..width as usize]),
            None => {
                bytes.extend(std::iter::repeat_n(0u8, width as usize));
                // A 32-bit immediate in a 64-bit operation is sign-extended by
                // the CPU, so the linker must be told to range-check it as
                // signed rather than let it wrap.
                let sign_extended = def.opsize == 64 && width == 4 && def.enc == Enc::Legacy;
                let r = if def.flags & IMM64 != 0 {
                    abi.abs(8).unwrap_or(0)
                } else if sign_extended {
                    abi.abs32_signed()
                } else {
                    abi.abs(width).unwrap_or(0)
                };
                let mut kind = FixupKind::data(width).with_reloc(r);
                kind.signed = sign_extended;
                if sign_extended {
                    kind.class = RelocClass::SignExtended;
                }
                if abi == reloc::Abi::I386 && width == 4 && names_got(cx, e) {
                    kind = got_distance(offset);
                }
                fixups.push(Fixup {
                    offset,
                    expr: e,
                    kind,
                    span: cx.exprs.span(e),
                });
            }
        }
    }

    // `is4`: a whole immediate byte whose top nibble names a register.
    if let (Some(r), None) = (roles.is4, roles.imm) {
        bytes.push((r.num & 0xf) << 4);
    }

    // 3DNow! puts its opcode selector where an immediate would go.
    if let Some(s) = def.suffix {
        bytes.push(s);
    }

    // A displacement fixup can only be built now that the instruction length,
    // and therefore the RIP-relative bias, is known.
    if let Some((offset, e, dspan, rip_relative, width)) = disp_fixup {
        let trailing = (bytes.len() - offset - width as usize) as i8;
        let kind = if rip_relative {
            // A `movq` load through the GOT is one a linker may turn into a
            // `leaq` of the symbol itself, which Mach-O records in the
            // relocation's type; ELF's `R_X86_64_GOTPCREL` does not say.
            let class = if def.opcode == [0x8b]
                && def.opsize == 64
                && def.enc == Enc::Legacy
                && modifier(cx, e).as_deref() == Some("gotpcrel")
            {
                RelocClass::GotLoad
            } else {
                RelocClass::Plain
            };
            FixupKind::pcrel(4, trailing + 4)
                .with_reloc(abi.pcrel(4).unwrap_or(0))
                .with_class(class)
        } else if width != 4 {
            FixupKind::data(width).with_reloc(abi.abs(width).unwrap_or(0))
        } else if bits == 64
            && mem.as_ref().is_none_or(|m| m.addr_size == 8)
            && !(def.opcode == [0x8d] && def.opsize != 64)
        {
            // A 64-bit-mode displacement is sign-extended to the address
            // width, so the linker has to range-check it as signed. With
            // 32-bit addressing it is an unsigned address instead, and so is
            // the result of a `lea` into a 32-bit register.
            FixupKind::data(4)
                .with_reloc(abi.abs32_signed())
                .with_class(RelocClass::SignExtended)
        } else if bits == 64
            && abi == reloc::Abi::X86_64
            && mem.as_ref().is_none_or(|m| m.addr_size == 8)
        {
            // The CPU still sign-extends the `lea`'s displacement; only the
            // result is truncated.
            FixupKind::data(4)
                .with_reloc(abi.abs(4).unwrap_or(0))
                .with_class(RelocClass::SignExtended)
        } else if abi == reloc::Abi::I386 && names_got(cx, e) {
            got_distance(offset as u32)
        } else if abi == reloc::Abi::I386
            && modifier(cx, e).as_deref() == Some("got")
            && got_load_is_relaxable(def, mem.as_ref())
        {
            FixupKind::data(4).with_reloc(reloc::Abi::I386_GOT32X)
        } else {
            FixupKind::data(4).with_reloc(abi.abs(4).unwrap_or(0))
        };
        fixups.push(Fixup {
            offset: offset as u32,
            expr: e,
            kind,
            span: dspan,
        });
    }

    // ---- relative branch target -------------------------------------------
    if let Some((e, width)) = roles.rel {
        // The wide displacement is as wide as the operand size, which in
        // 16-bit mode is a word.
        let width = if width == 4 && bits == 16 && def.opsize == 0 {
            2
        } else {
            width
        };
        let offset = bytes.len() as u32;
        bytes.extend(std::iter::repeat_n(0u8, width as usize));
        // GNU as routes a plain 64-bit-mode call through the PLT but leaves a
        // 32-bit-mode one PC-relative, and that follows the mode rather than
        // the object: `.code32` inside an x86-64 object gets `R_X86_64_PC32`.
        // NASM emits a plain `R_X86_64_PC32` for every branch, reserving the
        // PLT for an explicit `wrt ..plt`.
        let nasm = cx.dialect == crate::lexer::Dialect::Nasm;
        let reloc = match width {
            4 if bits == 64 && !nasm => abi.plt32(),
            _ => abi.pcrel(width).unwrap_or(0),
        };
        fixups.push(Fixup {
            offset,
            expr: e,
            // The displacement is measured from the end of the instruction,
            // which is `width` bytes past the start of this field.
            kind: FixupKind::pcrel(width, width as i8)
                .with_reloc(reloc)
                .with_class(RelocClass::Branch),
            span: cx.exprs.span(e),
        });
    }

    Some(Variant { bytes, fixups })
}

/// The name of the `@` modifier in an expression, lowercased, if it has one.
pub fn modifier(cx: &AsmCtx<'_>, e: ExprRef) -> Option<String> {
    match &cx.exprs.get(e).kind {
        ExprKind::Modifier(n, _) => Some(cx.interner.get(*n).to_ascii_lowercase()),
        ExprKind::Unary(_, a) => modifier(cx, *a),
        ExprKind::Binary(_, a, b) => modifier(cx, *a).or_else(|| modifier(cx, *b)),
        _ => None,
    }
}

/// True if an expression refers to `_GLOBAL_OFFSET_TABLE_`.
fn names_got(cx: &AsmCtx<'_>, e: ExprRef) -> bool {
    match &cx.exprs.get(e).kind {
        ExprKind::Sym(n) => cx.interner.get(*n) == "_GLOBAL_OFFSET_TABLE_",
        ExprKind::Unary(_, a) => names_got(cx, *a),
        ExprKind::Binary(_, a, b) => names_got(cx, *a) || names_got(cx, *b),
        _ => false,
    }
}

/// A reference to `_GLOBAL_OFFSET_TABLE_` in i386 code, which GNU as and
/// llvm-mc both turn into the distance to the GOT from the start of the
/// instruction: `R_386_GOTPC`, whose addend makes up for the field being
/// `offset` bytes in. That is what makes `addl $_GLOBAL_OFFSET_TABLE_, %ebx`
/// after a `call`/`pop` pair load the GOT's address.
fn got_distance(offset: u32) -> FixupKind {
    FixupKind::pcrel(4, -(offset as i8))
        .with_reloc(reloc::Abi::I386_GOTPC)
        .linker_only()
}

/// True for the `@GOT` loads GNU as marks `R_386_GOT32X`, which a linker may
/// rewrite to use the symbol's address directly: a 32-bit `mov` load, the
/// arithmetic operations and `test` reading the pointer, and an indirect
/// `call`, `jmp` or `push` through it, all with a base register or no
/// register at all. llvm-mc marks only the `mov`.
fn got_load_is_relaxable(def: &Def, mem: Option<&Mem>) -> bool {
    let Some(m) = mem else {
        return false;
    };
    if m.base.is_none() && m.index.is_some() || def.enc != Enc::Legacy {
        return false;
    }
    match (def.opcode.as_slice(), def.modrm) {
        ([0xff], ModRm::Ext(2 | 4 | 6)) => true,
        ([op], ModRm::Reg) => {
            def.opsize == 32 && (*op == 0x8b || *op == 0x85 || (*op & 0xc7 == 0x03 && *op < 0x40))
        }
        _ => false,
    }
}

/// The number of the segment register an address uses by default: `ss` (2)
/// when the base is `sp` or `bp` in any width, `ds` (3) otherwise. `r12` and
/// `r13` share their low bits but not the default.
fn default_segment(m: &Mem) -> u8 {
    match m.base {
        Some(b) if !m.rip_relative && matches!(b.num, 4 | 5) => 2,
        _ => 3,
    }
}

/// Where a symbolic displacement goes: its offset, expression, span, whether
/// it is RIP-relative, and its width in bytes.
type DispFixup = (usize, ExprRef, Span, bool, u8);

/// `rep` and `lock`, which go after the operand-size prefix. VEX and EVEX
/// have no operand-size prefix, so there they follow the address size.
fn push_rep_lock(bytes: &mut Vec<u8>, prefixes: Prefixes) {
    if let Some(r) = prefixes.rep {
        bytes.push(r);
    }
    if prefixes.lock {
        bytes.push(0xf0);
    }
}

/// Rejects decorators the chosen encoding cannot carry.
fn check_decorators(
    cx: &mut AsmCtx<'_>,
    def: &Def,
    roles: &Roles<'_>,
    rounding: Option<(RoundCtl, Span)>,
    has_mem: bool,
    span: Span,
) -> Option<()> {
    let d = &roles.decor;
    if def.enc != Enc::Evex {
        if let Some((_, rspan)) = rounding {
            cx.error(
                rspan,
                "embedded rounding control is only available on AVX-512 forms",
            );
            return None;
        }
        if !d.is_empty() {
            cx.error(
                d.span,
                "operand decorators are only available on AVX-512 forms",
            );
            return None;
        }
        return Some(());
    }

    if let Some((ctl, rspan)) = rounding {
        if ctl.is_sae_only() {
            if def.flags & (EVEX_SAE | EVEX_ER) == 0 {
                cx.error(rspan, "this instruction does not take `{sae}`");
                return None;
            }
            // Rounding-capable instructions spell exception suppression with
            // an explicit mode; a bare `{sae}` is rejected there by both
            // reference assemblers.
            if def.flags & EVEX_SAE == 0 {
                cx.error(
                    rspan,
                    "this instruction takes a rounding mode such as `{rn-sae}`, not `{sae}`",
                );
                return None;
            }
        } else if def.flags & EVEX_ER == 0 {
            cx.error(rspan, "this instruction takes no embedded rounding control");
            return None;
        }
        if has_mem {
            cx.error(
                rspan,
                "embedded rounding control cannot be combined with a memory operand",
            );
            return None;
        }
    }
    if d.mask.is_some() && def.flags & NOMASK != 0 {
        cx.error(d.span, "this instruction takes no writemask");
        return None;
    }
    if d.mask.is_none() && def.flags & NEEDS_MASK != 0 {
        cx.error(
            span,
            "this instruction requires a writemask such as `{%k1}`",
        );
        return None;
    }
    if d.zeroing && d.mask.is_none() {
        cx.error(d.span, "`{z}` requires a writemask register");
        return None;
    }
    // A result written to an opmask has nothing for `{z}` to zero: the
    // writemask already clears the bits it leaves out.
    if d.zeroing && def.ops.first() == Some(&Op::V(Vk::K)) {
        cx.error(
            d.span,
            "`{z}` cannot be used with a mask register destination",
        );
        return None;
    }
    // Nor does a store to memory, or a gather, whose mask is its own
    // bookkeeping.
    let stores = matches!(
        def.ops.first(),
        Some(Op::M(_) | Op::Vm(..) | Op::Vsib(_) | Op::Rm(_))
    ) && roles.rm.is_some_and(|o| o.is_mem());
    if d.zeroing && (stores || def.flags & NEEDS_MASK != 0) {
        cx.error(d.span, "`{z}` cannot be used on this instruction");
        return None;
    }
    if let Some(b) = d.broadcast {
        let bspan = b.span;
        if !has_mem {
            cx.error(bspan, "a broadcast decorator needs a memory operand");
            return None;
        }
        let Some(n) = def.broadcast_count() else {
            cx.error(bspan, "this instruction does not support broadcast");
            return None;
        };
        // N is redundant — it is the register's element count — so it is
        // recomputed and the source's spelling checked against it.
        if b.count != n {
            cx.error(bspan, format!("this operand broadcasts as `{{1to{n}}}`"));
            return None;
        }
    }
    Some(())
}

/// Checks that a VSIB pattern got a vector index and that nothing else did.
fn check_vsib(cx: &mut AsmCtx<'_>, def: &Def, mem: Option<&Mem>, span: Span) -> Option<()> {
    let want: Option<Vk> = def.ops.iter().find_map(|o| match o {
        Op::Vsib(k) => Some(*k),
        _ => None,
    });
    let index = mem.and_then(|m| m.index);
    match (want, index) {
        (Some(k), Some(i)) if k.accepts(i) => Some(()),
        (Some(_), _) => {
            cx.error(
                mem.map_or(span, |m| m.span),
                "this instruction needs a vector index register",
            );
            None
        }
        (None, Some(i)) if i.is_vector() => {
            cx.error(
                mem.map_or(span, |m| m.span),
                format!(
                    "`{}` can only index memory in a gather or scatter",
                    reg::name_of(i)
                ),
            );
            None
        }
        _ => Some(()),
    }
}

/// Emits the ModRM byte plus any SIB and displacement.
#[allow(clippy::too_many_arguments)]
fn encode_rm(
    cx: &mut AsmCtx<'_>,
    bits: u8,
    bytes: &mut Vec<u8>,
    disp_fixup: &mut Option<DispFixup>,
    reg_field: u8,
    rm_operand: &Operand,
    mem: Option<&Mem>,
    disp_scale: u32,
    force_sib: bool,
) -> Option<()> {
    // Register direct.
    if let Some(r) = rm_register(rm_operand) {
        bytes.push(0xc0 | ((reg_field & 7) << 3) | (r.num & 7));
        return Some(());
    }

    let Some(m) = mem else {
        cx.error(
            rm_operand.span,
            format!(
                "expected a register or memory operand, found {}",
                rm_operand.describe()
            ),
        );
        return None;
    };

    // RIP-relative: mod=00, rm=101, always a 32-bit displacement.
    if m.rip_relative {
        if force_sib {
            cx.error(
                m.span,
                "this instruction cannot address memory relative to `rip`",
            );
            return None;
        }
        if bits != 64 {
            cx.error(m.span, "RIP-relative addressing requires 64-bit mode");
            return None;
        }
        if m.base.is_some() || m.index.is_some() {
            cx.error(
                m.span,
                "RIP-relative addressing cannot be combined with other registers",
            );
            return None;
        }
        bytes.push(((reg_field & 7) << 3) | 0b101);
        let at = bytes.len();
        bytes.extend_from_slice(&[0; 4]);
        // A constant here is the displacement itself: `2(%rip)` addresses two
        // bytes past the next instruction. Only a symbolic displacement is
        // turned into "distance from here to that symbol".
        match m.disp {
            None => {}
            Some(e) => match cx.constant(e) {
                Some(v) => {
                    if i32::try_from(v).is_err() {
                        cx.error(m.span, format!("displacement {v} does not fit in 32 bits"));
                        return None;
                    }
                    bytes[at..at + 4].copy_from_slice(&(v as i32).to_le_bytes());
                }
                None => *disp_fixup = Some((at, e, m.span, true, 4)),
            },
        }
        return Some(());
    }

    // With 32-bit addressing a displacement that fits 32 bits is read as
    // signed, so `-1` and `0xffffffff` are one and the same byte.
    let disp_const = m.disp.and_then(|e| cx.constant(e)).map(|v| {
        if m.addr_size == 4 && (-(1 << 31)..=0xffff_ffff).contains(&v) {
            v as i32 as i64
        } else {
            v
        }
    });
    let has_disp = m.disp.is_some();
    let symbolic_disp = has_disp && disp_const.is_none();

    if m.addr_size == 2 {
        return encode_rm16(cx, bytes, disp_fixup, reg_field, m, disp_const);
    }

    // No base and no index: an absolute address.
    if m.base.is_none() && m.index.is_none() {
        if bits == 64 {
            // 64-bit mode has no ModRM form for a bare disp32, so the SIB
            // escape with no base and no index is used instead.
            bytes.push(((reg_field & 7) << 3) | 0b100);
            bytes.push((0b100 << 3) | 0b101);
        } else {
            bytes.push(((reg_field & 7) << 3) | 0b101);
        }
        push_disp32(cx, bytes, disp_fixup, m, disp_const);
        return Some(());
    }

    let base = m.base;
    let base_low = base.map_or(0, |b| b.num & 7);
    // rsp/r12 as a base always needs SIB; rbp/r13 always needs a displacement.
    // A VSIB index also forces SIB, since that is where it lives.
    let need_sib = force_sib || m.index.is_some() || base.is_none() || base_low == 0b100;
    let base_forces_disp = base.is_some() && base_low == 0b101;

    // index-only addressing encodes disp32 with mod=00.
    let disp_size: u8 = if symbolic_disp || base.is_none() {
        4
    } else {
        let v = disp_const.unwrap_or(0);
        // Under EVEX a one-byte displacement is stored pre-divided by the size
        // of the access, so it is only usable when the value divides exactly.
        let n = disp_scale as i64;
        let fits8 = v % n == 0 && (-128..=127).contains(&(v / n));
        match () {
            _ if v == 0 && !base_forces_disp => 0,
            _ if fits8 => 1,
            _ => 4,
        }
    };

    let mod_bits = if base.is_none() {
        0b00
    } else {
        match disp_size {
            0 => 0b00,
            1 => 0b01,
            _ => 0b10,
        }
    };

    if need_sib {
        bytes.push((mod_bits << 6) | ((reg_field & 7) << 3) | 0b100);
        let scale_bits = match m.scale {
            1 => 0,
            2 => 1,
            4 => 2,
            8 => 3,
            s => {
                cx.error(m.span, format!("invalid scale {s}"));
                return None;
            }
        };
        let index_bits = match m.index {
            Some(i) => i.num & 7,
            None => 0b100, // no index
        };
        let base_bits = match base {
            Some(b) => b.num & 7,
            None => 0b101, // no base; disp32 follows
        };
        bytes.push((scale_bits << 6) | (index_bits << 3) | base_bits);
    } else {
        bytes.push((mod_bits << 6) | ((reg_field & 7) << 3) | base_low);
    }

    match disp_size {
        0 => {}
        1 => bytes.push((disp_const.unwrap_or(0) / disp_scale as i64) as u8),
        _ => push_disp32(cx, bytes, disp_fixup, m, disp_const),
    }
    Some(())
}

/// ModRM for 16-bit addressing, which has no SIB byte: `rm` names one of
/// eight fixed base and index combinations.
fn encode_rm16(
    cx: &mut AsmCtx<'_>,
    bytes: &mut Vec<u8>,
    disp_fixup: &mut Option<DispFixup>,
    reg_field: u8,
    m: &Mem,
    disp_const: Option<i64>,
) -> Option<()> {
    let reg = (reg_field & 7) << 3;
    // Numbers of the registers involved: bx 3, bp 5, si 6, di 7.
    let rm = match (m.base.map(|r| r.num), m.index.map(|r| r.num)) {
        (None, None) => {
            // A bare 16-bit displacement.
            bytes.push(reg | 0b110);
            push_disp(cx, bytes, disp_fixup, m, disp_const, 2);
            return Some(());
        }
        (Some(3), Some(6)) => 0b000,
        (Some(3), Some(7)) => 0b001,
        (Some(5), Some(6)) => 0b010,
        (Some(5), Some(7)) => 0b011,
        (Some(6), None) => 0b100,
        (Some(7), None) => 0b101,
        (Some(5), None) => 0b110,
        (Some(3), None) => 0b111,
        _ => {
            cx.error(
                m.span,
                "16-bit addressing takes `bx` or `bp` and `si` or `di`, one or both",
            );
            return None;
        }
    };
    if m.scale != 1 {
        cx.error(m.span, "16-bit addressing has no scale factor");
        return None;
    }
    // A displacement that fits 16 bits is read as a signed word, so `0xffff`
    // is the byte -1; a wider one is truncated to a full word, as GNU as
    // does with a warning. Like an immediate, anything that fits 32 bits is
    // first read as a signed 32-bit value, so `0xffffffff` is -1 as well.
    // `bp` alone has no form without one.
    let disp_const = disp_const.map(|v| {
        if (-(1 << 31)..=0xffff_ffff).contains(&v) {
            v as i32 as i64
        } else {
            v
        }
    });
    let disp = match disp_const {
        None if m.disp.is_some() => 2,
        Some(v) if !(-0x8000..=0xffff).contains(&v) => 2,
        _ => {
            let v = disp_const.unwrap_or(0) as i16 as i64;
            if v == 0 && rm != 0b110 {
                0
            } else if (-128..=127).contains(&v) {
                1
            } else {
                2
            }
        }
    };
    let mod_bits = [0b00, 0b01, 0b10][disp as usize];
    bytes.push((mod_bits << 6) | reg | rm);
    match disp {
        0 => {}
        1 => bytes.push(disp_const.unwrap_or(0) as u8),
        _ => push_disp(cx, bytes, disp_fixup, m, disp_const, 2),
    }
    Some(())
}

fn push_disp32(
    cx: &mut AsmCtx<'_>,
    bytes: &mut Vec<u8>,
    disp_fixup: &mut Option<DispFixup>,
    m: &Mem,
    disp_const: Option<i64>,
) {
    push_disp(cx, bytes, disp_fixup, m, disp_const, 4);
}

/// A displacement of `width` bytes, or a fixup for one.
fn push_disp(
    cx: &mut AsmCtx<'_>,
    bytes: &mut Vec<u8>,
    disp_fixup: &mut Option<DispFixup>,
    m: &Mem,
    disp_const: Option<i64>,
    width: u8,
) {
    match disp_const {
        Some(v) => bytes.extend_from_slice(&v.to_le_bytes()[..width as usize]),
        None => {
            let at = bytes.len();
            bytes.extend(std::iter::repeat_n(0u8, width as usize));
            let e = m.disp.unwrap_or_else(|| cx.exprs.int(0, m.span));
            *disp_fixup = Some((at, e, m.span, false, width));
        }
    }
}

/// The canonical multi-byte no-ops recommended by both vendors, indexed by
/// length. Padding with these keeps alignment padding executable and cheap.
///
/// The exact split differs from GNU as, which varies it by `-mtune`; any
/// sequence of no-ops of the right total length is correct.
pub fn nop_bytes(bits: u8, len: usize) -> Vec<u8> {
    // The long forms are `0f 1f`, which predates neither 16-bit mode nor the
    // pre-P6 processors that 16-bit code is usually written for.
    if bits < 32 {
        return vec![0x90; len];
    }
    #[rustfmt::skip]
    const NOPS: [&[u8]; 12] = [
        &[],
        &[0x90],
        &[0x66, 0x90],
        &[0x0f, 0x1f, 0x00],
        &[0x0f, 0x1f, 0x40, 0x00],
        &[0x0f, 0x1f, 0x44, 0x00, 0x00],
        &[0x66, 0x0f, 0x1f, 0x44, 0x00, 0x00],
        &[0x0f, 0x1f, 0x80, 0x00, 0x00, 0x00, 0x00],
        &[0x0f, 0x1f, 0x84, 0x00, 0x00, 0x00, 0x00, 0x00],
        &[0x66, 0x0f, 0x1f, 0x84, 0x00, 0x00, 0x00, 0x00, 0x00],
        &[0x66, 0x66, 0x0f, 0x1f, 0x84, 0x00, 0x00, 0x00, 0x00, 0x00],
        &[0x66, 0x66, 0x66, 0x0f, 0x1f, 0x84, 0x00, 0x00, 0x00, 0x00, 0x00],
    ];
    let mut out = Vec::with_capacity(len);
    let mut left = len;
    while left > 0 {
        let take = left.min(NOPS.len() - 1);
        out.extend_from_slice(NOPS[take]);
        left -= take;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::insn::Tuple;
    use super::*;

    #[test]
    fn nop_padding_has_the_requested_length() {
        for bits in [16u8, 32, 64] {
            for len in 0..64 {
                assert_eq!(nop_bytes(bits, len).len(), len, "bits={bits} len={len}");
            }
        }
    }

    #[test]
    fn sixteen_bit_mode_uses_only_the_one_byte_nop() {
        assert_eq!(nop_bytes(16, 3), vec![0x90, 0x90, 0x90]);
    }

    #[test]
    fn tuple_scales_match_the_manual() {
        // Full vector: the whole register, or one element under broadcast.
        assert_eq!(Tuple::Fv.scale(64, false, false), Some(64));
        assert_eq!(Tuple::Fv.scale(32, false, false), Some(32));
        assert_eq!(Tuple::Fv.scale(64, false, true), Some(4));
        assert_eq!(Tuple::Fv.scale(64, true, true), Some(8));
        // Scalars scale by their element, whatever the vector length.
        assert_eq!(Tuple::T1s.scale(64, false, false), Some(4));
        assert_eq!(Tuple::T1s.scale(16, true, false), Some(8));
        // The fractional-memory tuples follow the vector length.
        assert_eq!(Tuple::Hvm.scale(64, false, false), Some(32));
        assert_eq!(Tuple::Qvm.scale(64, false, false), Some(16));
        assert_eq!(Tuple::Ovm.scale(64, false, false), Some(8));
    }
}
