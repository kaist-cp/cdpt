use proc_macro::TokenStream;
use quote::{quote, quote_spanned};
use syn::spanned::Spanned;
use syn::{Data, DeriveInput, Fields, GenericArgument, PathArguments, Type, parse_macro_input};

#[proc_macro_derive(TraceObj)]
pub fn derive_trace_obj(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let unroot_body = gen_body(&input.data, "unroot");
    let shade_body = gen_body(&input.data, "shade");

    let expanded = quote! {
        unsafe impl #impl_generics cdpt::TraceObj for #name #ty_generics #where_clause {
            fn unroot_outgoings(&self, guard: &cdpt::Guard) {
                #unroot_body
            }

            fn shade_outgoings(&self, guard: &cdpt::Guard) {
                #shade_body
            }
        }
        impl #impl_generics Drop for #name #ty_generics #where_clause {
            fn drop(&mut self) {
                // Does nothing, but just recursively calls the destructors
                // of all the fields of this type.
                // Note that a finalization is not supported.
            }
        }
    };

    TokenStream::from(expanded)
}

fn gen_body(data: &Data, method: &str) -> proc_macro2::TokenStream {
    let method_ident = quote::format_ident!("{}", method);
    match data {
        Data::Struct(data) => gen_fields_trace(&data.fields, &method_ident, None),
        Data::Enum(data) => {
            let variants = data.variants.iter().map(|variant| {
                let variant_ident = &variant.ident;
                let fields_trace =
                    gen_fields_trace(&variant.fields, &method_ident, Some(variant_ident));
                match &variant.fields {
                    Fields::Named(fields) => {
                        let names = fields.named.iter().map(|f| &f.ident);
                        quote! {
                            Self::#variant_ident { #(#names),* } => {
                                #fields_trace
                            }
                        }
                    }
                    Fields::Unnamed(fields) => {
                        let names =
                            (0..fields.unnamed.len()).map(|i| quote::format_ident!("f{}", i));
                        quote! {
                            Self::#variant_ident( #(#names),* ) => {
                                #fields_trace
                            }
                        }
                    }
                    Fields::Unit => {
                        quote! {
                            Self::#variant_ident => {}
                        }
                    }
                }
            });
            quote! {
                match self {
                    #(#variants),*
                }
            }
        }
        Data::Union(_) => panic!("TraceObj cannot be derived for unions"),
    }
}

fn gen_fields_trace(
    fields: &Fields,
    method: &syn::Ident,
    variant: Option<&syn::Ident>,
) -> proc_macro2::TokenStream {
    let recurse = fields.iter().enumerate().filter_map(|(i, field)| {
        let expr = if let Some(field_ident) = &field.ident {
            if variant.is_some() {
                quote! { #field_ident }
            } else {
                quote! { self.#field_ident }
            }
        } else {
            let idx = quote::format_ident!("f{}", i);
            if variant.is_some() {
                quote! { #idx }
            } else {
                let index = syn::Index::from(i);
                quote! { self.#index }
            }
        };

        gen_trace_for_type(&field.ty, &expr, method)
    });

    quote! {
        #(#recurse)*
    }
}

fn gen_trace_for_type(
    ty: &Type,
    expr: &proc_macro2::TokenStream,
    method: &syn::Ident,
) -> Option<proc_macro2::TokenStream> {
    match ty {
        Type::Path(p) => {
            let last = p.path.segments.last().unwrap();
            let ident = &last.ident;

            if ident == "Shared" || ident == "AtomicShared" || ident == "AtomicSharedOption" {
                return Some(quote_spanned! { ty.span() =>
                    #expr.#method(guard);
                });
            }

            if ident == "Option"
                && let PathArguments::AngleBracketed(args) = &last.arguments
                && let Some(GenericArgument::Type(inner)) = args.args.first()
                && let Some(inner_trace) = gen_trace_for_type(inner, &quote! { __inner }, method)
            {
                return Some(quote_spanned! { ty.span() =>
                    if let Some(__inner) = &#expr {
                        #inner_trace
                    }
                });
            }

            if (ident == "Vec" || ident == "VecDeque" || ident == "ArrayVec")
                && let PathArguments::AngleBracketed(args) = &last.arguments
                && let Some(GenericArgument::Type(inner)) = args.args.first()
                && let Some(inner_trace) = gen_trace_for_type(inner, &quote! { __inner }, method)
            {
                return Some(quote_spanned! { ty.span() =>
                    for __inner in &#expr {
                        #inner_trace
                    }
                });
            }

            if (ident == "Box" || ident == "Arc")
                && let PathArguments::AngleBracketed(args) = &last.arguments
                && let Some(GenericArgument::Type(inner)) = args.args.first()
            {
                let derefed = quote! { (*#expr) };
                if let Some(inner_trace) = gen_trace_for_type(inner, &derefed, method) {
                    return Some(inner_trace);
                }
            }

            if ident == "Result"
                && let PathArguments::AngleBracketed(args) = &last.arguments
            {
                let mut result_trace = quote! {};
                if let Some(GenericArgument::Type(ok_ty)) = args.args.first() {
                    if let Some(ok_trace) = gen_trace_for_type(ok_ty, &quote! { __ok }, method) {
                        result_trace.extend(quote! {
                            cdpt::export::Result::Ok(__ok) => { #ok_trace }
                        });
                    } else {
                        result_trace.extend(quote! {
                            cdpt::export::Result::Ok(_) => {}
                        });
                    }
                }
                if let Some(GenericArgument::Type(err_ty)) = args.args.get(1) {
                    if let Some(err_trace) = gen_trace_for_type(err_ty, &quote! { __err }, method) {
                        result_trace.extend(quote! {
                            cdpt::export::Result::Err(__err) => { #err_trace }
                        });
                    } else {
                        result_trace.extend(quote! {
                            cdpt::export::Result::Err(_) => {}
                        });
                    }
                }

                if !result_trace.is_empty() {
                    return Some(quote_spanned! { ty.span() =>
                        match &#expr {
                            #result_trace
                        }
                    });
                }
            }
        }
        Type::Tuple(t) => {
            let elems = t.elems.iter().enumerate().filter_map(|(i, elem)| {
                let index = syn::Index::from(i);
                gen_trace_for_type(elem, &quote! { #expr.#index }, method)
            });
            let elems: Vec<_> = elems.collect();
            if !elems.is_empty() {
                return Some(quote_spanned! { ty.span() =>
                    #(#elems)*
                });
            }
        }
        Type::Array(a) => {
            if let Some(inner_trace) = gen_trace_for_type(&a.elem, &quote! { __inner }, method) {
                return Some(quote_spanned! { ty.span() =>
                    for __inner in &#expr {
                        #inner_trace
                    }
                });
            }
        }
        Type::Slice(s) => {
            if let Some(inner_trace) = gen_trace_for_type(&s.elem, &quote! { __inner }, method) {
                return Some(quote_spanned! { ty.span() =>
                    for __inner in &#expr {
                        #inner_trace
                    }
                });
            }
        }
        Type::Reference(r) => {
            if let Some(inner_trace) = gen_trace_for_type(&r.elem, &quote! { __inner }, method) {
                return Some(quote_spanned! { ty.span() =>
                    let __inner = &#expr;
                    #inner_trace
                });
            }
        }
        _ => {}
    }
    None
}
