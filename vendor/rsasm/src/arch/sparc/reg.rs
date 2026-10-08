//! SPARC register names.
//!
//! The 32 integer registers have two spellings. `%r0`-`%r31` is the flat one;
//! the usual one names the four windows the register file is divided into —
//! `%g` globals, `%o` outgoing, `%l` locals, `%i` incoming — eight each, in
//! that order. `save` rotates the window so the caller's `%o` registers become
//! the callee's `%i` registers, which is why the two spellings must agree on
//! the numbering.

/// What a register can be used for. SPARC keeps these in separate name
/// spaces, so `%f1` and `%r1` are unrelated.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RegClass {
    /// `%g0`-`%i7`, encoded as 0-31.
    Int,
    /// `%f0`-`%f31`.
    Float,
    /// `%icc` (0) and `%xcc` (2): the integer condition codes a V9 predicted
    /// branch or conditional move selects between.
    Icc,
    /// `%fcc0`-`%fcc3`.
    Fcc,
    /// Ancillary state registers, reached through `rd`/`wr`. `%y` is ASR 0.
    Asr,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Reg {
    pub class: RegClass,
    /// Field value, already in encoding order.
    pub num: u8,
}

impl Reg {
    pub fn is_int(&self) -> bool {
        self.class == RegClass::Int
    }

    pub fn is_float(&self) -> bool {
        self.class == RegClass::Float
    }
}

/// `%g0`, the register that reads as zero and discards writes. Half of SPARC's
/// synthetic instructions are some real instruction with `%g0` in one slot.
pub const G0: Reg = Reg {
    class: RegClass::Int,
    num: 0,
};

/// `%o7`, where `call` leaves the return address.
pub const O7: Reg = Reg {
    class: RegClass::Int,
    num: 15,
};

/// `%i7`, the return address in the caller's window after `save`.
pub const I7: Reg = Reg {
    class: RegClass::Int,
    num: 31,
};

/// Parses `<prefix><decimal>` and returns the number if it is within `max`.
///
/// Leading zeros are rejected so that `%g00` is not silently `%g0`.
fn indexed(name: &str, prefix: &str, max: u8) -> Option<u8> {
    let digits = name.strip_prefix(prefix)?;
    if digits.is_empty() || (digits.len() > 1 && digits.starts_with('0')) {
        return None;
    }
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u16 = digits.parse().ok()?;
    (n <= max as u16).then_some(n as u8)
}

/// Looks up a register by its name without the `%` sigil, lowercased.
pub fn lookup(name: &str) -> Option<Reg> {
    let int = |num: u8| {
        Some(Reg {
            class: RegClass::Int,
            num,
        })
    };
    match name {
        // The stack and frame pointers are the two windowed registers with
        // names of their own: `%sp` is `%o6` and, after `save`, the same
        // physical register is the callee's `%fp` = `%i6`.
        "sp" => return int(14),
        "fp" => return int(30),
        "icc" => {
            return Some(Reg {
                class: RegClass::Icc,
                num: 0,
            });
        }
        "xcc" => {
            return Some(Reg {
                class: RegClass::Icc,
                num: 2,
            });
        }
        "y" => {
            return Some(Reg {
                class: RegClass::Asr,
                num: 0,
            });
        }
        _ => {}
    }
    if let Some(n) = indexed(name, "g", 7) {
        return int(n);
    }
    if let Some(n) = indexed(name, "o", 7) {
        return int(8 + n);
    }
    if let Some(n) = indexed(name, "l", 7) {
        return int(16 + n);
    }
    if let Some(n) = indexed(name, "i", 7) {
        return int(24 + n);
    }
    if let Some(n) = indexed(name, "r", 31) {
        return int(n);
    }
    if let Some(n) = indexed(name, "f", 31) {
        return Some(Reg {
            class: RegClass::Float,
            num: n,
        });
    }
    if let Some(n) = indexed(name, "fcc", 3) {
        return Some(Reg {
            class: RegClass::Fcc,
            num: n,
        });
    }
    // ASR 0 is `%y`; 1-6 are reserved but assemblers still let them be named.
    if let Some(n) = indexed(name, "asr", 31) {
        return Some(Reg {
            class: RegClass::Asr,
            num: n,
        });
    }
    None
}

/// A register's canonical spelling, for diagnostics.
pub fn name_of(r: Reg) -> String {
    match r.class {
        RegClass::Int => {
            let (letter, n) = match r.num {
                0..=7 => ('g', r.num),
                8..=15 => ('o', r.num - 8),
                16..=23 => ('l', r.num - 16),
                _ => ('i', r.num.saturating_sub(24)),
            };
            format!("%{letter}{n}")
        }
        RegClass::Float => format!("%f{}", r.num),
        RegClass::Icc => if r.num == 0 { "%icc" } else { "%xcc" }.to_string(),
        RegClass::Fcc => format!("%fcc{}", r.num),
        RegClass::Asr => {
            if r.num == 0 {
                "%y".to_string()
            } else {
                format!("%asr{}", r.num)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_windowed_and_flat_spellings_agree() {
        for (windowed, flat) in [("g0", "r0"), ("o0", "r8"), ("l0", "r16"), ("i7", "r31")] {
            assert_eq!(lookup(windowed), lookup(flat), "{windowed} vs {flat}");
        }
        assert_eq!(lookup("sp"), lookup("o6"));
        assert_eq!(lookup("fp"), lookup("i6"));
    }

    #[test]
    fn out_of_range_and_malformed_names_are_rejected() {
        for bad in [
            "g8", "o8", "r32", "f32", "fcc4", "asr32", "g", "g00", "g-1", "gx", "",
        ] {
            assert_eq!(lookup(bad), None, "`{bad}` should not be a register");
        }
    }
}
