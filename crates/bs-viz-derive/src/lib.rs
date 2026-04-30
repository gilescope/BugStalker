// SPDX-License-Identifier: MIT
//! `#[derive(DebugView)]` — the front door for Tier A
//! visualisers.
//!
//! At expansion time we read the type definition + any
//! `#[bs_viz(...)]` attributes, build a [`TypeViewSpec`], encode
//! it via [`bs_viz_spec::encode`] (so the wire format is decided
//! at one place — the spec crate), and emit a `static [u8; N]`
//! placed in `.bs_viz_spec` (or its Mach-O equivalent). The
//! `#[used]` attribute keeps the linker from garbage-collecting
//! the spec; BugStalker scans the section at attach time.
//!
//! Step 1 supports a deliberately small attribute surface:
//!
//! - type-level `#[bs_viz(summary = "tmpl")]`
//! - field-level `#[bs_viz(skip)]`
//! - field-level `#[bs_viz(rename = "name")]`
//! - field-level `#[bs_viz(format = "hex" | "bin" | "oct" |
//!   "iso8601" | "duration" | "utf8" | "hexdump")]`
//!
//! Generic types, enums, unions, the `custom` escape hatch, and
//! the variant-level attributes are all out-of-scope for step 1.
//! They produce a clear compile error so users hit a wall rather
//! than a silently-misencoded spec.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Data, DeriveInput, LitByteStr, parse_macro_input, spanned::Spanned};

use bs_viz_spec::{FieldSpec, Format, TypeViewSpec};

#[proc_macro_derive(DebugView, attributes(bs_viz))]
pub fn derive_debug_view(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand(input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand(input: DeriveInput) -> syn::Result<TokenStream2> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new(
            input.generics.span(),
            "step 1 of #[derive(DebugView)] does not support generics yet \
             — track the per-monomorphisation lowering in phase-4 batch 2",
        ));
    }
    let fields = match &input.data {
        Data::Struct(s) => &s.fields,
        Data::Enum(_) => {
            return Err(syn::Error::new(
                input.span(),
                "step 1 of #[derive(DebugView)] only handles structs; \
                 enums land in a later phase-4 batch",
            ));
        }
        Data::Union(_) => {
            return Err(syn::Error::new(
                input.span(),
                "#[derive(DebugView)] does not support unions",
            ));
        }
    };

    let summary = parse_type_attrs(&input.attrs)?;
    let mut field_specs = Vec::new();
    for f in fields.iter() {
        let Some(name_ident) = f.ident.as_ref() else {
            return Err(syn::Error::new(
                f.span(),
                "step 1 of #[derive(DebugView)] only handles named fields; \
                 tuple/unit structs land in a later phase-4 batch",
            ));
        };
        let attrs = parse_field_attrs(&f.attrs)?;
        field_specs.push(FieldSpec {
            name: name_ident.to_string(),
            rename: attrs.rename,
            hidden: attrs.skip,
            format: attrs.format.unwrap_or(Format::Default),
        });
    }

    // The macro only knows the local-module name of the type
    // (e.g. `Person`); the on-wire name needs to be the
    // fully-qualified demangled form (`my_crate::Person`).
    // `module_path!()` gives us the prefix at expansion's
    // *actual* call site, so we splice it in via a `concat!()`
    // at runtime — wait, `module_path!` is a runtime macro. To
    // keep encoding fully at compile time, we instead emit a
    // const fn that produces the bytes lazily, inside the
    // `static`'s init. The `concat!` of `module_path!()` and
    // `"::Name"` is fine — the proc-macro emits Rust source, the
    // compiler evaluates it.
    let ty_ident = &input.ident;
    let local_name = ty_ident.to_string();

    // Encode at proc-macro time using a placeholder type_name;
    // patch the bytes at the language level by rebuilding the
    // header to include `module_path!()`. Concretely: the macro
    // emits a `static` whose initialiser calls `bs_viz_spec`'s
    // `encode_with_module_path` helper at const-eval... no,
    // const-eval can't allocate. We sidestep all of that by
    // doing two things:
    //
    // 1. The macro encodes the *full* spec at expansion time
    //    using the *local* type name. This is strictly enough
    //    information for BugStalker to identify which type this
    //    spec belongs to within the binary's symbol table —
    //    BugStalker resolves type names by *partial match* on
    //    the suffix (`::Person`) so the missing module prefix
    //    is fine.
    // 2. The static's name is also unique-suffixed with the
    //    type name, so multiple derives in one crate don't
    //    collide on the same static identifier.
    //
    // If proves limiting (e.g. two crates each defining
    // `Person`), batch 2 will introduce a `module_path!()`-aware
    // resolver. For step 1, suffix matching is enough.
    let spec = TypeViewSpec {
        type_name: local_name.clone(),
        summary,
        fields: field_specs,
    };
    let bytes = bs_viz_spec::encode(&spec);
    let byte_lit = LitByteStr::new(&bytes, ty_ident.span());
    let n = bytes.len();

    // Synthesise a unique static identifier so two derives in
    // one crate don't collide.
    let static_ident = quote::format_ident!("__BS_VIZ_SPEC_{}", ty_ident);

    // Edition 2024 made `link_section` and `used` unsafe
    // attributes, so wrap them in `unsafe(...)`. The form is
    // also accepted on 2021 since rustc 1.82, so we don't need
    // a per-edition split — emit one form that works everywhere.
    let section_attrs = quote! {
        #[cfg_attr(target_os = "linux", unsafe(link_section = ".bs_viz_spec"))]
        #[cfg_attr(any(target_os = "macos", target_os = "ios"),
                   unsafe(link_section = "__DATA,__bs_viz_spec"))]
        #[used]
    };

    Ok(quote! {
        #[allow(non_upper_case_globals)]
        const _: () = {
            #section_attrs
            static #static_ident: [u8; #n] = *#byte_lit;
        };
    })
}

