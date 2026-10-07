extern crate proc_macro;
use proc_macro::TokenStream;
#[proc_macro]
pub fn twice(i: TokenStream) -> TokenStream { let s = i.to_string(); format!("({s}) * 2").parse().unwrap() }
#[proc_macro_derive(Hi)]
pub fn hi(i: TokenStream) -> TokenStream { let n = i.into_iter().filter_map(|t| if let proc_macro::TokenTree::Ident(x)=t {Some(x.to_string())} else {None}).last().unwrap(); format!("impl {n} {{ fn hi() -> &'static str {{ \"hi {n}\" }} }}").parse().unwrap() }
