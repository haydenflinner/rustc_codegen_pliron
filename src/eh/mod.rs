//! `.eh_frame` + `.gcc_except_table` (LSDA) emission for Cranelift-compiled
//! functions. Adapted from rustc_codegen_cranelift's `debuginfo/unwind.rs`
//! (MIT/Apache-2.0).

mod gcc_except_table;

use cranelift_codegen::ir::{AbiParam, Endianness, Signature, types};
use cranelift_codegen::isa::unwind::UnwindInfo;
use cranelift_codegen::{Context, FinalizedMachExceptionHandler};
use cranelift_module::{DataDescription, DataId, FuncId, FuncOrDataId, Linkage, Module};
use cranelift_object::{ObjectModule, ObjectProduct};
use gcc_except_table::*;
use gimli::write::{Address, CieId, EhFrame, EndianVec, FrameTable, Result, Writer};
use gimli::{Encoding, Format, RunTimeEndian, SectionId};
use object::write::{Relocation, StandardSection};
use object::{RelocationEncoding, RelocationFlags};

pub const EXCEPTION_HANDLER_CLEANUP: u32 = 0;
pub const EXCEPTION_HANDLER_CATCH: u32 = 1;

fn address_for_func(id: FuncId) -> Address {
    Address::Symbol { symbol: id.as_u32() as usize, addend: 0 }
}

fn address_for_data(id: DataId) -> Address {
    Address::Symbol { symbol: (id.as_u32() | 1 << 31) as usize, addend: 0 }
}

pub struct UnwindContext {
    endian: RunTimeEndian,
    frame_table: FrameTable,
    cie_id: Option<CieId>,
    lsda: bool,
}

impl UnwindContext {
    /// `unwind`: reference `rust_eh_personality` and emit LSDAs (panic=unwind).
    pub fn new(module: &mut ObjectModule, pic: bool, unwind: bool) -> Self {
        let endian = match module.isa().endianness() {
            Endianness::Little => RunTimeEndian::Little,
            Endianness::Big => RunTimeEndian::Big,
        };
        let mut frame_table = FrameTable::default();
        let cie_id = module.isa().create_systemv_cie().map(|mut cie| {
            let ptr_enc = if pic {
                gimli::DwEhPe(gimli::DW_EH_PE_pcrel.0 | gimli::DW_EH_PE_sdata4.0)
            } else {
                gimli::DW_EH_PE_absptr
            };
            cie.fde_address_encoding = ptr_enc;
            if unwind {
                let code_enc = if pic {
                    let fmt = if module.isa().triple().architecture == target_lexicon::Architecture::X86_64 {
                        gimli::DW_EH_PE_sdata4
                    } else {
                        gimli::DW_EH_PE_sdata8
                    };
                    gimli::DwEhPe(gimli::DW_EH_PE_indirect.0 | gimli::DW_EH_PE_pcrel.0 | fmt.0)
                } else {
                    gimli::DwEhPe(gimli::DW_EH_PE_indirect.0 | gimli::DW_EH_PE_absptr.0)
                };
                cie.lsda_encoding = Some(ptr_enc);
                let pt = module.target_config().pointer_type();
                let personality = match module.get_name("rust_eh_personality") {
                    Some(FuncOrDataId::Func(id)) => id,
                    _ => {
                        let sig = Signature {
                            params: vec![
                                AbiParam::new(types::I32),
                                AbiParam::new(types::I32),
                                AbiParam::new(types::I64),
                                AbiParam::new(pt),
                                AbiParam::new(pt),
                            ],
                            returns: vec![AbiParam::new(types::I32)],
                            call_conv: module.target_config().default_call_conv,
                        };
                        module.declare_function("rust_eh_personality", Linkage::Import, &sig).unwrap()
                    }
                };
                // Indirect so the personality may live in another DSO.
                let pref = module.declare_data("DW.ref.rust_eh_personality", Linkage::Local, false, false).unwrap();
                let mut d = DataDescription::new();
                // Must not be zero-init: the unwinder can't handle it in .bss.
                d.define(vec![0; pt.bytes() as usize].into_boxed_slice());
                let fr = module.declare_func_in_data(personality, &mut d);
                d.write_function_addr(0, fr);
                module.define_data(pref, &d).unwrap();
                cie.personality = Some((code_enc, address_for_data(pref)));
            }
            frame_table.add_cie(cie)
        });
        UnwindContext { endian, frame_table, cie_id, lsda: unwind }
    }

