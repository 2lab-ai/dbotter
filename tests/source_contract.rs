use std::fs;
use std::path::{Path, PathBuf};

use proc_macro2::TokenStream;
use quote::ToTokens as _;
use syn::{Attribute, ImplItem, Item, TraitItem};

#[test]
fn every_production_ast_region_has_no_panicking_placeholder_or_legacy_upsert() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut files = vec![root.join("build.rs")];
    collect_rs(&root.join("src"), &mut files);
    for path in files {
        let production = production_tokens(&path);
        for forbidden in [
            "panic!(",
            "todo!(",
            "unimplemented!(",
            ".unwrap(",
            ".expect(",
            "upsert_profile",
            "UpsertProfile",
        ] {
            assert!(
                !production.contains(forbidden),
                "{} contains forbidden production token {forbidden}",
                path.display()
            );
        }
        if path.ends_with("src/ui/adapter.rs") {
            assert!(
                production.contains("bounded_ports"),
                "production after an item-level cfg(test) must still be scanned"
            );
        }
    }
}

fn production_tokens(path: &Path) -> String {
    let source = fs::read_to_string(path).expect("source reads");
    let parsed = syn::parse_file(&source).expect("production Rust parses");
    let mut tokens = TokenStream::new();
    collect_items(&parsed.items, &mut tokens);
    tokens
        .to_string()
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn collect_items(items: &[Item], output: &mut TokenStream) {
    for item in items {
        if test_only(item_attributes(item)) {
            continue;
        }
        match item {
            Item::Mod(module) if module.content.is_some() => {
                let (_, items) = module.content.as_ref().expect("checked inline module");
                collect_items(items, output);
            }
            Item::Impl(item_impl) => {
                item_impl.generics.to_tokens(output);
                item_impl.self_ty.to_tokens(output);
                for item in &item_impl.items {
                    if !test_only(impl_item_attributes(item)) {
                        item.to_tokens(output);
                    }
                }
            }
            Item::Trait(item_trait) => {
                item_trait.ident.to_tokens(output);
                for item in &item_trait.items {
                    if !test_only(trait_item_attributes(item)) {
                        item.to_tokens(output);
                    }
                }
            }
            _ => item.to_tokens(output),
        }
    }
}

fn test_only(attributes: &[Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        if !attribute.path().is_ident("cfg") {
            return false;
        }
        let syn::Meta::List(list) = &attribute.meta else {
            return false;
        };
        list.tokens
            .to_string()
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>()
            == "test"
    })
}

fn item_attributes(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(item) => &item.attrs,
        Item::Enum(item) => &item.attrs,
        Item::ExternCrate(item) => &item.attrs,
        Item::Fn(item) => &item.attrs,
        Item::ForeignMod(item) => &item.attrs,
        Item::Impl(item) => &item.attrs,
        Item::Macro(item) => &item.attrs,
        Item::Mod(item) => &item.attrs,
        Item::Static(item) => &item.attrs,
        Item::Struct(item) => &item.attrs,
        Item::Trait(item) => &item.attrs,
        Item::TraitAlias(item) => &item.attrs,
        Item::Type(item) => &item.attrs,
        Item::Union(item) => &item.attrs,
        Item::Use(item) => &item.attrs,
        Item::Verbatim(_) => &[],
        _ => &[],
    }
}

fn impl_item_attributes(item: &ImplItem) -> &[Attribute] {
    match item {
        ImplItem::Const(item) => &item.attrs,
        ImplItem::Fn(item) => &item.attrs,
        ImplItem::Type(item) => &item.attrs,
        ImplItem::Macro(item) => &item.attrs,
        ImplItem::Verbatim(_) => &[],
        _ => &[],
    }
}

fn trait_item_attributes(item: &TraitItem) -> &[Attribute] {
    match item {
        TraitItem::Const(item) => &item.attrs,
        TraitItem::Fn(item) => &item.attrs,
        TraitItem::Type(item) => &item.attrs,
        TraitItem::Macro(item) => &item.attrs,
        TraitItem::Verbatim(_) => &[],
        _ => &[],
    }
}

fn collect_rs(directory: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).expect("source directory reads") {
        let entry = entry.expect("directory entry");
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}
