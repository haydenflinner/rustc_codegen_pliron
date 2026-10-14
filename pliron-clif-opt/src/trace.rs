//! Tracing callsites for names only known at runtime.
//!
//! `tracing`'s `span!` macros need a literal span name, but pipeline step
//! names live in the `Step` table. Step names are `&'static str`, so each
//! one gets a lazily-registered [`Callsite`] — the step's real name is the
//! span name (`INFO slp: ... close time.busy=...` rather than an opaque
//! `step{name="slp"}`), and a filtered-out callsite reports
//! `Interest::never` so `step_span` costs a hash lookup and an atomic load.
//!
//! Span-field matchers (`PLIRON_TRACE='...[{name="slp"}]'`) only gate
//! *events nested inside* a matching span — they cannot suppress the
//! span's own close line, which is why the callsite-per-name route exists.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use tracing::Span;
use tracing_core::callsite::{self, Callsite};
use tracing_core::field::{FieldSet, Value};
use tracing_core::metadata::Kind;
use tracing_core::subscriber::Interest;
use tracing_core::{Level, Metadata};
use rustc_hash::FxHashMap;

const NEVER: u8 = 0;
const SOMETIMES: u8 = 1;
const ALWAYS: u8 = 2;

/// [`DefaultCallsite`](tracing_core::callsite::DefaultCallsite) can't be
/// built at runtime — its `Metadata` needs the callsite's own `Identifier`
/// before the callsite exists. This type breaks the cycle: it exists as an
/// empty shell first (serving as the `Identifier` target), then its
/// `Metadata` is filled in before [`callsite::register`].
struct DynCallsite {
    interest: AtomicU8,
    meta: OnceLock<&'static Metadata<'static>>,
}

impl Callsite for DynCallsite {
    fn set_interest(&self, i: Interest) {
        let v = if i.is_never() {
            NEVER
        } else if i.is_always() {
            ALWAYS
        } else {
            SOMETIMES
        };
        self.interest.store(v, Ordering::Release);
    }
    fn metadata(&self) -> &'static Metadata<'static> {
        self.meta.get().expect("callsite used before meta set")
    }
}

pub struct DynSite {
    cs: &'static DynCallsite,
    meta: &'static Metadata<'static>,
}

/// One lazily-built callsite per `&'static str` name. `target` is the
/// span's tracing target (e.g. `pliron::step`) and `fields` the declared
/// field names; the leaked callsite+metadata live for the process, so
/// callers must pass compile-time-stable names, not arbitrary strings.
pub fn dyn_site(target: &'static str, fields: &'static [&'static str], name: &'static str) -> &'static DynSite {
    static MAP: Mutex<Option<FxHashMap<(&'static str, &'static str), &'static DynSite>>> =
        Mutex::new(None);
    let key = (target, name);
    let mut g = MAP.lock().unwrap();
    g.get_or_insert_default().entry(key).or_insert_with(|| {
        let cs: &'static DynCallsite = Box::leak(Box::new(DynCallsite {
            interest: AtomicU8::new(SOMETIMES),
            meta: OnceLock::new(),
        }));
        let fields = FieldSet::new(fields, callsite::Identifier(cs));
        let meta: &'static Metadata<'static> = Box::leak(Box::new(Metadata::new(
            name,
            target,
            Level::INFO,
            None,
            None,
            None,
            fields,
            Kind::SPAN,
        )));
        cs.meta.set(meta).ok().unwrap();
        callsite::register(cs);
        Box::leak(Box::new(DynSite { cs, meta }))
    })
}

impl DynSite {
    fn enabled(&self) -> bool {
        !matches!(self.cs.interest.load(Ordering::Acquire), NEVER)
    }

    /// A span named `site`'s name with `field` recorded. `Span::none` when
    /// the callsite's cached interest is `never` — no registry traffic at
    /// all on the filtered-out path.
    pub fn span_with(&self, field: &str, value: &str) -> Span {
        if !self.enabled() {
            return Span::none();
        }
        let Some(f) = self.meta.fields().field(field) else {
            return Span::new(self.meta, &self.meta.fields().value_set(&[]));
        };
        let vals = [(&f, Some(&value as &dyn Value))];
        let vs = self.meta.fields().value_set(&vals);
        Span::new(self.meta, &vs)
    }
}
