// SPDX-License-Identifier: MIT
//! `syscall! { … }` DSL macro for the sub-phase 3B recorder.
//!
//! Parses a list of syscall declarations into a
//! `&[bs_syscall_spec::SyscallSpec]` const-initializer expression.
//! See `crates/bs-syscall-spec/src/lib.rs` for the data shape;
//! see `doc/plans/phase-5-time-travel.md` § 3B for the strategy.
//!
//! ## Grammar
//!
//! ```text
//! input        := entry (";" entry)* ";"?
//! entry        := IDENT "[" INT_LIT "]" "(" params ")" "->" ret
//! params       := /* empty */ | param ("," param)* ","?
//! param        := IDENT ":" kind
//! kind         := "fd"
//!               | "i32" | "u32" | "i64" | "u64" | "isize" | "usize"
//!               | "cstr"
//!               | "ptr"
//!               | "in_buf"  "(" "len" "=" IDENT ")"
//!               | "out_buf" "(" "len" "=" IDENT ")"
//! ret          := "isize" | "i32" | "u64" | "never"
//! ```
//!
//! `len = ret` (the literal identifier `ret`) is the sentinel for
//! "use the syscall's return value as the byte count" — see
//! `bs_syscall_spec::ParamKind::OutBuf` for the contract.
//!
//! ## Example
//!
//! ```ignore
//! use bs_syscall_macro::syscall;
//!
//! const TABLE: &[bs_syscall_spec::SyscallSpec] = syscall! {
//!     read[0](fd: fd, buf: out_buf(len = ret), count: usize) -> isize;
//!     write[1](fd: fd, buf: in_buf(len = count), count: usize) -> isize;
//!     open[2](pathname: cstr, flags: i32, mode: u32) -> i32;
//!     close[3](fd: fd) -> i32;
//! };
//! ```

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{Ident, LitInt, Result as SynResult, Token, bracketed, parenthesized, parse_macro_input};

/// `syscall! { … }` — see crate docs for the grammar.
#[proc_macro]
pub fn syscall(input: TokenStream) -> TokenStream {
    let table = parse_macro_input!(input as Table);
    table.expand().into()
}

// ---------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------

struct Table {
    entries: Vec<Entry>,
}

struct Entry {
    name: Ident,
    nr: LitInt,
    params: Vec<ParamEntry>,
    ret: RetKind,
}

struct ParamEntry {
    name: Ident,
    kind: KindExpr,
}

enum KindExpr {
    Fd,
    Scalar(ScalarKind),
    Cstr,
    Ptr,
    InBuf { len_param: String },
    OutBuf { len_param: String },
}

enum ScalarKind {
    U32,
    U64,
    I32,
    I64,
    USize,
    ISize,
}

