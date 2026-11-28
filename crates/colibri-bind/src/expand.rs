//! The expansion of `#[colibri::export]`, on `proc_macro2` so that it can be tested.

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote, quote_spanned};
use syn::ext::IdentExt;
use syn::parse::{Parse, ParseStream, Parser};
use syn::spanned::Spanned;
use syn::{
    Error, Fields, FnArg, Ident, ImplItem, ImplItemFn, Item, ItemEnum, ItemFn, ItemImpl,
    ItemStruct, Pat, Result, Signature, Token, Type, Visibility,
};

/// What the attribute says: `value`, `no_methods`, `crate = path`.
#[derive(Default)]
struct Options {
    value: Option<Span>,
    no_methods: Option<Span>,
    runtime: Option<syn::Path>,
    /// `nested`, for `#[colibri::module]` only: allows a function below the crate root.
    nested: Option<Span>,
}

impl Parse for Options {
    fn parse(input: ParseStream) -> Result<Self> {
        let mut options = Options::default();
        while !input.is_empty() {
            if input.peek(Token![crate]) {
                input.parse::<Token![crate]>()?;
                input.parse::<Token![=]>()?;
                options.runtime = Some(input.parse()?);
            } else {
                let word: Ident = input.parse()?;
                match word.to_string().as_str() {
                    "value" => options.value = Some(word.span()),
                    "no_methods" => options.no_methods = Some(word.span()),
                    "nested" => options.nested = Some(word.span()),
                    other => {
                        return Err(Error::new(
                            word.span(),
                            format!(
                                "unknown option `{other}`: the options are `value`, `no_methods` and `crate = path`"
                            ),
                        ));
                    }
                }
            }
            if input.is_empty() {
                break;
            }
            input.parse::<Token![,]>()?;
        }
        Ok(options)
    }
}

/// The path of `colibri_runtime::__private`, which everything generated goes through.
struct Paths {
    private: TokenStream,
}

pub fn export(args: TokenStream, item: TokenStream) -> TokenStream {
    let parsed = Options::parse.parse2(args);
    let item_copy = item.clone();
    let result = parsed.and_then(|options| {
        if let Some(span) = options.nested {
            return Err(Error::new(
                span,
                "`nested` is for `#[colibri::module]`: `#[colibri::export]` takes `value`, `no_methods` and `crate = path`",
            ));
        }
        let parsed: Item = syn::parse2(item)?;
        let runtime = match &options.runtime {
            Some(path) => quote!(#path),
            None => quote!(::colibri_runtime),
        };
        let paths = Paths {
            private: quote!(#runtime::__private),
        };
        match parsed {
            Item::Struct(s) => export_struct(&options, &paths, s),
            Item::Enum(e) => export_enum(&options, &paths, e),
            Item::Impl(i) => export_impl(&options, &paths, i),
            Item::Fn(f) => export_fn(&options, &paths, f, &runtime),
            other => Err(Error::new_spanned(
                other,
                "`#[colibri::export]` goes on a struct, an enum, an `impl` block or a function",
            )),
        }
    });
    match result {
        Ok(tokens) => tokens,
        // The item stays, so that the editor still knows it and only one error shows.
        Err(error) => {
            let error = error.to_compile_error();
            quote!(#item_copy #error)
        }
    }
}

fn only_value_allowed(options: &Options, what: &str) -> Result<()> {
    match options.value {
        Some(span) => Err(Error::new(
            span,
            format!("`value` is for a struct or an enum, not for {what}"),
        )),
        None => Ok(()),
    }
}

fn no_generics(generics: &syn::Generics, what: &str) -> Result<()> {
    if generics.params.is_empty() && generics.where_clause.is_none() {
        return Ok(());
    }
    Err(Error::new_spanned(
        generics,
        format!("{what} cannot be generic: Colibri sees one type, so export a concrete one"),
    ))
}

// ---- structs and enums ----

fn export_struct(options: &Options, paths: &Paths, item: ItemStruct) -> Result<TokenStream> {
    no_generics(&item.generics, "an exported struct")?;
    let name = &item.ident;
    let private = &paths.private;
    if options.value.is_none() {
        // An opaque host class: `describe` takes its methods from the exported `impl`.
        let describe = if options.no_methods.is_some() {
            quote! { fn describe(_: &mut #private::TypeBuilder<'_, Self>) {} }
        } else {
            quote! {
                fn describe(d: &mut #private::TypeBuilder<'_, Self>) {
                    <Self as #private::ExportedMethods>::describe_methods(d);
                }
            }
        };
        let text = name.unraw().to_string();
        return Ok(quote! {
            #item
            impl #private::HostType for #name {
                const NAME: &'static str = #text;
                #describe
            }
        });
    }
    if let Some(span) = options.no_methods {
        return Err(Error::new(
            span,
            "`no_methods` is for a host class, and a `value` struct has no methods",
        ));
    }
    let Fields::Named(fields) = &item.fields else {
        return Err(Error::new_spanned(
            &item.fields,
            "a `value` struct needs named fields: Colibri structs are `struct Name { field: type }`",
        ));
    };
    let mut names = Vec::new();
    let mut types = Vec::new();
    for field in &fields.named {
        let ident = field.ident.as_ref().expect("named fields have names");
        if !matches!(field.vis, Visibility::Public(_)) {
            return Err(Error::new_spanned(
                field,
                format!(
                    "the field `{ident}` must be `pub`: a `value` struct is converted field by field, and scripts see every field"
                ),
            ));
        }
        names.push(ident);
        types.push(&field.ty);
    }
    let text = name.unraw().to_string();
    let field_texts: Vec<String> = names.iter().map(|n| n.unraw().to_string()).collect();
    let indices: Vec<usize> = (0..names.len()).collect();
    let copy_check = quote_spanned! {name.span()=>
        const _: fn() = || {
            fn assert_copy<T: ::core::marker::Copy>() {}
            assert_copy::<#name>();
        };
    };
    Ok(quote! {
        #item
        #copy_check
        impl #private::RefParam for #name {
            type Owned = #name;
        }
        impl #private::IntoColibri for #name {
            fn ty(reg: &mut #private::TypeRegistry) -> #private::TypeDesc {
                let fields = ::std::vec![
                    #((#field_texts, <#types as #private::IntoColibri>::ty(reg))),*
                ];
                reg.name_rust_type(::core::any::TypeId::of::<#name>(), ::core::any::type_name::<#name>());
                reg.define_struct_of(::core::any::TypeId::of::<#name>(), #text, fields)
            }
            fn into_colibri(self, cx: &mut #private::Cx<'_>) -> #private::Value {
                let fields = ::std::vec![
                    #(<#types as #private::IntoColibri>::into_colibri(self.#names, cx)),*
                ];
                cx.new_struct_of(::core::any::TypeId::of::<#name>(), #text, fields)
            }
        }
        impl #private::FromColibri for #name {
            fn ty(reg: &mut #private::TypeRegistry) -> #private::TypeDesc {
                <Self as #private::IntoColibri>::ty(reg)
            }
            #[allow(unused_variables)]
            fn from_colibri(
                value: &#private::Value,
                cx: &mut #private::Cx<'_>,
            ) -> ::core::result::Result<Self, #private::Error> {
                ::core::result::Result::Ok(#name {
                    #(#names: <#types as #private::FromColibri>::from_colibri(
                        &#private::field(value, #indices, #text)?,
                        cx,
                    )?),*
                })
            }
        }
    })
}

