// Public-API coverage for the context-interned attribute store:
// `AttributeDict` values are `AttrInstId` handles into `Context`'s
// arena, deduplicated on `hash_attr`/`eq_attr`; interface queries use
// the per-entry interface list; interned attrs are immutable so
// mutation paths (given names) are copy-on-write.

use pliron::{
    attribute::AttributeDict,
    builtin::{
        attr_interfaces::{OutlinedAttr, TypedAttrInterface},
        attributes::{StringAttr, TypeAttr},
        given_names::{
            get_operation_result_name, remove_operation_result_name,
            set_operation_result_name,
        },
        ops::ModuleOp,
        types::{IntegerType, Signedness},
    },
    context::{Context, Ptr},
    ident,
    irfmt::parsers::spaced,
    op::Op,
    operation::Operation,
    parsable::{Parsable, parse_from_str},
    printable::Printable,
};

fn attr_key() -> pliron::identifier::Identifier {
    ident!("test_key")
}

fn string_of(attr: &StringAttr) -> String {
    attr.clone().into()
}

#[test]
fn equal_attrs_share_one_interned_entry() {
    let ctx = &Context::new();
    let key = attr_key();

    let mut d1 = AttributeDict::default();
    let mut d2 = AttributeDict::default();
    d1.set(ctx, key.clone(), StringAttr::new("same".into()));
    d2.set(ctx, key.clone(), StringAttr::new("same".into()));
    assert_eq!(
        d1.get_id(&key),
        d2.get_id(&key),
        "equal attrs must dedup to the same AttrInstId"
    );

    // A different value gets a different handle.
    let mut d3 = AttributeDict::default();
    d3.set(ctx, key.clone(), StringAttr::new("other".into()));
    assert_ne!(d1.get_id(&key), d3.get_id(&key));

    // Re-setting a key copy-on-writes: other dicts keep the old attr.
    let id_a = d1.get_id(&key);
    d1.set(ctx, key.clone(), StringAttr::new("changed".into()));
    assert_ne!(d1.get_id(&key), id_a);
    assert_eq!(
        string_of(d2.get::<StringAttr>(ctx, &key).unwrap()),
        "same",
        "interned attrs are immutable — d2's entry must be untouched"
    );
    assert_eq!(
        string_of(d1.get::<StringAttr>(ctx, &key).unwrap()),
        "changed"
    );
}

#[test]
fn iface_queries_resolve_through_the_store() {
    let ctx = &Context::new();
    let key = attr_key();

    let i64_ty = IntegerType::get(ctx, 64, Signedness::Signed);
    let mut dict = AttributeDict::default();
    dict.set(ctx, key.clone(), TypeAttr::new(i64_ty.into()));

    assert!(dict.impls::<dyn TypedAttrInterface>(ctx, &key));
    assert!(!dict.impls::<dyn OutlinedAttr>(ctx, &key));
    assert!(dict.get_as::<dyn TypedAttrInterface>(ctx, &key).is_some());

    let mut sdict = AttributeDict::default();
    sdict.set(ctx, key.clone(), StringAttr::new("x".into()));
    assert!(!sdict.impls::<dyn TypedAttrInterface>(ctx, &key));
    assert!(sdict.get_as::<dyn TypedAttrInterface>(ctx, &key).is_none());
}

#[test]
fn dict_parses_and_prints_via_ctx() {
    let ctx = &mut Context::new();
    let dict = parse_from_str(
        AttributeDict::parser(()),
        ctx,
        r#"[test_key: builtin.string "v", test_key2: builtin.unit]"#,
    )
    .expect("dict should parse");
    assert_eq!(
        string_of(dict.get::<StringAttr>(ctx, &attr_key()).unwrap()),
        "v"
    );
    let printed = dict.disp(ctx).to_string();
    assert!(printed.contains("builtin.string \"v\""), "{printed}");

    // Roundtrip: re-print + re-parse keeps the same entries.
    let reparsed = parse_from_str(AttributeDict::parser(()), ctx, &printed).unwrap();
    assert_eq!(dict, reparsed);
}

/// The one in-place-mutation path upstream had: given names. With
/// interned attrs the edit must be copy-on-write — renaming op1 must
/// not disturb op2 sharing the deduped entry.
#[test]
fn given_names_are_copy_on_write() {
    let ctx = &mut Context::new();
    let input = r#"
        builtin.module @m {
        ^block_0_0():
          llvm.func @f: llvm.func <builtin.integer i64() variadic = false> [] {
          ^entry():
            a = llvm.constant <builtin.integer <1: i64>> : builtin.integer i64;
            b = llvm.constant <builtin.integer <2: i64>> : builtin.integer i64;
            llvm.return a
          }
        }
        "#;
    let op = parse_from_str(spaced(Operation::top_level_parser()), ctx, input).unwrap();
    let module = Operation::get_op::<ModuleOp>(op, ctx).unwrap();
    // module's block holds the FuncOp; the func's entry block holds the ops.
    let func = pliron_ir_opt::inline::blocks(ctx, module.get_operation())
        .into_iter()
        .flat_map(|b| pliron_ir_opt::inline::ops(ctx, b))
        .next()
        .expect("module should contain the func");
    let ops: Vec<Ptr<Operation>> = pliron_ir_opt::inline::blocks(ctx, func)
        .into_iter()
        .flat_map(|b| pliron_ir_opt::inline::ops(ctx, b))
        .filter(|op| op.deref(ctx).get_num_results() == 1)
        .collect();
    let [a, b] = ops[..2] else {
        panic!("expected the two constants")
    };

    let key = pliron::builtin::attributes::ATTR_KEY_GIVEN_NAMES.clone();
    set_operation_result_name(ctx, a, 0, Some(ident!("x")));
    set_operation_result_name(ctx, b, 0, Some(ident!("x")));
    // Same names → the two ops share the deduped GivenNamesAttr.
    assert_eq!(
        a.deref(ctx).attributes().get_id(&key),
        b.deref(ctx).attributes().get_id(&key)
    );

    // Renaming `a` must not rename `b`.
    set_operation_result_name(ctx, a, 0, Some(ident!("y")));
    assert_eq!(get_operation_result_name(ctx, a, 0), Some(ident!("y")));
    assert_eq!(get_operation_result_name(ctx, b, 0), Some(ident!("x")));

    // Removing a's name leaves b's intact and drops a's attr entirely.
    remove_operation_result_name(ctx, a, 0);
    assert_eq!(get_operation_result_name(ctx, a, 0), None);
    assert!(a.deref(ctx).attributes().get_id(&key).is_none());
    assert_eq!(get_operation_result_name(ctx, b, 0), Some(ident!("x")));
}
