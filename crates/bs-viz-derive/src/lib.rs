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

use bs_viz_spec::{FieldSpec, Format, TypeViewSpec, VariantSpec};

#[proc_macro_derive(DebugView, attributes(bs_viz))]
pub fn derive_debug_view(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand(input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand(input: DeriveInput) -> syn::Result<TokenStream2> {
    // Phase 4 step 3: generics are now allowed. We still emit a
    // single spec entry per *type definition* (not per
    // monomorphisation) — the v0 demangler hands BugStalker
    // names like `Wrap<i32>`, and the registry's lookup strips
    // the generic-args before searching, so `Wrap<i32>` and
    // `Wrap<String>` both resolve to the one spec emitted from
    // `pub struct Wrap<T> { ... }`. This is correct as long as
    // the field set + summary template are generic-uniform; the
    // common case for crate authors.
    let (fields, enum_variants): (Option<&syn::Fields>, Option<&syn::DataEnum>) =
        match &input.data {
            Data::Struct(s) => (Some(&s.fields), None),
            // Phase 4 step 8: enums also carry per-variant
            // attributes — `#[bs_viz(summary = "...", tag = "...")]`
            // on individual variants, plus per-field overrides
            // scoped to the variant.
            Data::Enum(e) => (None, Some(e)),
            Data::Union(_) => {
                return Err(syn::Error::new(
                    input.span(),
                    "#[derive(DebugView)] does not support unions",
                ));
            }
        };

    let summary = parse_type_attrs(&input.attrs)?;
    let mut field_specs = Vec::new();
    let field_iter: Box<dyn Iterator<Item = &syn::Field>> = match fields {
        Some(fs) => Box::new(fs.iter()),
        None => Box::new(std::iter::empty()),
    };
    for (idx, f) in field_iter.enumerate() {
        // Step 5: tuple structs are now supported. Rust emits
        // tuple-struct fields under DWARF as `__0`, `__1`, etc.
        // — we mirror that name in the spec so the registry's
        // field lookup matches what the renderer sees at debug
        // time. Named fields still use their literal name.
        // Unit structs (no fields) fall out of this loop with
        // `field_specs` empty, which the spec format already
        // tolerates.
        let field_name = match f.ident.as_ref() {
            Some(ident) => ident.to_string(),
            None => format!("__{idx}"),
        };
        let attrs = parse_field_attrs(&f.attrs)?;
        field_specs.push(FieldSpec {
            name: field_name,
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
    // Step 8: collect per-variant specs when this is an enum.
    let variant_specs = match enum_variants {
        Some(e) => collect_variant_specs(e)?,
        None => Vec::new(),
    };

    let spec = TypeViewSpec {
        type_name: local_name.clone(),
        summary,
        fields: field_specs,
        variants: variant_specs,
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

/// Walk a `Data::Enum`, build a `VariantSpec` per variant. Each
/// variant's `#[bs_viz(...)]` attributes parse the same way as a
/// type's, with `summary` + `tag` recognised; field-level
/// attributes parse identically to struct fields. Tuple-style
/// variants name their fields `__0`, `__1`, ... matching the
/// struct convention.
fn collect_variant_specs(e: &syn::DataEnum) -> syn::Result<Vec<VariantSpec>> {
    let mut out = Vec::with_capacity(e.variants.len());
    for v in &e.variants {
        let (summary, tag) = parse_variant_attrs(&v.attrs)?;
        let mut field_specs = Vec::new();
        for (idx, f) in v.fields.iter().enumerate() {
            let field_name = match f.ident.as_ref() {
                Some(ident) => ident.to_string(),
                None => format!("__{idx}"),
            };
            let attrs = parse_field_attrs(&f.attrs)?;
            field_specs.push(FieldSpec {
                name: field_name,
                rename: attrs.rename,
                hidden: attrs.skip,
                format: attrs.format.unwrap_or(Format::Default),
            });
        }
        out.push(VariantSpec {
            name: v.ident.to_string(),
            summary,
            tag,
            fields: field_specs,
        });
    }
    Ok(out)
}

/// Parse `#[bs_viz(summary = "...", tag = "...")]` on an enum
/// variant. Both attributes are optional.
fn parse_variant_attrs(attrs: &[syn::Attribute]) -> syn::Result<(Option<String>, Option<String>)> {
    let mut summary = None;
    let mut tag = None;
    for attr in attrs {
        if !attr.path().is_ident("bs_viz") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("summary") {
                let v: syn::LitStr = meta.value()?.parse()?;
                summary = Some(v.value());
                Ok(())
            } else if meta.path.is_ident("tag") {
                let v: syn::LitStr = meta.value()?.parse()?;
                tag = Some(v.value());
                Ok(())
            } else {
                Err(meta.error(
                    "unknown #[bs_viz(...)] attribute on enum variant \
                     (step 8 supports `summary = \"...\"` and `tag = \"...\"`)",
                ))
            }
        })?;
    }
    Ok((summary, tag))
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