fn export_enum(options: &Options, paths: &Paths, item: ItemEnum) -> Result<TokenStream> {
    if options.value.is_none() {
        return Err(Error::new_spanned(
            &item.ident,
            "an enum is exported by value: write `#[colibri::export(value)]`",
        ));
    }
    if let Some(span) = options.no_methods {
        return Err(Error::new(
            span,
            "`no_methods` is for a host class, and a `value` enum has no methods",
        ));
    }
    no_generics(&item.generics, "an exported enum")?;
    let name = &item.ident;
    let private = &paths.private;
    let mut variants = Vec::new();
    for v in &item.variants {
        if !matches!(v.fields, Fields::Unit) {
            return Err(Error::new_spanned(
                &v.fields,
                format!(
                    "the variant `{}` has a payload: only enums without payloads can be exported, as Colibri enums",
                    v.ident
                ),
            ));
        }
        if let Some((_, discriminant)) = &v.discriminant {
            return Err(Error::new_spanned(
                discriminant,
                "an exported enum cannot give its variants numbers: Colibri identifies them by position",
            ));
        }
        variants.push(&v.ident);
    }
    let text = name.unraw().to_string();
    let variant_texts: Vec<String> = variants.iter().map(|v| v.unraw().to_string()).collect();
    let indices: Vec<u32> = (0..variants.len() as u32).collect();
    Ok(quote! {
        #item
        impl #private::RefParam for #name {
            type Owned = #name;
        }
        impl #private::IntoColibri for #name {
            fn ty(reg: &mut #private::TypeRegistry) -> #private::TypeDesc {
                reg.name_rust_type(::core::any::TypeId::of::<#name>(), ::core::any::type_name::<#name>());
                reg.define_enum_of(::core::any::TypeId::of::<#name>(), #text, &[#(#variant_texts),*])
            }
            fn into_colibri(self, cx: &mut #private::Cx<'_>) -> #private::Value {
                let index: u32 = match self {
                    #(#name::#variants => #indices,)*
                };
                cx.new_variant_of(::core::any::TypeId::of::<#name>(), #text, index)
            }
        }
        impl #private::FromColibri for #name {
            fn ty(reg: &mut #private::TypeRegistry) -> #private::TypeDesc {
                <Self as #private::IntoColibri>::ty(reg)
            }
            fn from_colibri(
                value: &#private::Value,
                _cx: &mut #private::Cx<'_>,
            ) -> ::core::result::Result<Self, #private::Error> {
                match value.variant_index() {
                    #(::core::option::Option::Some(#indices) => ::core::result::Result::Ok(#name::#variants),)*
                    other => ::core::result::Result::Err(#private::Error::Convert(
                        ::std::format!("expected a `{}` value, got variant {:?}", #text, other),
                    )),
                }
            }
        }
    })
}

// ---- functions and methods ----

/// One exported function, as the generated code needs it.
struct Exported {
    /// The Colibri name.
    name: String,
    ident: Ident,
    /// `Some(mutable)` for a method.
    receiver: Option<bool>,
    /// The `&mut Ctx` parameter, as written (`cx: &mut colibri::Ctx`).
    ctx: Option<(Ident, Type)>,
    /// The other parameters, as written.
    params: Vec<(Ident, Type)>,
}

/// How a parameter reaches the host function: the type the closure takes, and the expression
/// that passes it on.
struct Lowered {
    /// The closure parameter's pattern: `mut` for the owner of a `&mut`.
    pat: TokenStream,
    ty: TokenStream,
    arg: TokenStream,
}

