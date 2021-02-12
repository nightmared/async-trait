use crate::lifetime::CollectLifetimes;
use crate::parse::Item;
use crate::receiver::{ mut_pat, has_self_in_block, has_self_in_sig, ReplaceSelf};
use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote, quote_spanned, ToTokens};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::visit_mut::VisitMut;
use syn::{
    parse_quote, Block, FnArg, GenericParam, Generics, Ident, ImplItem, Lifetime, Pat, PatIdent,
    Receiver, ReturnType, Signature, Stmt, Token, TraitItem, Type, TypeParamBound,
    WhereClause,
};

impl ToTokens for Item {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        match self {
            Item::Trait(item) => item.to_tokens(tokens),
            Item::Impl(item) => item.to_tokens(tokens),
        }
    }
}

#[derive(Clone, Copy)]
enum Context<'a> {
    Trait {
        generics: &'a Generics,
        supertraits: &'a Supertraits,
    },
    Impl {
        impl_generics: &'a Generics,
    },
}

impl Context<'_> {
    fn lifetimes<'a>(&'a self, used: &'a [Lifetime]) -> impl Iterator<Item = &'a GenericParam> {
        let generics = match self {
            Context::Trait { generics, .. } => generics,
            Context::Impl { impl_generics, .. } => impl_generics,
        };
        generics.params.iter().filter(move |param| {
            if let GenericParam::Lifetime(param) = param {
                used.contains(&param.lifetime)
            } else {
                false
            }
        })
    }
}

type Supertraits = Punctuated<TypeParamBound, Token![+]>;