enum RetKind {
    Isize,
    I32,
    U64,
    Never,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

impl Parse for Table {
    fn parse(input: ParseStream) -> SynResult<Self> {
        let entries: Punctuated<Entry, Token![;]> =
            input.parse_terminated(Entry::parse, Token![;])?;
        Ok(Table {
            entries: entries.into_iter().collect(),
        })
    }
}

impl Parse for Entry {
    fn parse(input: ParseStream) -> SynResult<Self> {
        let name: Ident = input.parse()?;
        let nr_buf;
        bracketed!(nr_buf in input);
        let nr: LitInt = nr_buf.parse()?;

        let param_buf;
        parenthesized!(param_buf in input);
        let params: Punctuated<ParamEntry, Token![,]> =
            param_buf.parse_terminated(ParamEntry::parse, Token![,])?;

        input.parse::<Token![->]>()?;
        let ret_ident: Ident = input.parse()?;
        let ret = parse_ret(&ret_ident)?;

        Ok(Entry {
            name,
            nr,
            params: params.into_iter().collect(),
            ret,
        })
    }
}

impl Parse for ParamEntry {
    fn parse(input: ParseStream) -> SynResult<Self> {
        let name: Ident = input.parse()?;
        input.parse::<Token![:]>()?;
        let kind = KindExpr::parse(input)?;
        Ok(ParamEntry { name, kind })
    }
}

impl KindExpr {
    fn parse(input: ParseStream) -> SynResult<Self> {
        let head: Ident = input.parse()?;
        match head.to_string().as_str() {
            "fd" => Ok(KindExpr::Fd),
            "u32" => Ok(KindExpr::Scalar(ScalarKind::U32)),
            "u64" => Ok(KindExpr::Scalar(ScalarKind::U64)),
            "i32" => Ok(KindExpr::Scalar(ScalarKind::I32)),
            "i64" => Ok(KindExpr::Scalar(ScalarKind::I64)),
            "usize" => Ok(KindExpr::Scalar(ScalarKind::USize)),
            "isize" => Ok(KindExpr::Scalar(ScalarKind::ISize)),
            "cstr" => Ok(KindExpr::Cstr),
            "ptr" => Ok(KindExpr::Ptr),
            "in_buf" | "out_buf" => {
                let buf_buf;
                parenthesized!(buf_buf in input);
                let len_kw: Ident = buf_buf.parse()?;
                if len_kw != "len" {
                    return Err(syn::Error::new(
                        len_kw.span(),
                        format!("expected `len`, found `{len_kw}`"),
                    ));
                }
                buf_buf.parse::<Token![=]>()?;
                let len_param: Ident = buf_buf.parse()?;
                let len_str = len_param.to_string();
                Ok(if head == "in_buf" {
                    KindExpr::InBuf { len_param: len_str }
                } else {
                    KindExpr::OutBuf { len_param: len_str }
                })
            }
            other => Err(syn::Error::new(
                head.span(),
                format!(
                    "unknown param kind `{other}`; expected one of: fd, u32, u64, \
                     i32, i64, usize, isize, cstr, ptr, in_buf(len = …), \
                     out_buf(len = …)",
                ),
            )),
        }
    }
}

fn parse_ret(ident: &Ident) -> SynResult<RetKind> {
    match ident.to_string().as_str() {
        "isize" => Ok(RetKind::Isize),
        "i32" => Ok(RetKind::I32),
        "u64" => Ok(RetKind::U64),
        "never" => Ok(RetKind::Never),
        other => Err(syn::Error::new(
            ident.span(),
            format!(
                "unknown return kind `{other}`; expected one of: isize, i32, u64, \
                 never",
            ),
        )),
    }
}

// ---------------------------------------------------------------------------
// Emit
// ---------------------------------------------------------------------------

impl Table {
    fn expand(&self) -> TokenStream2 {
        let entries = self.entries.iter().map(|e| e.expand());
        quote! {
            &[
                #( #entries ),*
            ] as &[::bs_syscall_spec::SyscallSpec]
        }
    }
}

impl Entry {
    fn expand(&self) -> TokenStream2 {
        let name_str = self.name.to_string();
        let nr = &self.nr;
        let params = self.params.iter().map(|p| p.expand());
        let ret = self.ret.expand();
        quote! {
            ::bs_syscall_spec::SyscallSpec {
                name: #name_str,
                nr: #nr,
                params: &[
                    #( #params ),*
                ],
                ret: #ret,
            }
        }
    }
}

impl ParamEntry {
    fn expand(&self) -> TokenStream2 {
        let name_str = self.name.to_string();
        let kind = self.kind.expand();
        quote! {
            ::bs_syscall_spec::Param {
                name: #name_str,
                kind: #kind,
            }
        }
    }
}

impl KindExpr {
    fn expand(&self) -> TokenStream2 {
        match self {
            KindExpr::Fd => quote!(::bs_syscall_spec::ParamKind::Fd),
            KindExpr::Cstr => quote!(::bs_syscall_spec::ParamKind::InCStr),
            KindExpr::Ptr => quote!(::bs_syscall_spec::ParamKind::OpaquePtr),
            KindExpr::Scalar(k) => {
                let v = k.expand();
                quote!(::bs_syscall_spec::ParamKind::Scalar(#v))
            }
            KindExpr::InBuf { len_param } => {
                quote!(::bs_syscall_spec::ParamKind::InBuf { len_param: #len_param })
            }
            KindExpr::OutBuf { len_param } => {
                quote!(::bs_syscall_spec::ParamKind::OutBuf { len_param: #len_param })
            }
        }
    }
}

impl ScalarKind {
    fn expand(&self) -> TokenStream2 {
        match self {
            ScalarKind::U32 => quote!(::bs_syscall_spec::ScalarKind::U32),
            ScalarKind::U64 => quote!(::bs_syscall_spec::ScalarKind::U64),
            ScalarKind::I32 => quote!(::bs_syscall_spec::ScalarKind::I32),
            ScalarKind::I64 => quote!(::bs_syscall_spec::ScalarKind::I64),
            ScalarKind::USize => quote!(::bs_syscall_spec::ScalarKind::USize),
            ScalarKind::ISize => quote!(::bs_syscall_spec::ScalarKind::ISize),
        }
    }
}

impl RetKind {
    fn expand(&self) -> TokenStream2 {
        match self {
            RetKind::Isize => quote!(::bs_syscall_spec::ReturnKind::Isize),
            RetKind::I32 => quote!(::bs_syscall_spec::ReturnKind::I32),
            RetKind::U64 => quote!(::bs_syscall_spec::ReturnKind::U64),
            RetKind::Never => quote!(::bs_syscall_spec::ReturnKind::Never),
        }
    }
}
