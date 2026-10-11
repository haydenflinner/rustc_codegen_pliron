// Mirrors rustc_hir_analysis::param_default_policy: an outer match on a
// sparse enum-discriminant set {1,3,4,5,19}, and inside the `Item` arm an
// inner match on {3,4,9..=15}. `Item`/`ItemKind::Trait` must yield `Allowed`.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Policy {
    Allowed,
    FutureCompatForbidden,
    Forbidden,
}

struct Item {
    kind: ItemKind,
}

#[derive(Clone, Copy)]
enum ItemKind {
    ExternCrate,
    Use,
    Static,
    Const,
    Fn,
    Macro,
    Mod,
    ForeignMod,
    GlobalAsm,
    TyAlias,
    Enum,
    Struct,
    Union,
    Trait,
    TraitAlias,
    Impl,
}

enum Node<'a> {
    Crate,
    Item(&'a Item),
    Synthetic,
    ForeignItem(u32),
    TraitItem(u32),
    ImplItem(u32),
    V6,
    V7,
    V8,
    V9,
    V10,
    V11,
    V12,
    V13,
    V14,
    V15,
    V16,
    V17,
    V18,
    OpaqueTy(u32),
}

#[inline(never)]
fn param_default_policy(node: &Node) -> Option<Policy> {
    match node {
        Node::Item(item) => match item.kind {
            ItemKind::Const => Some(Policy::Forbidden),
            ItemKind::Fn | ItemKind::Impl => Some(Policy::FutureCompatForbidden),
            ItemKind::TyAlias
            | ItemKind::Enum
            | ItemKind::Struct
            | ItemKind::Union
            | ItemKind::Trait
            | ItemKind::TraitAlias => Some(Policy::Allowed),
            _ => None,
        },
        Node::ForeignItem(..) => Some(Policy::Forbidden),
        Node::TraitItem(..) | Node::ImplItem(..) => Some(Policy::FutureCompatForbidden),
        Node::OpaqueTy(..) => Some(Policy::Allowed),
        _ => None,
    }
}

fn main() {
    let kinds = [
        (ItemKind::Const, Some(Policy::Forbidden)),
        (ItemKind::Fn, Some(Policy::FutureCompatForbidden)),
        (ItemKind::TyAlias, Some(Policy::Allowed)),
        (ItemKind::Enum, Some(Policy::Allowed)),
        (ItemKind::Struct, Some(Policy::Allowed)),
        (ItemKind::Union, Some(Policy::Allowed)),
        (ItemKind::Trait, Some(Policy::Allowed)),
        (ItemKind::TraitAlias, Some(Policy::Allowed)),
        (ItemKind::Impl, Some(Policy::FutureCompatForbidden)),
        (ItemKind::Use, None),
    ];
    let mut bad = 0;
    for (kind, want) in kinds {
        let item = Item { kind };
        let got = param_default_policy(&Node::Item(&item));
        if got != want {
            bad += 1;
            eprintln!("kind {:?} -> {:?}, want {:?}", kind as u32, got, want);
        }
    }
    for (node, want) in [
        (Node::ForeignItem(0), Some(Policy::Forbidden)),
        (Node::TraitItem(0), Some(Policy::FutureCompatForbidden)),
        (Node::ImplItem(0), Some(Policy::FutureCompatForbidden)),
        (Node::OpaqueTy(0), Some(Policy::Allowed)),
        (Node::Crate, None),
        (Node::Synthetic, None),
        (Node::V6, None),
        (Node::V18, None),
    ] {
        let got = param_default_policy(&node);
        if got != want {
            bad += 1;
            eprintln!("node -> {:?}, want {:?}", got, want);
        }
    }
    assert_eq!(bad, 0, "{bad} wrong policy results");
}