    pub fn add_function(&mut self, module: &mut ObjectModule, func_id: FuncId, context: &Context) {
        let code = context.compiled_code().unwrap();
        let Some(UnwindInfo::SystemV(info)) = code.create_unwind_info(module.isa()).unwrap() else {
            return;
        };
        let mut fde = info.to_fde(address_for_func(func_id));
        if self.lsda {
            let lsda = module.declare_anonymous_data(false, false).unwrap();
            let encoding = Encoding {
                format: Format::Dwarf32,
                version: 1,
                address_size: module.isa().frontend_config().pointer_bytes(),
            };
            let mut t = GccExceptTable {
                call_sites: CallSiteTable(vec![]),
                actions: ActionTable::new(),
                type_info: TypeInfoTable::new(gimli::DW_EH_PE_udata4),
            };
            let catch_type = t.type_info.add(Address::Constant(0));
            let catch_action = t.actions.add(Action { kind: ActionKind::Catch(catch_type), next_action: None });
            for cs in code.buffer.call_sites() {
                let start = u64::from(cs.ret_addr - 1);
                if cs.exception_handlers.is_empty() {
                    t.call_sites.0.push(CallSite { start, length: 1, landing_pad: 0, action_entry: None });
                }
                for &h in cs.exception_handlers {
                    let FinalizedMachExceptionHandler::Tag(tag, lp) = h else { unreachable!() };
                    let action_entry = match tag.as_u32() {
                        EXCEPTION_HANDLER_CLEANUP => None,
                        EXCEPTION_HANDLER_CATCH => Some(catch_action),
                        _ => unreachable!(),
                    };
                    t.call_sites.0.push(CallSite { start, length: 1, landing_pad: u64::from(lp), action_entry });
                }
            }
            let mut w = WriterRelocate::new(self.endian);
            t.write(&mut w, encoding).unwrap();
            let mut data = DataDescription::new();
            data.define(w.writer.into_vec().into_boxed_slice());
            data.set_custom_section(".gcc_except_table");
            for r in &w.relocs {
                let DebugRelocName::Symbol(id) = r.name else { unreachable!() };
                let id = id as u32;
                if id & 1 << 31 == 0 {
                    let fr = module.declare_func_in_data(FuncId::from_u32(id), &mut data);
                    data.write_function_addr(r.offset, fr);
                } else {
                    let gv = module.declare_data_in_data(DataId::from_u32(id & !(1 << 31)), &mut data);
                    data.write_data_addr(r.offset, gv, 0);
                }
            }
            module.define_data(lsda, &data).unwrap();
            fde.lsda = Some(address_for_data(lsda));
        }
        self.frame_table.add_fde(self.cie_id.unwrap(), fde);
    }

    pub fn emit(self, product: &mut ObjectProduct) {
        let mut eh = EhFrame::from(WriterRelocate::new(self.endian));
        self.frame_table.write_eh_frame(&mut eh).unwrap();
        let w = eh.0;
        if w.writer.slice().is_empty() {
            return;
        }
        let sec = product.object.section_id(StandardSection::EhFrame);
        product.object.section_mut(sec).set_data(w.writer.into_vec(), 8);
        for r in &w.relocs {
            let (symbol, off) = match r.name {
                DebugRelocName::Section(_) => (product.object.section_symbol(sec), 0),
                DebugRelocName::Symbol(id) => {
                    let id = id as u32;
                    let s = if id & 1 << 31 == 0 {
                        product.function_symbol(FuncId::from_u32(id))
                    } else {
                        product.data_symbol(DataId::from_u32(id & !(1 << 31)))
                    };
                    product.object.symbol_section_and_offset(s).unwrap_or((s, 0))
                }
            };
            product
                .object
                .add_relocation(
                    sec,
                    Relocation {
                        offset: u64::from(r.offset),
                        symbol,
                        flags: RelocationFlags::Generic {
                            kind: r.kind,
                            encoding: RelocationEncoding::Generic,
                            size: r.size * 8,
                        },
                        addend: off as i64 + r.addend,
                    },
                )
                .unwrap();
        }
    }
}