/// Parsed `#[bs_viz(...)]` data on a struct itself. Only
/// `summary = "..."` is supported in step 1.
fn parse_type_attrs(attrs: &[syn::Attribute]) -> syn::Result<Option<String>> {
    let mut summary: Option<String> = None;
    for attr in attrs {
        if !attr.path().is_ident("bs_viz") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("summary") {
                let v = meta.value()?;
                let s: syn::LitStr = v.parse()?;
                summary = Some(s.value());
                Ok(())
            } else {
                Err(meta.error(
                    "unknown #[bs_viz(...)] attribute on type \
                     (step 1 supports `summary = \"...\"` only)",
                ))
            }
        })?;
    }
    Ok(summary)
}

#[derive(Default)]
struct FieldAttrs {
    skip: bool,
    rename: Option<String>,
    format: Option<Format>,
}

fn parse_field_attrs(attrs: &[syn::Attribute]) -> syn::Result<FieldAttrs> {
    let mut out = FieldAttrs::default();
    for attr in attrs {
        if !attr.path().is_ident("bs_viz") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("skip") {
                out.skip = true;
                Ok(())
            } else if meta.path.is_ident("rename") {
                let v: syn::LitStr = meta.value()?.parse()?;
                out.rename = Some(v.value());
                Ok(())
            } else if meta.path.is_ident("format") {
                let v: syn::LitStr = meta.value()?.parse()?;
                out.format = Some(parse_format(&v.value(), v.span())?);
                Ok(())
            } else {
                Err(meta.error(
                    "unknown #[bs_viz(...)] attribute on field \
                     (step 1 supports `skip`, `rename = \"...\"`, \
                     `format = \"hex|bin|oct|iso8601|duration|utf8|hexdump\"`)",
                ))
            }
        })?;
    }
    Ok(out)
}

fn parse_format(s: &str, span: proc_macro2::Span) -> syn::Result<Format> {
    Ok(match s {
        "hex" => Format::Hex,
        "bin" => Format::Bin,
        "oct" => Format::Oct,
        "iso8601" => Format::Iso8601,
        "duration" => Format::Duration,
        "utf8" => Format::Utf8,
        "hexdump" => Format::Hexdump,
        other => {
            return Err(syn::Error::new(
                span,
                format!(
                    "unknown format `{other}`; valid: hex, bin, oct, iso8601, \
                     duration, utf8, hexdump"
                ),
            ));
        }
    })
}
