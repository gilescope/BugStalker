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

    let type_attrs = parse_type_attrs(&input.attrs)?;
    let summary = type_attrs.summary;
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
    // Step 9 + 10 — type-name resolution.
    //
    // - `#[bs_viz(name = "fully::qualified")]` → the recorded
    //   `type_name` is the user-provided string verbatim. The
    //   macro emits a `static [u8; N]` initialised by
    //   `bs_viz_spec::assemble_verbatim`.
    // - No `name` attr → step 10 composes the recorded name
    //   from `module_path!()` + `"::"` + the local ident at the
    //   user crate's compile time, via
    //   `bs_viz_spec::assemble_with_module_path`. The registry
    //   then sees the same fully-qualified form the v0
    //   demangler produces, and exact-match resolution Just
    //   Works without a suffix-match fallback.
    let explicit_name = type_attrs.name.clone();
    let local_ident_str = ty_ident.to_string();

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

    // Encode the proc-macro-known portion of the payload (every
    // byte after the type_name str). The runtime type_name part
    // is composed at user-crate compile time by the const-fn
    // assembler.
    let suffix_spec = TypeViewSpec {
        // type_name doesn't matter for the suffix encoding.
        type_name: String::new(),
        summary,
        fields: field_specs,
        variants: variant_specs,
    };
    let suffix_bytes = bs_viz_spec::encode_payload_suffix(&suffix_spec);
    let suffix_lit = LitByteStr::new(&suffix_bytes, ty_ident.span());
    let suffix_len = suffix_bytes.len();

    // Synthesise unique static identifiers so two derives in
    // one crate don't collide.
    let suffix_ident = quote::format_ident!("__BS_VIZ_SUFFIX_{}", ty_ident);
    let spec_ident = quote::format_ident!("__BS_VIZ_SPEC_{}", ty_ident);

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

    let body = match explicit_name {
        Some(name) => {
            // Verbatim: total = 13 + name.len() + suffix_len.
            quote! {
                #[allow(non_upper_case_globals)]
                const _: () = {
                    const __NAME: &::core::primitive::str = #name;
                    const __SUFFIX_LEN: ::core::primitive::usize = #suffix_len;
                    const __SUFFIX: [::core::primitive::u8; __SUFFIX_LEN] = *#suffix_lit;
                    const __TOTAL: ::core::primitive::usize =
                        13 + __NAME.len() + __SUFFIX_LEN;
                    #section_attrs
                    static #spec_ident: [::core::primitive::u8; __TOTAL] =
                        ::bs_viz_sdk::__internal::assemble_verbatim::<__TOTAL>(
                            __NAME, &__SUFFIX,
                        );
                    // Suppress unused-name lint when this static
                    // is the only reference to it.
                    let _ = &#spec_ident;
                };
            }
        }
        None => {
            // module_path!()-composed: total = 15 + module.len()
            // + local.len() + suffix_len.
            let _ = suffix_ident; // silence unused warning when not branched
            quote! {
                #[allow(non_upper_case_globals)]
                const _: () = {
                    const __MODULE: &::core::primitive::str = ::core::module_path!();
                    const __LOCAL: &::core::primitive::str = #local_ident_str;
                    const __SUFFIX_LEN: ::core::primitive::usize = #suffix_len;
                    const __SUFFIX: [::core::primitive::u8; __SUFFIX_LEN] = *#suffix_lit;
                    const __TOTAL: ::core::primitive::usize =
                        15 + __MODULE.len() + __LOCAL.len() + __SUFFIX_LEN;
                    #section_attrs
                    static #spec_ident: [::core::primitive::u8; __TOTAL] =
                        ::bs_viz_sdk::__internal::assemble_with_module_path::<__TOTAL>(
                            __MODULE, __LOCAL, &__SUFFIX,
                        );
                    let _ = &#spec_ident;
                };
            }
        }
    };

    Ok(body)
}

#[derive(Default)]
struct TypeAttrs {
    summary: Option<String>,
    /// Step 9 — explicit `name = "..."` override for the recorded
    /// type-name. When set, this string is used verbatim as the
    /// spec's `type_name` (and therefore the registry's lookup
    /// key) instead of the local stringified ident. The escape
    /// hatch for users who hit a suffix-match ambiguity bail —
    /// they can disambiguate by writing the full path
    /// themselves: `#[bs_viz(name = "my_crate::Person")]`. Once
    /// the macro learns to compose `module_path!()` at the user
    /// crate's compile time (a follow-up), this attribute
    /// becomes redundant for the common case but stays as the
    /// authoritative override for re-exports / module renames.
    name: Option<String>,
}

fn parse_type_attrs(attrs: &[syn::Attribute]) -> syn::Result<TypeAttrs> {
    let mut out = TypeAttrs::default();
    for attr in attrs {
        if !attr.path().is_ident("bs_viz") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("summary") {
                let v: syn::LitStr = meta.value()?.parse()?;
                out.summary = Some(v.value());
                Ok(())
            } else if meta.path.is_ident("name") {
                let v: syn::LitStr = meta.value()?.parse()?;
                out.name = Some(v.value());
                Ok(())
            } else {
                Err(meta.error(
                    "unknown #[bs_viz(...)] attribute on type \
                     (supports `summary = \"...\"` and \
                     `name = \"fully::qualified::Path\"`)",
                ))
            }
        })?;
    }
    Ok(out)
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
