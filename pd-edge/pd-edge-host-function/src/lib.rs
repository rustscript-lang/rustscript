mod edge;

use proc_macro::TokenStream;
use syn::{ItemFn, Meta, Token, parse_macro_input, punctuated::Punctuated};

#[proc_macro_attribute]
pub fn pd_edge_host_function(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr with Punctuated::<Meta, Token![,]>::parse_terminated);
    match edge::expand_pd_edge_host_function(args, parse_macro_input!(item as ItemFn)) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}
