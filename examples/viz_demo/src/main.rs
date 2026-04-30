// SPDX-License-Identifier: MIT
//! Phase 4 Tier-A end-to-end smoke debuggee. Two `#[derive(DebugView)]`
//! types so the integration test can verify both the count of
//! recovered specs *and* their attribute coverage.

use bs_viz_sdk::DebugView;

#[derive(DebugView)]
#[bs_viz(summary = "Person({name}, age {age})")]
pub struct Person {
    pub name: String,
    pub age: u32,
    #[bs_viz(skip)]
    pub _private_token: u64,
    #[bs_viz(rename = "kind")]
    pub category: u8,
    #[bs_viz(format = "hex")]
    pub flags: u32,
}

#[derive(DebugView)]
#[bs_viz(summary = "Box<{label}> = {n}")]
pub struct Counter {
    pub label: &'static str,
    pub n: u64,
}

/// Step 3 — generic types are supported. The macro emits one
/// spec per *definition*; the registry's lookup strips
/// `<…>` so `Wrap<i32>` resolves to this entry.
#[derive(DebugView)]
#[bs_viz(summary = "Wrap[{inner}]")]
pub struct Wrap<T> {
    pub inner: T,
}

fn main() {
    let p = Person {
        name: "Ada".to_string(),
        age: 36,
        _private_token: 0xDEAD_BEEF,
        category: 7,
        flags: 0x00FF_00FF,
    };
    let c = Counter { label: "ticks", n: 42 };
    let w_i32 = Wrap { inner: 17_i32 };
    let w_str = Wrap { inner: "fish" };
    // Reference everything so the linker keeps them.
    println!("{} / {} / {}", p.name, c.label, w_i32.inner); // BP_LINE = next line below
    std::hint::black_box((&p, &c, &w_i32, &w_str));
}