/// A reference parameter becomes an owned local of the type its trait names, which the call
/// borrows: `&str` from a `String`, `&[T]` from a `Vec<T>`, `&V` from the value, and `&T` /
/// `&mut T` of a host class from a `Ref<T>` / `RefMut<T>`, which hold the borrow flag for the
/// call. Anything else passes as written.
fn lower(paths: &Paths, ident: &Ident, ty: &Type) -> Result<Lowered> {
    let private = &paths.private;
    let Type::Reference(r) = strip(ty) else {
        return Ok(Lowered {
            pat: quote!(#ident),
            ty: quote!(#ty),
            arg: quote!(#ident),
        });
    };
    let elem = &*r.elem;
    let span = ty.span();
    if r.mutability.is_some() {
        if matches!(elem, Type::Slice(_)) || matches!(elem, Type::Path(p) if p.path.is_ident("str"))
        {
            return Err(Error::new_spanned(
                ty,
                "a `&mut str` or `&mut [T]` parameter cannot be exported: scripts pass copies, so a change would be lost; take `&str`, `&[T]` or an owned value",
            ));
        }
        Ok(Lowered {
            pat: quote_spanned!(span=> mut #ident),
            ty: quote_spanned!(span=> <#elem as #private::MutParam>::Owned),
            arg: quote!(&mut #ident),
        })
    } else {
        Ok(Lowered {
            pat: quote!(#ident),
            ty: quote_spanned!(span=> <#elem as #private::RefParam>::Owned),
            arg: quote!(&#ident),
        })
    }
}

/// The type without the invisible groups and parentheses a macro or a `(T)` leaves around it.
fn strip(mut ty: &Type) -> &Type {
    loop {
        match ty {
            Type::Group(g) => ty = &g.elem,
            Type::Paren(p) => ty = &p.elem,
            other => return other,
        }
    }
}

/// The first reference in a result type that a script could not hold: `&'static` ones are
/// copied out (`&'static str`, `&'static [T]`), every other one would borrow from the host.
fn borrowed_in(ty: &Type) -> Option<&Type> {
    match strip(ty) {
        Type::Reference(r) => {
            if r.lifetime.as_ref().is_some_and(|l| l.ident == "static") {
                borrowed_in(&r.elem)
            } else {
                Some(strip(ty))
            }
        }
        Type::Path(p) => p.path.segments.iter().find_map(|seg| match &seg.arguments {
            syn::PathArguments::AngleBracketed(a) => a.args.iter().find_map(|arg| match arg {
                syn::GenericArgument::Type(t) => borrowed_in(t),
                _ => None,
            }),
            _ => None,
        }),
        Type::Tuple(t) => t.elems.iter().find_map(borrowed_in),
        Type::Array(a) => borrowed_in(&a.elem),
        Type::Slice(a) => borrowed_in(&a.elem),
        _ => None,
    }
}

/// A result that borrows cannot be converted: the script's value would have to borrow from the host.
fn check_output(sig: &Signature) -> Result<()> {
    if let syn::ReturnType::Type(_, ty) = &sig.output
        && let Some(bad) = borrowed_in(ty)
    {
        return Err(Error::new_spanned(
            bad,
            format!(
                "`{}` returns a reference, which a script cannot hold: return an owned value (a copy, a `String`, a `Vec`, or a `Handle`); only `&'static` results are copied",
                sig.ident
            ),
        ));
    }
    Ok(())
}

fn is_ctx(ty: &Type) -> bool {
    let Type::Reference(r) = strip(ty) else {
        return false;
    };
    if r.mutability.is_none() {
        return false;
    }
    match &*r.elem {
        Type::Path(p) => p.path.segments.last().is_some_and(|s| s.ident == "Ctx"),
        _ => false,
    }
}

fn analyse(sig: &Signature, allow_receiver: bool) -> Result<Exported> {
    let what = format!("`{}`", sig.ident);
    if let Some(span) = sig.asyncness.map(|t| t.span) {
        return Err(Error::new(
            span,
            format!("{what} is `async`, which cannot be exported"),
        ));
    }
    if let syn::Safety::Unsafe(unsafety) = &sig.safety {
        return Err(Error::new_spanned(
            unsafety,
            format!("{what} is `unsafe`, which cannot be exported"),
        ));
    }
    if let Some(abi) = &sig.abi {
        return Err(Error::new_spanned(
            abi,
            format!("{what} has an `extern` ABI, which cannot be exported"),
        ));
    }
    if !sig.generics.params.is_empty() || sig.generics.where_clause.is_some() {
        return Err(Error::new_spanned(
            &sig.generics,
            format!("{what} is generic: Colibri sees concrete types, so export a concrete wrapper"),
        ));
    }
    if let Some(v) = &sig.variadic {
        return Err(Error::new_spanned(v, format!("{what} is variadic")));
    }
    check_output(sig)?;
    let mut receiver = None;
    let mut ctx = None;
    let mut params = Vec::new();
    for (i, arg) in sig.inputs.iter().enumerate() {
        match arg {
            FnArg::Receiver(r) => {
                if !allow_receiver {
                    return Err(Error::new_spanned(
                        r,
                        format!("{what} is a function, not a method"),
                    ));
                }
                let syn::ReceiverKind::Reference(_, _, mutability) = &r.kind else {
                    return Err(Error::new_spanned(
                        r,
                        format!(
                            "{what} takes `self` by value: the host owns its objects, so take `&self` or `&mut self`"
                        ),
                    ));
                };
                receiver = Some(mutability.is_some());
            }
            FnArg::Typed(typed) => {
                let Pat::Ident(pat) = &*typed.pat else {
                    return Err(Error::new_spanned(
                        &typed.pat,
                        "an exported parameter needs a plain name: Colibri shows parameter names to scripts",
                    ));
                };
                if pat.by_ref.is_some() || pat.subpat.is_some() {
                    return Err(Error::new_spanned(
                        &typed.pat,
                        "an exported parameter needs a plain name",
                    ));
                }
                let first_after_receiver = i == usize::from(receiver.is_some());
                if first_after_receiver && is_ctx(&typed.ty) && ctx.is_none() {
                    ctx = Some((pat.ident.clone(), (*typed.ty).clone()));
                } else {
                    params.push((pat.ident.clone(), (*typed.ty).clone()));
                }
            }
        }
    }
    Ok(Exported {
        name: sig.ident.unraw().to_string(),
        ident: sig.ident.clone(),
        receiver,
        ctx,
        params,
    })
}

impl Exported {
    fn param_names(&self) -> Vec<String> {
        self.params
            .iter()
            .map(|(n, _)| n.unraw().to_string())
            .collect()
    }
}

fn export_impl(options: &Options, paths: &Paths, item: ItemImpl) -> Result<TokenStream> {
    only_value_allowed(options, "an `impl` block")?;
    if let Some(span) = options.no_methods {
        return Err(Error::new(
            span,
            "`no_methods` goes on the struct, not on its `impl` block",
        ));
    }
    let private = &paths.private;
    if let Some((path, _)) = &item.trait_ {
        return Err(Error::new_spanned(
            path,
            "export an inherent `impl` block: the methods of a trait are not exported",
        ));
    }
    no_generics(&item.generics, "an exported `impl` block")?;
    let self_ty = &item.self_ty;
    let mut describe = Vec::new();
    for member in &item.items {
        let ImplItem::Fn(f) = member else {
            continue;
        };
        if !matches!(f.vis, Visibility::Public(_)) {
            continue;
        }
        describe.push(describe_member(paths, f)?);
    }
    Ok(quote! {
        #item
        impl #private::ExportedMethods for #self_ty {
            #[allow(unused_variables)]
            fn describe_methods(d: &mut #private::TypeBuilder<'_, Self>) {
                #(#describe)*
            }
        }
    })
}

fn describe_member(paths: &Paths, f: &ImplItemFn) -> Result<TokenStream> {
    let e = analyse(&f.sig, true)?;
    let name = &e.name;
    let ident = &e.ident;
    let names = e.param_names();
    let lowered = e
        .params
        .iter()
        .map(|(n, t)| lower(paths, n, t))
        .collect::<Result<Vec<_>>>()?;
    let pats: Vec<&TokenStream> = lowered.iter().map(|l| &l.pat).collect();
    let types: Vec<&TokenStream> = lowered.iter().map(|l| &l.ty).collect();
    let args: Vec<&TokenStream> = lowered.iter().map(|l| &l.arg).collect();
    let ctx_param = e.ctx.as_ref().map(|(n, t)| quote!(#n: #t,));
    let ctx_arg = e.ctx.as_ref().map(|(n, _)| quote!(#n,));
    Ok(match e.receiver {
        None => quote! {
            d.static_fn(#name, [#(#names),*], |#ctx_param #(#pats: #types),*| {
                Self::#ident(#ctx_arg #(#args),*)
            });
        },
        Some(mutable) => {
            let (method, this_ty) = if mutable {
                (format_ident!("method_mut"), quote!(&mut Self))
            } else {
                (format_ident!("method"), quote!(&Self))
            };
            quote! {
                d.#method(#name, [#(#names),*], |#ctx_param this: #this_ty, #(#pats: #types),*| {
                    Self::#ident(this, #ctx_arg #(#args),*)
                });
            }
        }
    })
}

fn export_fn(
    options: &Options,
    paths: &Paths,
    item: ItemFn,
    runtime: &TokenStream,
) -> Result<TokenStream> {
    only_value_allowed(options, "a function")?;
    if let Some(span) = options.no_methods {
        return Err(Error::new(span, "`no_methods` is for a host class"));
    }
    let e = analyse(&item.sig, false)?;
    let name = &e.name;
    let ident = &e.ident;
    let vis = &item.vis;
    let names = e.param_names();
    let module = ident;
    let has_refs = e
        .params
        .iter()
        .any(|(_, t)| matches!(strip(t), Type::Reference(_)));
    // A function with reference parameters is called through a closure that owns them.
    let (imports, callee) = if has_refs {
        let lowered = e
            .params
            .iter()
            .map(|(n, t)| lower(paths, n, t))
            .collect::<Result<Vec<_>>>()?;
        let pats: Vec<&TokenStream> = lowered.iter().map(|l| &l.pat).collect();
        let types: Vec<&TokenStream> = lowered.iter().map(|l| &l.ty).collect();
        let args: Vec<&TokenStream> = lowered.iter().map(|l| &l.arg).collect();
        let ctx_param = e.ctx.as_ref().map(|(n, t)| quote!(#n: #t,));
        let ctx_arg = e.ctx.as_ref().map(|(n, _)| quote!(#n,));
        (
            quote!(
                #[allow(unused_imports)]
                use super::*;
            ),
            quote!(|#ctx_param #(#pats: #types),*| super::#ident(#ctx_arg #(#args),*)),
        )
    } else {
        (quote!(), quote!(super::#ident))
    };
    Ok(quote! {
        #item
        #[allow(non_camel_case_types, dead_code)]
        #vis mod #module {
            #imports
            /// Registers the function with the `Vm`, with its Rust parameter names.
            pub fn register(
                vm: &mut #runtime::Vm,
            ) -> ::core::result::Result<#runtime::Registration<'_>, #runtime::Error> {
                #runtime::__private::register_exported(vm, #name, [#(#names),*], #callee)
            }
            /// Registers the function into a module: `module.add_fn(name::export)?`.
            pub fn export(
                module: &mut #runtime::HostModule<'_>,
            ) -> ::core::result::Result<(), #runtime::Error> {
                #runtime::__private::register_exported_in(module, #name, [#(#names),*], #callee)
            }
        }
    })
}

// ---- the binding module ----

/// `#[colibri::module]`: checks `fn(&mut Module<'_>) -> Result<(), Error>`, keeps the function
/// and writes the hidden `__colibri_binding` module next to it.
pub fn module(args: TokenStream, item: TokenStream) -> TokenStream {
    let parsed = Options::parse.parse2(args);
    let item_copy = item.clone();
    let result = parsed.and_then(|options| {
        for (span, word) in [(options.value, "value"), (options.no_methods, "no_methods")] {
            if let Some(span) = span {
                return Err(Error::new(
                    span,
                    format!(
                        "`{word}` is for `#[colibri::export]`: `#[colibri::module]` takes only `crate = path`"
                    ),
                ));
            }
        }
        let nested = options.nested.is_some();
        let item: ItemFn = syn::parse2(item).map_err(|e| {
            Error::new(
                e.span(),
                "`#[colibri::module]` goes on a function: `fn name(m: &mut colibri::Module<'_>) -> Result<(), colibri::Error>`",
            )
        })?;
        let runtime = match &options.runtime {
            Some(path) => quote!(#path),
            None => quote!(::colibri_runtime),
        };
        module_fn(item, &runtime, nested)
    });
    match result {
        Ok(tokens) => tokens,
        Err(error) => {
            let error = error.to_compile_error();
            quote!(#item_copy #error)
        }
    }
}

const MODULE_SHAPE: &str =
    "a binding module is `fn name(m: &mut colibri::Module<'_>) -> Result<(), colibri::Error>`";

/// The last segment of a type path, when it is one.
fn last_segment(ty: &Type) -> Option<&syn::PathSegment> {
    match strip(ty) {
        Type::Path(p) if p.qself.is_none() => p.path.segments.last(),
        _ => None,
    }
}

fn module_fn(item: ItemFn, runtime: &TokenStream, nested: bool) -> Result<TokenStream> {
    let sig = &item.sig;
    let ident = &sig.ident;
    if let Some(t) = &sig.asyncness {
        return Err(Error::new(t.span, "a binding module cannot be `async`"));
    }
    if let syn::Safety::Unsafe(t) = &sig.safety {
        return Err(Error::new_spanned(t, "a binding module cannot be `unsafe`"));
    }
    if let Some(abi) = &sig.abi {
        return Err(Error::new_spanned(
            abi,
            "a binding module cannot have an `extern` ABI",
        ));
    }
    if sig
        .generics
        .params
        .iter()
        .any(|p| !matches!(p, syn::GenericParam::Lifetime(_)))
        || sig.generics.where_clause.is_some()
    {
        return Err(Error::new_spanned(
            &sig.generics,
            format!("a binding module cannot be generic: {MODULE_SHAPE}"),
        ));
    }
    let mut inputs = sig.inputs.iter();
    let (Some(FnArg::Typed(param)), None) = (inputs.next(), inputs.next()) else {
        return Err(Error::new_spanned(
            &sig.inputs,
            format!("`{ident}` must take exactly one parameter, the module: {MODULE_SHAPE}"),
        ));
    };
    let is_module = matches!(strip(&param.ty), Type::Reference(r)
        if r.mutability.is_some()
            && last_segment(&r.elem).is_some_and(|s| s.ident == "Module" || s.ident == "HostModule"));
    if !is_module {
        return Err(Error::new_spanned(
            &param.ty,
            format!("the parameter must be `&mut colibri::Module<'_>`: {MODULE_SHAPE}"),
        ));
    }
    let returns_result = match &sig.output {
        syn::ReturnType::Type(_, ty) => last_segment(ty).is_some_and(|seg| {
            let syn::PathArguments::AngleBracketed(args) = &seg.arguments else {
                return false;
            };
            let types: Vec<&Type> = args
                .args
                .iter()
                .filter_map(|a| match a {
                    syn::GenericArgument::Type(t) => Some(t),
                    _ => None,
                })
                .collect();
            seg.ident == "Result"
                && types.len() == 2
                && matches!(strip(types[0]), Type::Tuple(t) if t.elems.is_empty())
                && last_segment(types[1]).is_some_and(|s| s.ident == "Error")
        }),
        syn::ReturnType::Default => false,
    };
    if !returns_result {
        return Err(Error::new_spanned(
            &sig.output,
            format!(
                "`{ident}` must return `Result<(), colibri::Error>`, because registering can fail: {MODULE_SHAPE}"
            ),
        ));
    }
    let name = ident.unraw().to_string();
    // The module sits next to the function, and a runner reaches it as `krate::__colibri_binding`.
    let root_check = (!nested).then(|| {
        quote_spanned! {ident.span()=>
            const _: () = {
                let path = ::core::module_path!().as_bytes();
                let mut i = 0;
                while i < path.len() {
                    if path[i] == b':' {
                        ::core::panic!(
                            "`#[colibri::module]` goes at the crate root: a runner finds the binding as `krate::__colibri_binding`"
                        );
                    }
                    i += 1;
                }
            };
        }
    });
    Ok(quote! {
        #item
        #root_check
        /// What a generated runner calls: `__colibri_binding::register(&mut vm, "native")`
        /// registers the binding under the import name the manifest chose.
        #[doc(hidden)]
        pub mod __colibri_binding {
            /// The Rust name of the binding function. Informational: the import name is the
            /// manifest key.
            pub const NAME: &str = #name;
            /// Registers the binding's module under `name` in `vm`. A second registration
            /// under the same name is an error.
            pub fn register(
                vm: &mut #runtime::Vm,
                name: &str,
            ) -> ::core::result::Result<(), #runtime::Error> {
                #runtime::__private::run_binding(vm, #name, name, super::#ident)
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &str, item: &str) -> String {
        export(args.parse().unwrap(), item.parse().unwrap()).to_string()
    }

    fn error_of(args: &str, item: &str) -> String {
        let out = run(args, item);
        assert!(out.contains("compile_error"), "no error in: {out}");
        out
    }

    #[test]
    fn export_refuses_the_nested_option_of_module() {
        let out = error_of("nested", "pub struct P { pub x: f32 }");
        assert!(
            out.contains("`nested` is for `#[colibri::module]`"),
            "{out}"
        );
        let out = error_of("value, nested", "pub struct P { pub x: f32 }");
        assert!(out.contains("`nested` is for"), "{out}");
    }

    #[test]
    fn a_value_struct_converts_field_by_field_and_asserts_copy() {
        let out = run(
            "value",
            "#[derive(Clone, Copy)] pub struct Vec3 { pub x: f32, pub y: f32 }",
        );
        assert!(out.contains("define_struct"), "{out}");
        assert!(out.contains("assert_copy"), "{out}");
        assert!(out.contains("new_struct"), "{out}");
        assert!(out.contains("colibri_runtime"), "{out}");
        assert!(!out.contains("compile_error"), "{out}");
    }

    #[test]
    fn a_private_field_is_an_error_naming_it() {
        let out = error_of("value", "struct P { pub x: f32, y: f32 }");
        assert!(out.contains("the field `y` must be `pub`"), "{out}");
    }

    #[test]
    fn value_structs_need_named_fields_and_no_generics() {
        assert!(error_of("value", "struct P(pub f32);").contains("named fields"));
        assert!(error_of("value", "struct P<T> { pub x: T }").contains("cannot be generic"));
    }

    #[test]
    fn enums_must_be_fieldless_without_numbers_and_by_value() {
        let ok = run("value", "enum Dir { North, South }");
        assert!(ok.contains("define_enum"), "{ok}");
        assert!(error_of("value", "enum E { A(i64) }").contains("has a payload"));
        assert!(error_of("value", "enum E { A = 3 }").contains("cannot give its variants numbers"));
        assert!(error_of("", "enum E { A }").contains("exported by value"));
    }

    #[test]
    fn a_plain_struct_is_a_host_class() {
        let out = run("", "pub struct World { gravity: f32 }");
        assert!(out.contains("HostType"), "{out}");
        assert!(out.contains("ExportedMethods"), "{out}");
        let none = run("no_methods", "struct Tag;");
        assert!(!none.contains("ExportedMethods"), "{none}");
        assert!(error_of("value, no_methods", "struct P { pub x: f32 }").contains("no_methods"));
    }

    #[test]
    fn an_impl_exports_its_pub_fns_with_real_names() {
        let out = run(
            "",
            "impl World { pub fn new(gravity: f32) -> World { todo!() } pub fn step(&mut self, dt: f32) {} pub fn pos(&self, cx_free: i64) -> f32 { 0.0 } fn hidden(&self) {} }",
        );
        assert!(out.contains("static_fn (\"new\" , [\"gravity\"]"), "{out}");
        assert!(out.contains("method_mut (\"step\" , [\"dt\"]"), "{out}");
        assert!(out.contains("method (\"pos\" , [\"cx_free\"]"), "{out}");
        assert!(!out.contains("\"hidden\""), "{out}");
    }

    #[test]
    fn a_leading_context_is_passed_through_and_not_named() {
        let out = run(
            "",
            "impl World { pub fn spawn(&mut self, cx: &mut colibri::Ctx, n: i64) {} }",
        );
        assert!(out.contains("[\"n\"]"), "{out}");
        assert!(out.contains("cx : & mut colibri :: Ctx"), "{out}");
        assert!(out.contains("Self :: spawn (this , cx , n)"), "{out}");
    }

    #[test]
    fn methods_that_cannot_be_exported_say_why() {
        assert!(error_of("", "impl W { pub fn eat(self) {} }").contains("by value"));
        assert!(error_of("", "impl W { pub async fn f(&self) {} }").contains("async"));
        assert!(error_of("", "impl W { pub fn f<T>(&self, t: T) {} }").contains("generic"));
        assert!(
            error_of("", "impl W { pub fn f(&self, (a, b): (i64, i64)) {} }")
                .contains("plain name")
        );
        assert!(error_of("", "impl<T> W<T> { pub fn f(&self) {} }").contains("cannot be generic"));
        assert!(error_of("", "impl Show for W { pub fn f(&self) {} }").contains("inherent"));
        assert!(error_of("value", "impl W { }").contains("not for an `impl`"));
    }

    fn module_run(args: &str, item: &str) -> String {
        module(args.parse().unwrap(), item.parse().unwrap()).to_string()
    }

    #[test]
    fn a_function_module_has_an_export_that_registers_into_a_module() {
        let out = run("", "pub fn greet(who: &str) -> String { todo!() }");
        assert!(
            out.contains("pub fn export (module : & mut :: colibri_runtime :: HostModule"),
            "{out}"
        );
        assert!(
            out.contains("register_exported_in (module , \"greet\""),
            "{out}"
        );
    }

    #[test]
    fn a_binding_module_writes_name_and_register() {
        let out = module_run(
            "",
            "fn physics(m: &mut colibri::Module<'_>) -> Result<(), colibri::Error> { Ok(()) }",
        );
        assert!(!out.contains("compile_error"), "{out}");
        assert!(out.contains("mod __colibri_binding"), "{out}");
        assert!(out.contains("const NAME : & str = \"physics\""), "{out}");
        assert!(
            out.contains("fn register (vm : & mut :: colibri_runtime :: Vm , name : & str ,)"),
            "{out}"
        );
        assert!(
            out.contains("run_binding (vm , \"physics\" , name , super :: physics)"),
            "{out}"
        );
        assert!(out.contains("goes at the crate root"), "{out}");
        let nested = module_run(
            "nested",
            "fn p(m: &mut Module<'_>) -> Result<(), Error> { Ok(()) }",
        );
        assert!(!nested.contains("goes at the crate root"), "{nested}");
        assert!(nested.contains("run_binding"), "{nested}");
        // The spellings a crate may use.
        for sig in [
            "pub fn p(m: &mut Module) -> Result<(), Error> { Ok(()) }",
            "fn p<'a>(m: &mut colibri::HostModule<'a>) -> ::core::result::Result<(), colibri_runtime::Error> { Ok(()) }",
        ] {
            let out = module_run("", sig);
            assert!(!out.contains("compile_error"), "{sig}: {out}");
        }
        let renamed = module_run(
            "crate = my::rt",
            "fn p(m: &mut Module<'_>) -> Result<(), Error> { Ok(()) }",
        );
        assert!(renamed.contains("my :: rt :: Vm"), "{renamed}");
    }

    #[test]
    fn a_binding_module_of_the_wrong_shape_says_the_shape() {
        for (item, why) in [
            (
                "fn p() -> Result<(), Error> { Ok(()) }",
                "exactly one parameter",
            ),
            (
                "fn p(m: &mut Module<'_>, n: i64) -> Result<(), Error> { Ok(()) }",
                "exactly one parameter",
            ),
            (
                "fn p(m: &Module<'_>) -> Result<(), Error> { Ok(()) }",
                "`&mut colibri::Module<'_>`",
            ),
            (
                "fn p(m: &mut Vm) -> Result<(), Error> { Ok(()) }",
                "`&mut colibri::Module<'_>`",
            ),
            (
                "fn p(m: Module<'_>) -> Result<(), Error> { Ok(()) }",
                "`&mut colibri::Module<'_>`",
            ),
            (
                "fn p(m: &mut Module<'_>) {}",
                "must return `Result<(), colibri::Error>`",
            ),
            (
                "fn p(m: &mut Module<'_>) -> Result<i64, Error> { todo!() }",
                "must return",
            ),
            (
                "fn p(m: &mut Module<'_>) -> Result<(), String> { todo!() }",
                "must return",
            ),
            (
                "fn p(m: &mut Module<'_>) -> Option<()> { todo!() }",
                "must return",
            ),
            (
                "fn p<T>(m: &mut Module<'_>) -> Result<(), Error> { Ok(()) }",
                "cannot be generic",
            ),
            (
                "async fn p(m: &mut Module<'_>) -> Result<(), Error> { Ok(()) }",
                "async",
            ),
            (
                "unsafe fn p(m: &mut Module<'_>) -> Result<(), Error> { Ok(()) }",
                "unsafe",
            ),
            ("struct S;", "goes on a function"),
        ] {
            let out = module_run("", item);
            assert!(out.contains("compile_error"), "{item}: {out}");
            assert!(out.contains(why), "{item}: {out}");
            // The function is kept, so the editor still knows it.
            if item.contains("fn p") {
                assert!(out.contains("fn p"), "{item}: {out}");
                assert!(!out.contains("mod __colibri_binding"), "{item}: {out}");
            }
        }
        let out = module_run(
            "value",
            "fn p(m: &mut Module<'_>) -> Result<(), Error> { Ok(()) }",
        );
        assert!(out.contains("takes only `crate = path`"), "{out}");
    }

    #[test]
    fn a_function_gets_a_registering_module_of_its_name() {
        let out = run("", "pub fn submit_job(x: f32, y: f32) -> i64 { 0 }");
        assert!(out.contains("mod submit_job"), "{out}");
        assert!(
            out.contains(
                "register_exported (vm , \"submit_job\" , [\"x\" , \"y\"] , super :: submit_job)"
            ),
            "{out}"
        );
        assert!(error_of("", "fn f<T>() {}").contains("generic"));
        assert!(error_of("", "fn f(self_: i64, (a, b): (i64, i64)) {}").contains("plain name"));
    }

    #[test]
    fn other_items_and_bad_options_are_errors() {
        assert!(error_of("", "const X: i64 = 1;").contains("struct, an enum"));
        assert!(error_of("shiny", "struct S;").contains("unknown option `shiny`"));
    }

    #[test]
    fn the_runtime_path_can_be_renamed() {
        let out = run(
            "value, crate = my::rt",
            "#[derive(Clone, Copy)] struct P { pub x: i64 }",
        );
        assert!(out.contains("my :: rt :: __private"), "{out}");
        assert!(!out.contains("colibri_runtime"), "{out}");
    }

    #[test]
    fn the_item_is_kept_next_to_an_error() {
        let out = error_of("value", "struct P { y: f32 }");
        assert!(out.contains("struct P"), "{out}");
    }

    #[test]
    fn a_str_parameter_is_a_string_local_that_is_borrowed() {
        let out = run("", "impl W { pub fn say(&self, text: &str) {} }");
        assert!(
            out.contains("text : < str as :: colibri_runtime :: __private :: RefParam > :: Owned"),
            "{out}"
        );
        assert!(out.contains("Self :: say (this , & text)"), "{out}");
        assert!(out.contains("[\"text\"]"), "{out}");
        assert!(!out.contains("compile_error"), "{out}");
    }

    #[test]
    fn a_slice_parameter_is_a_vec_local() {
        let out = run(
            "",
            "impl W { pub fn sum(&self, xs: &[f32]) -> f32 { 0.0 } }",
        );
        assert!(
            out.contains("< [f32] as :: colibri_runtime :: __private :: RefParam > :: Owned"),
            "{out}"
        );
        assert!(out.contains("Self :: sum (this , & xs)"), "{out}");
    }

    #[test]
    fn a_class_parameter_borrows_shared_or_exclusive() {
        let out = run(
            "",
            "impl World { pub fn pos(&self, body: &Body) {} pub fn push(&mut self, body: &mut Body, by: f32) {} }",
        );
        assert!(
            out.contains("body : < Body as :: colibri_runtime :: __private :: RefParam > :: Owned"),
            "{out}"
        );
        assert!(out.contains("Self :: pos (this , & body)"), "{out}");
        assert!(out.contains("mut body : < Body as :: colibri_runtime :: __private :: MutParam > :: Owned , by : f32"), "{out}");
        assert!(
            out.contains("Self :: push (this , & mut body , by)"),
            "{out}"
        );
    }

    #[test]
    fn a_static_fn_and_a_function_take_references_too() {
        let out = run("", "impl W { pub fn named(name: &str) -> W { todo!() } }");
        assert!(out.contains("Self :: named (& name)"), "{out}");
        let out = run(
            "",
            "pub fn join(a: &str, n: i64, b: &mut Body) -> i64 { 0 }",
        );
        assert!(
            out.contains("register_exported (vm , \"join\" , [\"a\" , \"n\" , \"b\"] , | a :"),
            "{out}"
        );
        assert!(out.contains("super :: join (& a , n , & mut b)"), "{out}");
        assert!(out.contains("use super :: *"), "{out}");
        // Without references the function itself is registered, as before.
        let plain = run("", "pub fn plain(a: i64) {}");
        assert!(plain.contains("super :: plain)"), "{plain}");
        assert!(!plain.contains("use super"), "{plain}");
    }

    #[test]
    fn value_types_and_enums_can_be_borrowed() {
        let out = run("value", "#[derive(Clone, Copy)] struct P { pub x: f32 }");
        assert!(out.contains("RefParam for P"), "{out}");
        let out = run("value", "enum E { A }");
        assert!(out.contains("RefParam for E"), "{out}");
    }

    #[test]
    fn a_context_is_still_not_a_parameter() {
        let out = run("", "impl W { pub fn f(&self, cx: &mut Ctx, s: &str) {} }");
        assert!(out.contains("[\"s\"]"), "{out}");
        assert!(out.contains("Self :: f (this , cx , & s)"), "{out}");
    }

    #[test]
    fn a_borrowing_result_says_to_return_an_owned_value() {
        for item in [
            "impl W { pub fn name(&self) -> &str { \"\" } }",
            "impl W { pub fn body(&self) -> &Body { todo!() } }",
            "impl W { pub fn new() -> &'a str { \"\" } }",
            "impl W { pub fn n(&self) -> Option<&str> { None } }",
            "impl W { pub fn n(&self) -> Result<&Body, String> { todo!() } }",
            "impl W { pub fn n(&self) -> (i64, &[i64]) { todo!() } }",
            "fn f() -> (&str) { \"\" }",
        ] {
            let out = error_of("", item);
            assert!(out.contains("return an owned value"), "{item}: {out}");
        }
    }

    #[test]
    fn a_static_result_is_copied_out_and_still_compiles() {
        let out = run("", "fn v() -> &'static str { \"v1\" }");
        assert!(!out.contains("compile_error"), "{out}");
        let out = run("", "fn v() -> &'static [i64] { &[] }");
        assert!(!out.contains("compile_error"), "{out}");
        let out = run(
            "",
            "impl W { pub fn v(&self) -> Option<&'static str> { None } }",
        );
        assert!(!out.contains("compile_error"), "{out}");
    }

    #[test]
    fn a_parenthesised_reference_parameter_is_still_lowered() {
        let out = run("", "impl W { pub fn f(&self, s: (&str)) {} }");
        assert!(out.contains("RefParam"), "{out}");
    }

    #[test]
    fn a_mutable_str_or_slice_is_refused() {
        let out = error_of("", "impl W { pub fn f(&self, s: &mut str) {} }");
        assert!(out.contains("`&mut str` or `&mut [T]`"), "{out}");
        assert!(error_of("", "fn f(s: &mut [i64]) {}").contains("`&mut str` or `&mut [T]`"));
    }
}