pub fn expand(input: &mut Item, is_local: bool) {
    match input {
        Item::Trait(input) => {
            let context = Context::Trait {
                generics: &input.generics,
                supertraits: &input.supertraits,
            };
            for inner in &mut input.items {
                if let TraitItem::Method(method) = inner {
                    let sig = &mut method.sig;
                    if sig.asyncness.is_some() {
                        let block = &mut method.default;
                        let mut has_self = has_self_in_sig(sig);
                        if let Some(block) = block {
                            has_self |= has_self_in_block(block);
                            transform_block(sig, block);
                            method
                                .attrs
                                .push(parse_quote!(#[allow(clippy::used_underscore_binding)]));
                        }
                        let has_default = method.default.is_some();
                        transform_sig(context, sig, has_self, has_default, is_local);
                        method.attrs.push(parse_quote!(#[must_use]));
                    }
                }
            }
        }
        Item::Impl(input) => {
            let mut lifetimes = CollectLifetimes::new("'impl");
            lifetimes.visit_type_mut(&mut *input.self_ty);
            lifetimes.visit_path_mut(&mut input.trait_.as_mut().unwrap().1);
            let params = &input.generics.params;
            let elided = lifetimes.elided;
            input.generics.params = parse_quote!(#(#elided,)* #params);

            let context = Context::Impl {
                impl_generics: &input.generics,
            };
            for inner in &mut input.items {
                if let ImplItem::Method(method) = inner {
                    let sig = &mut method.sig;
                    if sig.asyncness.is_some() {
                        let block = &mut method.block;
                        let has_self = has_self_in_sig(sig) || has_self_in_block(block);
                        transform_block(sig, block);
                        transform_sig(context, sig, has_self, false, is_local);
                        method
                            .attrs
                            .push(parse_quote!(#[allow(clippy::used_underscore_binding)]));
                    }
                }
            }
        }
    }
}

// Input:
//     async fn f<T>(&self, x: &T) -> Ret;
//
// Output:
//     fn f<'life0, 'life1, 'async_trait, T>(
//         &'life0 self,
//         x: &'life1 T,
//     ) -> Pin<Box<dyn Future<Output = Ret> + Send + 'async_trait>>
//     where
//         'life0: 'async_trait,
//         'life1: 'async_trait,
//         T: 'async_trait,
//         Self: Sync + 'async_trait;
fn transform_sig(
    context: Context,
    sig: &mut Signature,
    has_self: bool,
    has_default: bool,
    is_local: bool,
) {
    sig.fn_token.span = sig.asyncness.take().unwrap().span;

    let ret = match &sig.output {
        ReturnType::Default => quote!(()),
        ReturnType::Type(_, ret) => quote!(#ret),
    };

    let mut lifetimes = CollectLifetimes::new("'life");
    for arg in sig.inputs.iter_mut() {
        match arg {
            FnArg::Receiver(arg) => lifetimes.visit_receiver_mut(arg),
            FnArg::Typed(arg) => lifetimes.visit_type_mut(&mut arg.ty),
        }
    }

    let where_clause = sig
        .generics
        .where_clause
        .get_or_insert_with(|| WhereClause {
            where_token: Default::default(),
            predicates: Punctuated::new(),
        });
    for param in sig
        .generics
        .params
        .iter()
        .chain(context.lifetimes(&lifetimes.explicit))
    {
        match param {
            GenericParam::Type(param) => {
                let param = &param.ident;
                where_clause
                    .predicates
                    .push(parse_quote!(#param: 'async_trait));
            }
            GenericParam::Lifetime(param) => {
                let param = &param.lifetime;
                where_clause
                    .predicates
                    .push(parse_quote!(#param: 'async_trait));
            }
            GenericParam::Const(_) => {}
        }
    }
    for elided in lifetimes.elided {
        sig.generics.params.push(parse_quote!(#elided));
        where_clause
            .predicates
            .push(parse_quote!(#elided: 'async_trait));
    }
    sig.generics.params.push(parse_quote!('async_trait));
    if has_self {
        let bound: Ident = match sig.inputs.iter().next() {
            Some(FnArg::Receiver(Receiver {
                reference: Some(_),
                mutability: None,
                ..
            })) => parse_quote!(Sync),
            Some(FnArg::Typed(arg))
                if match (arg.pat.as_ref(), arg.ty.as_ref()) {
                    (Pat::Ident(pat), Type::Reference(ty)) => {
                        pat.ident == "self" && ty.mutability.is_none()
                    }
                    _ => false,
                } =>
            {
                parse_quote!(Sync)
            }
            _ => parse_quote!(Send),
        };
        let assume_bound = match context {
            Context::Trait { supertraits, .. } => !has_default || has_bound(supertraits, &bound),
            Context::Impl { .. } => true,
        };
        where_clause.predicates.push(if assume_bound || is_local {
            parse_quote!(Self: 'async_trait)
        } else {
            parse_quote!(Self: ::core::marker::#bound + 'async_trait)
        });
    }

    for (i, arg) in sig.inputs.iter_mut().enumerate() {
        match arg {
            FnArg::Receiver(Receiver {
                reference: Some(_), ..
            }) => {}
            FnArg::Receiver(arg) => arg.mutability = None,
            FnArg::Typed(arg) => {
                if let Pat::Ident(ident) = &mut *arg.pat {
                    ident.by_ref = None;
                    ident.mutability = None;
                } else {
                    let span = arg.pat.span();
                    let positional = positional_arg(i, span);
                    let m = mut_pat(&mut arg.pat);
                    arg.pat = parse_quote!(#m #positional);
                }
            }
        }
    }

    let bounds = if is_local {
        quote!('async_trait)
    } else {
        quote!(::core::marker::Send + 'async_trait)
    };

    sig.output = parse_quote! {
        -> ::core::pin::Pin<Box<
            dyn ::core::future::Future<Output = #ret> + #bounds
        >>
    };
}

// Input:
//     async fn f<T>(&self, x: &T, (a, b): (A, B)) -> Ret {
//         self + x + a + b
//     }
//
// Output:
//     Box::pin(async move {
//         let ___ret: Ret = {
//             let __self = self;
//             let x = x;
//             let (a, b) = __arg1;
//
//             __self + x + a + b
//         };
//
//         ___ret
//     })
fn transform_block(
    sig: &mut Signature,
    block: &mut Block,
) {
    if let Some(Stmt::Item(syn::Item::Verbatim(item))) = block.stmts.first() {
        if block.stmts.len() == 1 && item.to_string() == ";" {
            return;
        }
    }

    let self_prefix = "__";
    let mut self_span = None;
    let decls = sig.inputs.iter().enumerate().map(|(i, arg)| match arg {
        FnArg::Receiver(Receiver { self_token, mutability, .. }) => {
            let mut ident = format_ident!("{}self", self_prefix);
            ident.set_span(self_token.span());
            self_span = Some(self_token.span());
            quote!(let #mutability #ident = #self_token;)
        }
        FnArg::Typed(arg) => {
            if let Pat::Ident(PatIdent { ident, mutability, .. }) = &*arg.pat {
                if ident == "self" {
                    self_span = Some(ident.span());
                    let prefixed = format_ident!("{}{}", self_prefix, ident);
                    quote!(let #mutability #prefixed = #ident;)
                } else {
                    quote!(let #mutability #ident = #ident;)
                }
            } else {
                let pat = &arg.pat;
                let ident = positional_arg(i, pat.span());
                quote!(let #pat = #ident;)
            }
        }
    }).collect::<Vec<_>>();

    if let Some(span) = self_span {
        let mut replace_self = ReplaceSelf(self_prefix, span);
        replace_self.visit_block_mut(block);
    }

    let stmts = &block.stmts;
    let ret_ty = match &sig.output {
        ReturnType::Default => quote_spanned!(block.span()=>()),
        ReturnType::Type(_, ret) => quote!(#ret),
    };

    let box_pin = quote_spanned!(ret_ty.span()=>
        Box::pin(async move {
            let __ret: #ret_ty = {
                #(#decls)*
                let __async_trait: ();
                #(#stmts)*
            };

            #[allow(unreachable_code)]
            __ret
        })
    );

    block.stmts = parse_quote!(#box_pin);
}

fn positional_arg(i: usize, span: Span) -> Ident {
    format_ident!("__arg{}", i, span = span)
}

fn has_bound(supertraits: &Supertraits, marker: &Ident) -> bool {
    for bound in supertraits {
        if let TypeParamBound::Trait(bound) = bound {
            if bound.path.is_ident(marker) {
                return true;
            }
        }
    }
    false
}
