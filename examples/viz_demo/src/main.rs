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

/// Step 4 — `iso8601` and `duration` formats applied at render
/// time. `created_at` is opted in to ISO-8601 interpretation
/// (Unix epoch seconds), `latency_ns` to a duration display.
#[derive(DebugView)]
#[bs_viz(summary = "Event(created={created_at}, took={latency_ns})")]
pub struct Event {
    #[bs_viz(format = "iso8601")]
    pub created_at: i64,
    #[bs_viz(format = "duration")]
    pub latency_ns: u64,
    pub seq: u32,
}

/// Step 3 — generic types are supported. The macro emits one
/// spec per *definition*; the registry's lookup strips
/// `<…>` so `Wrap<i32>` resolves to this entry.
#[derive(DebugView)]
#[bs_viz(summary = "Wrap[{inner}]")]
pub struct Wrap<T> {
    pub inner: T,
}

/// Step 5 — tuple structs are supported. Rust DWARF names the
/// fields `__0`, `__1`, etc.; the macro mirrors that
/// convention so summary placeholders + per-field attributes
/// resolve. Newtype-style `pub struct UserId(pub u64);` is the
/// canonical use case.
#[derive(DebugView)]
#[bs_viz(summary = "UserId#{__0}")]
pub struct UserId(#[bs_viz(format = "hex")] pub u64);

/// Step 5 — multi-field tuple struct.
#[derive(DebugView)]
#[bs_viz(summary = "Point({__0}, {__1})")]
pub struct Point(pub i32, pub i32);

/// Step 5 — unit struct. Empty field list is valid in the spec
/// format; the registry round-trips it via `decode_all`.
#[derive(DebugView)]
#[bs_viz(summary = "Sentinel")]
pub struct Sentinel;

/// Step 9 — `name = "..."` override. When two crates each
/// derive `DebugView` on a type with the same local name, the
/// suffix-match lookup is ambiguous and bails to `None`. The
/// override lets users record the fully-qualified name so the
/// exact-match path fires. The recorded name is what
/// `Debugger::view_spec_for` looks up.
#[derive(DebugView)]
#[bs_viz(name = "qualified::Marker", summary = "Marker#{__0}")]
pub struct Marker(pub u32);

/// Step 11 — `format = "utf8"` and `format = "hexdump"` applied
/// to byte-array fields. `body` is opted into a UTF-8 string
/// view; `raw` is opted into a hex dump.
#[derive(DebugView)]
#[bs_viz(summary = "Doc({size} bytes)")]
pub struct Doc {
    #[bs_viz(format = "utf8")]
    pub body: Vec<u8>,
    #[bs_viz(format = "hexdump")]
    pub raw: Vec<u8>,
    pub size: u32,
}

/// Step 6 — enum with type-level summary as fallback.
/// Step 8 — per-variant `summary` overrides the type-level one
/// when that variant is active, and `tag = "..."` annotates the
/// rendered enum with a state-tag chip.
#[derive(DebugView)]
#[bs_viz(summary = "Status[{__0}]")]
pub enum Status {
    #[bs_viz(summary = "✓ Connected (port {__0})", tag = "ok")]
    Connected(u32),
    #[bs_viz(summary = "○ Disconnected", tag = "warn")]
    Disconnected,
    #[bs_viz(summary = "✗ Error: {__0}", tag = "err")]
    Error(&'static str),
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
    let ev = Event {
        // 2024-01-15T12:34:56Z (a known ISO-8601 fixed point).
        created_at: 1705_322_096,
        // 5 ms in nanoseconds.
        latency_ns: 5_000_000,
        seq: 9,
    };
    let uid = UserId(0xCAFE_BABE);
    let pt = Point(10, 20);
    let sentinel = Sentinel;
    let status_ok = Status::Connected(443);
    let status_err = Status::Error("transport reset");
    let marker = Marker(99);
    let doc = Doc {
        body: b"hello world".to_vec(),
        raw: vec![0x00, 0xFF, 0x42, 0x53, 0x56, 0x31],
        size: 11,
    };
    // Reference everything so the linker keeps them.
    println!("{} / {} / {} / {}", p.name, c.label, w_i32.inner, ev.seq); // BP_LINE = next line below
    std::hint::black_box((
        &p, &c, &w_i32, &w_str, &ev, &uid, &pt, &sentinel, &status_ok, &status_err,
        &marker, &doc,
    ));
}