#[derive(Clone)]
struct DebugReloc {
    offset: u32,
    size: u8,
    name: DebugRelocName,
    addend: i64,
    kind: object::RelocationKind,
}

#[derive(Clone)]
enum DebugRelocName {
    Section(#[allow(dead_code)] SectionId),
    Symbol(usize),
}

/// A gimli [`Writer`] that records relocations.
#[derive(Clone)]
struct WriterRelocate {
    relocs: Vec<DebugReloc>,
    writer: EndianVec<RunTimeEndian>,
}

impl WriterRelocate {
    fn new(endian: RunTimeEndian) -> Self {
        WriterRelocate { relocs: Vec::new(), writer: EndianVec::new(endian) }
    }
    fn reloc(&mut self, offset: usize, size: u8, name: DebugRelocName, addend: i64, kind: object::RelocationKind) {
        self.relocs.push(DebugReloc { offset: offset as u32, size, name, addend, kind });
    }
}

impl Writer for WriterRelocate {
    type Endian = RunTimeEndian;
    fn endian(&self) -> Self::Endian {
        self.writer.endian()
    }
    fn len(&self) -> usize {
        self.writer.len()
    }
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write(bytes)
    }
    fn write_at(&mut self, offset: usize, bytes: &[u8]) -> Result<()> {
        self.writer.write_at(offset, bytes)
    }
    fn write_address(&mut self, address: Address, size: u8) -> Result<()> {
        match address {
            Address::Constant(val) => self.write_udata(val, size),
            Address::Symbol { symbol, addend } => {
                self.reloc(self.len(), size, DebugRelocName::Symbol(symbol), addend, object::RelocationKind::Absolute);
                self.write_udata(0, size)
            }
        }
    }
    fn write_offset(&mut self, val: usize, section: SectionId, size: u8) -> Result<()> {
        self.reloc(self.len(), size, DebugRelocName::Section(section), val as i64, object::RelocationKind::Absolute);
        self.write_udata(0, size)
    }
    fn write_offset_at(&mut self, offset: usize, val: usize, section: SectionId, size: u8) -> Result<()> {
        self.reloc(offset, size, DebugRelocName::Section(section), val as i64, object::RelocationKind::Absolute);
        self.write_udata_at(offset, 0, size)
    }
    fn write_eh_pointer(&mut self, address: Address, eh_pe: gimli::DwEhPe, size: u8) -> Result<()> {
        match address {
            Address::Constant(val) => {
                let val = match eh_pe.application() {
                    gimli::DW_EH_PE_absptr => val,
                    gimli::DW_EH_PE_pcrel => (self.len() as u64).wrapping_sub(val),
                    _ => return Err(gimli::write::Error::UnsupportedPointerEncoding(eh_pe)),
                };
                self.write_eh_pointer_data(val, eh_pe.format(), size)
            }
            Address::Symbol { symbol, addend } => match eh_pe.application() {
                gimli::DW_EH_PE_pcrel => {
                    let size = match eh_pe.format() {
                        gimli::DW_EH_PE_sdata4 => 4,
                        gimli::DW_EH_PE_sdata8 => 8,
                        _ => return Err(gimli::write::Error::UnsupportedPointerEncoding(eh_pe)),
                    };
                    self.reloc(self.len(), size, DebugRelocName::Symbol(symbol), addend, object::RelocationKind::Relative);
                    self.write_udata(0, size)
                }
                gimli::DW_EH_PE_absptr => {
                    self.reloc(self.len(), size, DebugRelocName::Symbol(symbol), addend, object::RelocationKind::Absolute);
                    self.write_udata(0, size)
                }
                _ => Err(gimli::write::Error::UnsupportedPointerEncoding(eh_pe)),
            },
        }
    }
}
