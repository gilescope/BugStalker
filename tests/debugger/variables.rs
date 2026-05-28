// SPDX-License-Identifier: MIT
use crate::VARS_APP;
use crate::common::TestHooks;
use crate::common::{TestInfo, rust_version};
use crate::{assert_no_proc, prepare_debugee_process};
use bugstalker::debugger::DebuggerBuilder;
use bugstalker::debugger::call::fmt::call_debug_fmt;
use bugstalker::debugger::variable::dqe::{Dqe, Literal, LiteralOrWildcard, PointerCast, Selector};
use bugstalker::debugger::variable::render::RenderValue;
use bugstalker::debugger::variable::execute::FileScopeFilter;
use bugstalker::debugger::variable::mutability::{self, Mutability};
use bugstalker::debugger::variable::value::specialization::LockState;
use bugstalker::debugger::variable::value::{Member, SpecializedValue, SupportedScalar, Value};
use bugstalker::version::Version;
use bugstalker::version_switch;
use serial_test::serial;
use std::collections::HashMap;

pub fn assert_scalar(value: &Value, exp_type: &str, exp_val: Option<SupportedScalar>) {
    let Value::Scalar(scalar) = value else {
        panic!("not a scalar");
    };
    assert_eq!(value.r#type().name_fmt(), exp_type);
    assert_eq!(scalar.value, exp_val);
}

fn assert_struct(val: &Value, exp_type: &str, for_each_member: impl Fn(usize, &Member)) {
    let Value::Struct(structure) = val else {
        panic!("not a struct");
    };
    // Phase 3A annotates `dyn Trait` fat-pointer structs with a
    // `[→ Concrete]` suffix on the type name when the vtable
    // resolves. The annotation is render-only metadata; for the
    // canonical type-name check we strip it before comparing so
    // these tests don't have to know whether vtable resolution
    // succeeded for the build under test.
    let actual = val.r#type().name_fmt().to_string();
    let actual_canonical = actual
        .split_once(" [→ ")
        .map(|(prefix, _)| prefix)
        .unwrap_or(&actual);
    assert_eq!(actual_canonical, exp_type);
    for (i, member) in structure.members.iter().enumerate() {
        for_each_member(i, member)
    }
}

fn assert_member(member: &Member, expected_field_name: &str, with_value: impl Fn(&Value)) {
    assert_eq!(member.field_name.as_deref(), Some(expected_field_name));
    with_value(&member.value);
}

fn assert_array(val: &Value, exp_type: &str, for_each_item: impl Fn(usize, &Value)) {
    let Value::Array(array) = val else {
        panic!("not an array");
    };
    assert_eq!(array.type_ident.name_fmt(), exp_type);
    for (i, item) in array.items.as_ref().unwrap_or(&vec![]).iter().enumerate() {
        for_each_item(i, &item.value)
    }
}

fn assert_c_enum(val: &Value, exp_type: &str, exp_value: Option<String>) {
    let Value::CEnum(c_enum) = val else {
        panic!("not a c_enum");
    };
    assert_eq!(val.r#type().name_fmt(), exp_type);
    assert_eq!(c_enum.value, exp_value);
}

fn assert_rust_enum(val: &Value, exp_type: &str, with_value: impl FnOnce(&Value)) {
    let Value::RustEnum(rust_enum) = val else {
        panic!("not a c_enum");
    };
    assert_eq!(val.r#type().name_fmt(), exp_type);
    with_value(&rust_enum.value.as_ref().unwrap().value);
}

fn assert_pointer(val: &Value, exp_type: &str) {
    let Value::Pointer(ptr) = val else {
        panic!("not a pointer");
    };
    assert_eq!(ptr.type_ident.name_fmt(), exp_type);
}

fn assert_vec(val: &Value, exp_type: &str, exp_cap: usize, with_buf: impl FnOnce(&Value)) {
    let Value::Specialized {
        value: Some(SpecializedValue::Vector(vector)),
        ..
    } = val
    else {
        panic!("not a vector");
    };
    assert_eq!(val.r#type().name_fmt(), exp_type);
    let Value::Scalar(capacity) = &vector.structure.members[1].value else {
        panic!("no capacity");
    };
    assert_eq!(capacity.value, Some(SupportedScalar::Usize(exp_cap)));
    with_buf(&vector.structure.members[0].value);
}

fn assert_string(val: &Value, exp_value: &str) {
    let Value::Specialized {
        value: Some(SpecializedValue::String(string)),
        ..
    } = val
    else {
        panic!("not a string");
    };
    assert_eq!(string.value, exp_value);
}

fn assert_str(val: &Value, exp_value: &str) {
    let Value::Specialized {
        value: Some(SpecializedValue::Str(str)),
        ..
    } = val
    else {
        panic!("not a &str");
    };
    assert_eq!(str.value, exp_value);
}

fn assert_init_tls(val: &Value, exp_type: &str, with_inner: impl FnOnce(&Value)) {
    let Value::Specialized {
        value: Some(SpecializedValue::Tls(tls)),
        ..
    } = val
    else {
        panic!("not a tls");
    };
    assert_eq!(tls.inner_type.name_fmt(), exp_type);
    with_inner(tls.inner_value.as_ref().unwrap());
}

fn assert_uninit_tls(val: &Value, exp_type: &str) {
    let Value::Specialized {
        value: Some(SpecializedValue::Tls(tls)),
        ..
    } = val
    else {
        panic!("not a tls");
    };
    assert_eq!(tls.inner_type.name_fmt(), exp_type);
    assert!(tls.inner_value.is_none());
}

fn assert_hashmap(val: &Value, exp_type: &str, with_kv_items: impl FnOnce(&Vec<(Value, Value)>)) {
    let Value::Specialized {
        value: Some(SpecializedValue::HashMap(map)),
        ..
    } = val
    else {
        panic!("not a hashmap");
    };
    assert_eq!(val.r#type().name_fmt(), exp_type);
    let mut items = map.kv_items.clone();
    items.sort_by(|v1, v2| {
        let k1_render = format!("{:?}", v1.0.value_layout());
        let k2_render = format!("{:?}", v2.0.value_layout());
        k1_render.cmp(&k2_render)
    });
    with_kv_items(&items);
}

fn assert_hashset(val: &Value, exp_type: &str, with_items: impl FnOnce(&Vec<Value>)) {
    let Value::Specialized {
        value: Some(SpecializedValue::HashSet(set)),
        ..
    } = val
    else {
        panic!("not a hashset");
    };
    assert_eq!(set.type_ident.name_fmt(), exp_type);
    let mut items = set.items.clone();
    items.sort_by(|v1, v2| {
        let k1_render = format!("{:?}", v1.value_layout());
        let k2_render = format!("{:?}", v2.value_layout());
        k1_render.cmp(&k2_render)
    });
    with_items(&items);
}

fn assert_btree_map(val: &Value, exp_type: &str, with_kv_items: impl FnOnce(&Vec<(Value, Value)>)) {
    let Value::Specialized {
        value: Some(SpecializedValue::BTreeMap(map)),
        ..
    } = val
    else {
        panic!("not a BTreeMap");
    };
    assert_eq!(map.type_ident.name_fmt(), exp_type);
    with_kv_items(&map.kv_items);
}

fn assert_btree_set(val: &Value, exp_type: &str, with_items: impl FnOnce(&Vec<Value>)) {
    let Value::Specialized {
        value: Some(SpecializedValue::BTreeSet(set)),
        ..
    } = val
    else {
        panic!("not a BTreeSet");
    };
    assert_eq!(set.type_ident.name_fmt(), exp_type);
    with_items(&set.items);
}

fn assert_vec_deque(val: &Value, exp_type: &str, exp_cap: usize, with_buf: impl FnOnce(&Value)) {
    let Value::Specialized {
        value: Some(SpecializedValue::VecDeque(vector)),
        ..
    } = val
    else {
        panic!("not a VecDeque");
    };
    assert_eq!(vector.structure.type_ident.name_fmt(), exp_type);
    let Value::Scalar(capacity) = &vector.structure.members[1].value else {
        panic!("no capacity");
    };
    assert_eq!(capacity.value, Some(SupportedScalar::Usize(exp_cap)));
    with_buf(&vector.structure.members[0].value);
}

/// Phase 1 S7 helper: assert a `Pin<P>` peeled to its pinnee, with
/// the wrapper type-identity preserved.
fn assert_pin(val: &Value, exp_outer_type: &str, with_inner: impl FnOnce(&Value)) {
    let Value::Specialized {
        value: Some(SpecializedValue::Pin(inner)),
        ..
    } = val
    else {
        panic!("not a Pin spec value: {:?}", val.r#type().name_fmt());
    };
    assert_eq!(val.r#type().name_fmt(), exp_outer_type);
    with_inner(inner.as_ref());
}

/// Phase 1 S11 helper: assert a `NonNull<T>` rendered as the bare
/// inner pointer with the wrapper type-identity preserved.
fn assert_nonnull_pointer(val: &Value, exp_outer_type: &str, exp_inner_type: &str) {
    let Value::Specialized {
        value: Some(SpecializedValue::NonNull(ptr)),
        ..
    } = val
    else {
        panic!("not a NonNull spec value: {:?}", val.r#type().name_fmt());
    };
    assert_eq!(val.r#type().name_fmt(), exp_outer_type);
    assert_pointer(&Value::Pointer(ptr.clone()), exp_inner_type);
}

/// Phase 1 S3 helper: assert an `Atomic*` rendered as a bare scalar
/// with the wrapper type-identity preserved.
fn assert_atomic_scalar(
    val: &Value,
    exp_outer_type: &str,
    exp_inner_type: &str,
    exp_val: SupportedScalar,
) {
    let Value::Specialized {
        value: Some(SpecializedValue::Atomic(inner)),
        ..
    } = val
    else {
        panic!("not an Atomic spec value");
    };
    assert_eq!(val.r#type().name_fmt(), exp_outer_type);
    assert_scalar(inner.as_ref(), exp_inner_type, Some(exp_val));
}

/// Phase 1 S3 helper: assert an `AtomicPtr<T>` rendered as a bare pointer.
fn assert_atomic_pointer(val: &Value, exp_outer_type: &str, exp_inner_type: &str) {
    let Value::Specialized {
        value: Some(SpecializedValue::Atomic(inner)),
        ..
    } = val
    else {
        panic!("not an Atomic spec value");
    };
    assert_eq!(val.r#type().name_fmt(), exp_outer_type);
    assert_pointer(inner.as_ref(), exp_inner_type);
}

fn assert_cell(val: &Value, exp_type: &str, with_value: impl FnOnce(&Value)) {
    let Value::Specialized {
        value: Some(SpecializedValue::Cell(value)),
        ..
    } = val
    else {
        panic!("not a Cell");
    };
    assert_eq!(val.r#type().name_fmt(), exp_type);
    with_value(value.as_ref());
}

fn assert_refcell(val: &Value, exp_type: &str, exp_borrow: isize, with_inner: impl FnOnce(&Value)) {
    let Value::Specialized {
        value: Some(SpecializedValue::RefCell(value)),
        ..
    } = val
    else {
        panic!("not a Cell");
    };
    assert_eq!(val.r#type().name_fmt(), exp_type);
    let Value::Struct(as_struct) = value.as_ref() else {
        panic!("not a struct")
    };

    let Value::Scalar(borrow) = &as_struct.members[0].value else {
        panic!("no borrow flag");
    };
    assert_eq!(
        borrow.value.as_ref().unwrap(),
        &SupportedScalar::Isize(exp_borrow)
    );
    with_inner(&as_struct.members[1].value);
}

fn assert_rc(val: &Value, exp_type: &str) {
    let Value::Specialized {
        value: Some(SpecializedValue::Rc(_)),
        ..
    } = val
    else {
        panic!("not an rc");
    };
    assert_eq!(val.r#type().name_fmt(), exp_type);
}

fn assert_arc(val: &Value, exp_type: &str) {
    let Value::Specialized {
        value: Some(SpecializedValue::Arc(_)),
        ..
    } = val
    else {
        panic!("not an arc");
    };
    assert_eq!(val.r#type().name_fmt(), exp_type);
}

/// Phase 1 S15 helper: assert a Weak with expected strong/weak counts.
fn assert_weak(val: &Value, exp_type: &str, exp_strong: u64, exp_weak: u64) {
    let Value::Specialized {
        value: Some(SpecializedValue::Weak { strong, weak, .. }),
        ..
    } = val
    else {
        panic!("not a Weak spec value: type={:?}", val.r#type().name_fmt());
    };
    assert_eq!(val.r#type().name_fmt(), exp_type);
    assert_eq!(*strong, exp_strong, "strong count mismatch for {exp_type}");
    assert_eq!(*weak, exp_weak, "weak count mismatch for {exp_type}");
}

fn assert_uuid(val: &Value, exp_type: &str) {
    let Value::Specialized {
        value: Some(SpecializedValue::Uuid(_)),
        ..
    } = val
    else {
        panic!("not an uuid");
    };
    assert_eq!(val.r#type().name_fmt(), exp_type);
}

fn assert_system_time(val: &Value, exp_value: (i64, u32)) {
    let Value::Specialized {
        value: Some(SpecializedValue::SystemTime(value)),
        ..
    } = val
    else {
        panic!("not a SystemTime");
    };
    assert_eq!(*value, exp_value);
}

fn assert_instant(val: &Value) {
    let Value::Specialized {
        value: Some(SpecializedValue::Instant(_)),
        ..
    } = val
    else {
        panic!("not an Instant");
    };
}

macro_rules! read_locals {
    ($debugger: expr => $($var: ident),*) => {
        let vars = $debugger.read_local_variables().unwrap();
        let &[$($var),*] = &vars.as_slice() else {
            panic!("Invalid variables count")
        };
    };
}

macro_rules! read_var_dqe {
    ($debugger: expr, $dqe: expr => $($var: ident),*) => {
        let vars = $debugger.read_variable($dqe).unwrap();
        let &[$($var),*] = &vars.as_slice() else {
            panic!("Invalid variables count")
        };
    };
}

macro_rules! read_arg_dqe {
    ($debugger: expr, $dqe: expr => $($var: ident),*) => {
        let args = $debugger.read_argument($dqe).unwrap();
        let &[$($var),*] = &args.as_slice() else {
            panic!("Invalid variables count")
        };
    };
}

macro_rules! read_var_dqe_type_order {
    ($debugger: expr, $dqe: expr => $($var: ident),*) => {
        let mut vars = $debugger.read_variable($dqe).unwrap();
        vars.sort_by(|v1, v2| v1.value().r#type().cmp(v2.value().r#type()));
        let &[$($var),*] = &vars.as_slice() else {
            panic!("Invalid variables count")
        };
    };
}

macro_rules! assert_idents {
    ($($var: ident => $name: literal),*) => {
        $(
            assert_eq!($var.identity().to_string(), $name);
        )*
    };
}

#[test]
#[serial]
fn test_read_scalar_variables() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 30).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(30));

    read_locals!(debugger => int8, int16, int32, int64, int128, isize, uint8, uint16, uint32, uint64, uint128, usize, f32, f64, b_true, b_false, ch_ascii, c_n_ascii);
    assert_idents!(
        int8 => "int8", int16 => "int16", int32 => "int32", int64 => "int64", int128 => "int128",
        isize => "isize", uint8 => "uint8", uint16 => "uint16", uint32 => "uint32", uint64 => "uint64",
        uint128 => "uint128", usize => "usize", f32 => "f32", f64 => "f64", b_true => "boolean_true",
        b_false => "boolean_false", ch_ascii => "char_ascii", c_n_ascii => "char_non_ascii"
    );

    assert_scalar(int8.value(), "i8", Some(SupportedScalar::I8(1)));
    assert_scalar(int16.value(), "i16", Some(SupportedScalar::I16(-1)));
    assert_scalar(int32.value(), "i32", Some(SupportedScalar::I32(2)));
    assert_scalar(int64.value(), "i64", Some(SupportedScalar::I64(-2)));
    assert_scalar(int128.value(), "i128", Some(SupportedScalar::I128(3)));
    assert_scalar(isize.value(), "isize", Some(SupportedScalar::Isize(-3)));
    assert_scalar(uint8.value(), "u8", Some(SupportedScalar::U8(1)));
    assert_scalar(uint16.value(), "u16", Some(SupportedScalar::U16(2)));
    assert_scalar(uint32.value(), "u32", Some(SupportedScalar::U32(3)));
    assert_scalar(uint64.value(), "u64", Some(SupportedScalar::U64(4)));
    assert_scalar(uint128.value(), "u128", Some(SupportedScalar::U128(5)));
    assert_scalar(usize.value(), "usize", Some(SupportedScalar::Usize(6)));
    assert_scalar(f32.value(), "f32", Some(SupportedScalar::F32(1.1)));
    assert_scalar(f64.value(), "f64", Some(SupportedScalar::F64(1.2)));
    assert_scalar(b_true.value(), "bool", Some(SupportedScalar::Bool(true)));
    assert_scalar(b_false.value(), "bool", Some(SupportedScalar::Bool(false)));
    assert_scalar(ch_ascii.value(), "char", Some(SupportedScalar::Char('a')));
    assert_scalar(c_n_ascii.value(), "char", Some(SupportedScalar::Char('😊')));

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_read_scalar_variables_at_place() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 11).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(11));

    let vars = debugger.read_local_variables().unwrap();
    // WAITFORFIX: https://github.com/rust-lang/rust/issues/113819
    // expected: assert_eq!(vars.len(), 4);
    // through this bug there is uninitialized variable here
    assert_eq!(vars.len(), 5);

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_read_struct() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 53).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(53));

    read_locals!(debugger => tuple_0, tuple_1, tuple_2, foo, foo2);
    assert_idents!(tuple_0 => "tuple_0", tuple_1 => "tuple_1", tuple_2 => "tuple_2", foo => "foo", foo2 => "foo2");

    assert_scalar(tuple_0.value(), "()", Some(SupportedScalar::Empty()));
    assert_struct(tuple_1.value(), "(f64, f64)", |i, member| match i {
        0 => assert_member(member, "__0", |val| {
            assert_scalar(val, "f64", Some(SupportedScalar::F64(0f64)))
        }),
        1 => assert_member(member, "__1", |val| {
            assert_scalar(val, "f64", Some(SupportedScalar::F64(1.1f64)))
        }),
        _ => panic!("2 members expected"),
    });
    assert_struct(
        tuple_2.value(),
        "(u64, i64, char, bool)",
        |i, member| match i {
            0 => assert_member(member, "__0", |val| {
                assert_scalar(val, "u64", Some(SupportedScalar::U64(1)))
            }),
            1 => assert_member(member, "__1", |val| {
                assert_scalar(val, "i64", Some(SupportedScalar::I64(-1)))
            }),
            2 => assert_member(member, "__2", |val| {
                assert_scalar(val, "char", Some(SupportedScalar::Char('a')))
            }),
            3 => assert_member(member, "__3", |val| {
                assert_scalar(val, "bool", Some(SupportedScalar::Bool(false)))
            }),
            _ => panic!("4 members expected"),
        },
    );
    assert_struct(foo.value(), "Foo", |i, member| match i {
        0 => assert_member(member, "bar", |val| {
            assert_scalar(val, "i32", Some(SupportedScalar::I32(100)))
        }),
        1 => assert_member(member, "baz", |val| {
            assert_scalar(val, "char", Some(SupportedScalar::Char('9')))
        }),
        _ => panic!("2 members expected"),
    });
    assert_struct(foo2.value(), "Foo2", |i, member| match i {
        0 => assert_member(member, "foo", |val| {
            assert_struct(val, "Foo", |i, member| match i {
                0 => assert_member(member, "bar", |val| {
                    assert_scalar(val, "i32", Some(SupportedScalar::I32(100)))
                }),
                1 => assert_member(member, "baz", |val| {
                    assert_scalar(val, "char", Some(SupportedScalar::Char('9')))
                }),
                _ => panic!("2 members expected"),
            })
        }),
        1 => assert_member(member, "additional", |val| {
            assert_scalar(val, "bool", Some(SupportedScalar::Bool(true)))
        }),
        _ => panic!("2 members expected"),
    });

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_read_array() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 61).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(61));

    read_locals!(debugger => arr_1, arr_2);
    assert_idents!(arr_1 => "arr_1", arr_2 => "arr_2");

    assert_array(arr_1.value(), "[i32]", |i, item| match i {
        0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(1))),
        1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(-1))),
        2 => assert_scalar(item, "i32", Some(SupportedScalar::I32(2))),
        3 => assert_scalar(item, "i32", Some(SupportedScalar::I32(-2))),
        4 => assert_scalar(item, "i32", Some(SupportedScalar::I32(3))),
        _ => panic!("5 items expected"),
    });
    assert_array(arr_2.value(), "[[i32]]", |i, item| match i {
        0 => assert_array(item, "[i32]", |i, item| match i {
            0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(1))),
            1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(-1))),
            2 => assert_scalar(item, "i32", Some(SupportedScalar::I32(2))),
            3 => assert_scalar(item, "i32", Some(SupportedScalar::I32(-2))),
            4 => assert_scalar(item, "i32", Some(SupportedScalar::I32(3))),
            _ => panic!("5 items expected"),
        }),
        1 => assert_array(item, "[i32]", |i, item| match i {
            0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(0))),
            1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(1))),
            2 => assert_scalar(item, "i32", Some(SupportedScalar::I32(2))),
            3 => assert_scalar(item, "i32", Some(SupportedScalar::I32(3))),
            4 => assert_scalar(item, "i32", Some(SupportedScalar::I32(4))),
            _ => panic!("5 items expected"),
        }),
        2 => assert_array(item, "[i32]", |i, item| match i {
            0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(0))),
            1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(-1))),
            2 => assert_scalar(item, "i32", Some(SupportedScalar::I32(-2))),
            3 => assert_scalar(item, "i32", Some(SupportedScalar::I32(-3))),
            4 => assert_scalar(item, "i32", Some(SupportedScalar::I32(-4))),
            _ => panic!("5 items expected"),
        }),
        _ => panic!("3 items expected"),
    });

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_read_enum() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 93).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(93));

    read_locals!(debugger => enum_1, enum_2, enum_3, enum_4, enum_5, enum_6, enum_7);
    assert_idents!(
        enum_1 => "enum_1", enum_2 => "enum_2", enum_3 => "enum_3", enum_4 => "enum_4",
        enum_5 => "enum_5", enum_6 => "enum_6", enum_7 => "enum_7"
    );

    assert_c_enum(enum_1.value(), "EnumA", Some("B".to_string()));
    assert_rust_enum(enum_2.value(), "EnumC", |enum_val| {
        assert_struct(enum_val, "C", |_, member| {
            assert_member(member, "__0", |val| {
                assert_scalar(val, "char", Some(SupportedScalar::Char('b')))
            })
        });
    });
    assert_rust_enum(enum_3.value(), "EnumC", |enum_val| {
        assert_struct(enum_val, "D", |i, member| {
            match i {
                0 => assert_member(member, "__0", |val| {
                    assert_scalar(val, "f64", Some(SupportedScalar::F64(1.1)))
                }),
                1 => assert_member(member, "__1", |val| {
                    assert_scalar(val, "f32", Some(SupportedScalar::F32(1.2)))
                }),
                _ => panic!("2 members expected"),
            };
        });
    });
    assert_rust_enum(enum_4.value(), "EnumC", |enum_val| {
        assert_struct(enum_val, "E", |_, _| {
            panic!("expected empty struct");
        });
    });
    assert_rust_enum(enum_5.value(), "EnumF", |enum_val| {
        assert_struct(enum_val, "F", |i, member| {
            match i {
                0 => assert_member(member, "__0", |val| {
                    assert_rust_enum(val, "EnumC", |enum_val| {
                        assert_struct(enum_val, "C", |_, member| {
                            assert_member(member, "__0", |val| {
                                assert_scalar(val, "char", Some(SupportedScalar::Char('f')))
                            })
                        });
                    })
                }),
                _ => panic!("1 members expected"),
            };
        });
    });
    assert_rust_enum(enum_6.value(), "EnumF", |enum_val| {
        assert_struct(enum_val, "G", |i, member| {
            match i {
                0 => assert_member(member, "__0", |val| {
                    assert_struct(val, "Foo", |i, member| match i {
                        0 => assert_member(member, "a", |val| {
                            assert_scalar(val, "i32", Some(SupportedScalar::I32(1)))
                        }),
                        1 => assert_member(member, "b", |val| {
                            assert_scalar(val, "char", Some(SupportedScalar::Char('1')))
                        }),
                        _ => panic!("2 members expected"),
                    })
                }),
                _ => panic!("1 members expected"),
            };
        });
    });
    assert_rust_enum(enum_7.value(), "EnumF", |enum_val| {
        assert_struct(enum_val, "J", |i, member| {
            match i {
                0 => assert_member(member, "__0", |val| {
                    assert_c_enum(val, "EnumA", Some("A".to_string()))
                }),
                _ => panic!("1 members expected"),
            };
        });
    });

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_read_pointers() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 119).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(119));

    read_locals!(debugger => a, ref_a, ptr_a, ptr_ptr_a, b, mut_ref_b, c, mut_ptr_c, box_d, f, ref_f);
    assert_idents!(
        a => "a", ref_a => "ref_a", ptr_a => "ptr_a", ptr_ptr_a => "ptr_ptr_a", b => "b",
        mut_ref_b => "mut_ref_b",c => "c", mut_ptr_c => "mut_ptr_c", box_d => "box_d", f => "f", ref_f => "ref_f"
    );

    assert_scalar(a.value(), "i32", Some(SupportedScalar::I32(2)));
    assert_pointer(ref_a.value(), "&i32");
    let deref = ref_a.clone().modify_value(|pcx, val| val.deref(pcx));
    assert_scalar(deref.unwrap().value(), "i32", Some(SupportedScalar::I32(2)));
    assert_pointer(ptr_a.value(), "*const i32");
    let deref = ptr_a.clone().modify_value(|pcx, val| val.deref(pcx));
    assert_scalar(deref.unwrap().value(), "i32", Some(SupportedScalar::I32(2)));
    assert_pointer(ptr_ptr_a.value(), "*const *const i32");
    let deref = ptr_ptr_a.clone().modify_value(|pcx, val| val.deref(pcx));
    assert_pointer(deref.unwrap().value(), "*const i32");
    let deref = ptr_ptr_a
        .clone()
        .modify_value(|pcx, v| v.deref(pcx).and_then(|v| v.deref(pcx)));
    assert_scalar(deref.unwrap().value(), "i32", Some(SupportedScalar::I32(2)));
    assert_scalar(b.value(), "i32", Some(SupportedScalar::I32(2)));
    assert_pointer(mut_ref_b.value(), "&mut i32");
    let deref = mut_ref_b.clone().modify_value(|pcx, v| v.deref(pcx));
    assert_scalar(deref.unwrap().value(), "i32", Some(SupportedScalar::I32(2)));
    assert_scalar(c.value(), "i32", Some(SupportedScalar::I32(2)));
    assert_pointer(mut_ptr_c.value(), "*mut i32");
    let deref = mut_ptr_c.clone().modify_value(|pcx, v| v.deref(pcx));
    assert_scalar(deref.unwrap().value(), "i32", Some(SupportedScalar::I32(2)));
    assert_pointer(
        box_d.value(),
        "alloc::boxed::Box<i32, alloc::alloc::Global>",
    );
    let deref = box_d.clone().modify_value(|pcx, v| v.deref(pcx));
    assert_scalar(deref.unwrap().value(), "i32", Some(SupportedScalar::I32(2)));
    // Phase 1 S9 — `Box<T>` smart-deref: the renderer surfaces the
    // pointee inline via `Wrapped(inner)` rather than the bare
    // address. Confirm the hook fired (a raw `*const i32` pointer
    // would render `Referential`).
    use bugstalker::debugger::variable::render::ValueLayout;
    let box_layout = box_d.value().value_layout().expect("box_d layout missing");
    match box_layout {
        ValueLayout::Wrapped(inner) => {
            assert_scalar(inner, "i32", Some(SupportedScalar::I32(2)));
        }
        other => panic!("expected Wrapped(inner) for Box<T> smart-deref, got {other:?}"),
    }
    // Raw `*const i32` should still be Referential.
    let ptr_a_layout = ptr_a.value().value_layout().expect("ptr_a layout missing");
    assert!(
        matches!(ptr_a_layout, ValueLayout::Referential(_)),
        "raw *const i32 should render as Referential, got {ptr_a_layout:?}"
    );
    assert_struct(f.value(), "Foo", |i, member| match i {
        0 => assert_member(member, "bar", |val| {
            assert_scalar(val, "i32", Some(SupportedScalar::I32(1)))
        }),
        1 => assert_member(member, "baz", |val| {
            assert_array(val, "[i32]", |i, item| match i {
                0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(1))),
                1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(2))),
                _ => panic!("2 items expected"),
            })
        }),
        2 => {
            assert_member(member, "foo", |val| assert_pointer(val, "&i32"));
            let foo_val = member.value.clone();
            let deref = f.clone().modify_value(|pcx, _| foo_val.deref(pcx));
            assert_scalar(deref.unwrap().value(), "i32", Some(SupportedScalar::I32(2)));
        }
        _ => panic!("3 members expected"),
    });
    assert_pointer(ref_f.value(), "&vars::references::Foo");
    let deref = ref_f.clone().modify_value(|pcx, v| v.deref(pcx));
    assert_struct(deref.unwrap().value(), "Foo", |i, member| match i {
        0 => assert_member(member, "bar", |val| {
            assert_scalar(val, "i32", Some(SupportedScalar::I32(1)))
        }),
        1 => assert_member(member, "baz", |val| {
            assert_array(val, "[i32]", |i, item| match i {
                0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(1))),
                1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(2))),
                _ => panic!("2 items expected"),
            })
        }),
        2 => {
            assert_member(member, "foo", |val| assert_pointer(val, "&i32"));
            let foo_val = member.value.clone();
            let deref = ref_f.clone().modify_value(|pcx, _| foo_val.deref(pcx));
            assert_scalar(deref.unwrap().value(), "i32", Some(SupportedScalar::I32(2)));
        }
        _ => panic!("3 members expected"),
    });

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_read_type_alias() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 126).unwrap();
    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(126));

    read_locals!(debugger => a_alias);
    assert_idents!(a_alias => "a_alias");
    assert_scalar(a_alias.value(), "i32", Some(SupportedScalar::I32(1)));

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_type_parameters() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 135).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(135));

    read_locals!(debugger => a);
    assert_idents!(a => "a");
    assert_struct(a.value(), "Foo<i32>", |i, member| match i {
        0 => assert_member(member, "bar", |val| {
            assert_scalar(val, "i32", Some(SupportedScalar::I32(1)))
        }),
        _ => panic!("1 members expected"),
    });

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_read_vec_and_slice() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 151).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(151));

    read_locals!(debugger => vec1, vec2, vec3, slice1, slice2);
    assert_idents!(vec1 => "vec1", vec2 => "vec2", vec3 => "vec3", slice1 => "slice1", slice2 => "slice2");

    assert_vec(vec1.value(), "Vec<i32, alloc::alloc::Global>", 3, |buf| {
        assert_array(buf, "[i32]", |i, item| match i {
            0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(1))),
            1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(2))),
            2 => assert_scalar(item, "i32", Some(SupportedScalar::I32(3))),
            _ => panic!("3 items expected"),
        })
    });

    assert_vec(
        vec2.value(),
        "Vec<vars::vec_and_slice_types::Foo, alloc::alloc::Global>",
        2,
        |buf| {
            assert_array(buf, "[Foo]", |i, item| match i {
                0 => assert_struct(item, "Foo", |i, member| match i {
                    0 => assert_member(member, "foo", |val| {
                        assert_scalar(val, "i32", Some(SupportedScalar::I32(1)))
                    }),
                    _ => panic!("1 members expected"),
                }),
                1 => assert_struct(item, "Foo", |i, member| match i {
                    0 => assert_member(member, "foo", |val| {
                        assert_scalar(val, "i32", Some(SupportedScalar::I32(2)))
                    }),
                    _ => panic!("1 members expected"),
                }),
                _ => panic!("2 items expected"),
            })
        },
    );

    assert_vec(
        vec3.value(),
        "Vec<alloc::vec::Vec<i32, alloc::alloc::Global>, alloc::alloc::Global>",
        2,
        |buf| {
            assert_array(buf, "[Vec<i32, alloc::alloc::Global>]", |i, item| match i {
                0 => assert_vec(item, "Vec<i32, alloc::alloc::Global>", 3, |buf| {
                    assert_array(buf, "[i32]", |i, item| match i {
                        0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(1))),
                        1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(2))),
                        2 => assert_scalar(item, "i32", Some(SupportedScalar::I32(3))),
                        _ => panic!("3 items expected"),
                    })
                }),
                1 => assert_vec(item, "Vec<i32, alloc::alloc::Global>", 3, |buf| {
                    assert_array(buf, "[i32]", |i, item| match i {
                        0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(1))),
                        1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(2))),
                        2 => assert_scalar(item, "i32", Some(SupportedScalar::I32(3))),
                        _ => panic!("3 items expected"),
                    })
                }),
                _ => panic!("2 items expected"),
            })
        },
    );

    assert_pointer(slice1.value(), "&[i32; 3]");
    let deref = slice1.clone().modify_value(|pcx, val| val.deref(pcx));
    assert_array(deref.unwrap().value(), "[i32]", |i, item| match i {
        0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(1))),
        1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(2))),
        2 => assert_scalar(item, "i32", Some(SupportedScalar::I32(3))),
        _ => panic!("3 items expected"),
    });

    assert_pointer(slice2.value(), "&[&[i32; 3]; 2]");
    let deref = slice2.clone().modify_value(|pcx, val| val.deref(pcx));
    assert_array(deref.unwrap().value(), "[&[i32; 3]]", |i, item| match i {
        0 => {
            assert_pointer(item, "&[i32; 3]");
            let item_val = item.clone();
            let deref = slice2.clone().modify_value(|pcx, _| item_val.deref(pcx));
            assert_array(deref.unwrap().value(), "[i32]", |i, item| match i {
                0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(1))),
                1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(2))),
                2 => assert_scalar(item, "i32", Some(SupportedScalar::I32(3))),
                _ => panic!("3 items expected"),
            });
        }
        1 => {
            assert_pointer(item, "&[i32; 3]");
            let item_val = item.clone();
            let deref = slice2.clone().modify_value(|pcx, _| item_val.deref(pcx));
            assert_array(deref.unwrap().value(), "[i32]", |i, item| match i {
                0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(1))),
                1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(2))),
                2 => assert_scalar(item, "i32", Some(SupportedScalar::I32(3))),
                _ => panic!("3 items expected"),
            });
        }
        _ => panic!("2 items expected"),
    });

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_read_strings() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 159).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(159));

    read_locals!(debugger => s1, s2, s3);
    assert_idents!(s1 => "s1", s2 => "s2", s3 => "s3");

    assert_string(s1.value(), "hello world");
    assert_str(s2.value(), "hello world");
    assert_str(s3.value(), "hello world");

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_read_static_variables() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 168).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(168));

    read_var_dqe!(debugger, Dqe::Variable(Selector::by_name("GLOB_1", false)) => glob_1);
    assert_idents!(glob_1 => "vars::GLOB_1");
    assert_str(glob_1.value(), "glob_1");

    read_var_dqe!(debugger, Dqe::Variable(Selector::by_name("GLOB_2", false)) => glob_2);
    assert_idents!(glob_2 => "vars::GLOB_2");
    assert_scalar(glob_2.value(), "i32", Some(SupportedScalar::I32(2)));

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

/// Variables-view §5.4 — bulk enumeration of file-scope statics
/// via the new `Debugger::read_static_variables` API powers the
/// DAP `Statics` scope. The user-crate filter must include the
/// fixture's `GLOB_1`/`GLOB_2`/`GLOB_3` (declared in the `vars`
/// crate) and exclude TLS internals which belong in the
/// `Thread-locals` scope.
#[test]
#[serial]
fn test_bulk_enumerate_statics() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 168).unwrap();
    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(168));

    let statics = debugger
        .read_static_variables(FileScopeFilter::CurrentCrate)
        .unwrap();
    let names: Vec<String> = statics.iter().map(|r| r.identity().to_string()).collect();

    // Sanity: the three fixture-declared file-scope statics in
    // the user (`vars`) crate must be enumerated.
    for needle in ["GLOB_1", "GLOB_2", "GLOB_3"] {
        assert!(
            names.iter().any(|n| n.contains(needle)),
            "expected {needle} in current-crate statics; got: {names:?}"
        );
    }
    // And TLS internals must NOT appear here — they're a separate
    // scope. Detection is by the rustc-lowered TLS name.
    for forbidden in ["__KEY", "__RUST_STD_INTERNAL_VAL"] {
        assert!(
            names.iter().all(|n| !n.contains(forbidden)),
            "{forbidden} leaked into statics: {names:?}"
        );
    }
    // Current-crate filter must keep std out — pick one common
    // std static that's always linked in a binary that uses stdio.
    assert!(
        names.iter().all(|n| !n.starts_with("std::")),
        "current-crate filter let std::* through: {names:?}"
    );

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

/// Variables-view §5.4 — bulk enumeration of TLS via
/// `Debugger::read_thread_local_variables`. The fixture declares
/// `THREAD_LOCAL_VAR_1` and `THREAD_LOCAL_VAR_2`; rustc lowers
/// each to a `DW_TAG_variable` named `__KEY` / `VAL` /
/// `__RUST_STD_INTERNAL_VAL` (the precise name depends on the
/// rustc version) nested under the user identifier's namespace.
#[test]
#[serial]
fn test_bulk_enumerate_thread_locals() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 168).unwrap();
    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(168));

    let tls = debugger
        .read_thread_local_variables(FileScopeFilter::CurrentCrate)
        .unwrap();
    let names: Vec<String> = tls.iter().map(|r| r.identity().to_string()).collect();

    // §5.4 known limit: `root_from_die` succeeds only when the TLS
    // slot's *value* is readable from the current thread. For
    // non-const-init thread_locals (THREAD_LOCAL_VAR_1, _2 here)
    // the slot isn't initialised on the main thread at the time
    // of the breakpoint, so the value-parse step returns None and
    // the entry is dropped. const-init thread_locals like
    // CONSTANT_THREAD_LOCAL *are* always readable.
    //
    // Asserting the const-init case proves the path works
    // end-to-end. The runtime-init case will start surfacing
    // entries once §5.4 gains the "<unavailable>" placeholder
    // fallback for unreadable values (see variables-view.md §7).
    assert!(
        names.iter().any(|n| n.contains("CONSTANT_THREAD_LOCAL")),
        "expected at least one TLS entry; got: {names:?}"
    );
    // And the non-TLS statics must NOT appear here even when
    // their values are perfectly readable — the kind filter must
    // exclude them by name-symbol regardless of parse success.
    for forbidden in ["GLOB_1", "GLOB_2", "GLOB_3"] {
        assert!(
            names.iter().all(|n| !n.contains(forbidden)),
            "{forbidden} leaked into thread-locals: {names:?}"
        );
    }

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

/// Variables-view §5.2 — the mutability classifier should run
/// without panicking on every variable produced by the bulk
/// enumeration APIs, and the GLOB_2 static (a plain `static i32`)
/// should classify as ReadOnly because the linker puts it in
/// `.rodata` regardless of the rustc / LLVM version. Other GLOB_*
/// statics may land in `.data.rel.ro` (read-only after relocation)
/// or similar — we don't assert on them to stay portable across
/// linker quirks.
#[test]
#[serial]
fn test_mutability_classifier_runs_on_live_variables() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 168).unwrap();
    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(168));

    // Every static must classify into some Mutability variant
    // without panicking. The exact bucket depends on the linker
    // (`.rodata` vs `.data.rel.ro`); we just ensure the path
    // returns something for every entry.
    let statics = debugger
        .read_static_variables(FileScopeFilter::CurrentCrate)
        .unwrap();
    assert!(!statics.is_empty(), "no statics enumerated");
    for qr in &statics {
        let _ = mutability::classify(qr, &debugger);
    }

    // GLOB_2 is `static GLOB_2: i32 = 2;` — pure integer literal,
    // no relocations, lands in `.rodata` on every supported
    // toolchain. Assert it classifies as ReadOnly to lock that in.
    let glob_2 = statics
        .iter()
        .find(|qr| qr.identity().to_string().contains("GLOB_2"))
        .expect("GLOB_2 not enumerated");
    assert_eq!(
        mutability::classify(glob_2, &debugger),
        Mutability::ReadOnly,
        "static GLOB_2 should be ReadOnly (in .rodata)"
    );

    // Local variables go through the type-based classifier. We
    // don't assert specifics (the fixture's locals are mostly
    // owned types which default-RW per the let-mut DWARF gap) —
    // just exercise the path and verify it doesn't panic.
    let locals = debugger.read_local_variables().unwrap();
    for qr in &locals {
        let _ = mutability::classify(qr, &debugger);
    }

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_read_only_local_variables() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 168).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(168));

    let vars = debugger
        .read_variable(Dqe::Variable(Selector::by_name("GLOB_1", true)))
        .unwrap();
    assert_eq!(vars.len(), 0);

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_read_static_variables_different_modules() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 179).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(179));

    read_var_dqe_type_order!(debugger, Dqe::Variable(Selector::by_name("GLOB_3", false)) => glob_3_1, glob_3_2);
    assert_idents!(glob_3_1 => "vars::ns_1::GLOB_3");
    assert_str(glob_3_1.value(), "glob_3");

    assert_idents!(glob_3_2 => "vars::GLOB_3");
    assert_scalar(glob_3_2.value(), "i32", Some(SupportedScalar::I32(3)));

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_read_tls_variables() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();
    let rust_version = rust_version(VARS_APP).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 194).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(194));

    read_var_dqe!(debugger, Dqe::Variable(Selector::by_name(
            "THREAD_LOCAL_VAR_1",
            false,
        )) => tls_var_1);

    version_switch!(
        rust_version,
        .. (1 . 80) => {
            assert_idents!(tls_var_1 => "vars::THREAD_LOCAL_VAR_1::__getit::__KEY");
        },
        (1 . 80) .. (1 . 92) => {
            assert_idents!(tls_var_1 => "vars::THREAD_LOCAL_VAR_1::{constant#0}::{closure#1}::VAL");
        },
        (1 . 92) .. => {
            assert_idents!(tls_var_1 => "vars::THREAD_LOCAL_VAR_1::{constant#0}::{closure#1}::__RUST_STD_INTERNAL_VAL");
        }
    );
    assert_init_tls(tls_var_1.value(), "Cell<i32>", |inner| {
        assert_cell(inner, "Cell<i32>", |value| {
            assert_scalar(value, "i32", Some(SupportedScalar::I32(2)))
        })
    });

    read_var_dqe!(debugger, Dqe::Variable(Selector::by_name(
            "THREAD_LOCAL_VAR_2",
            false,
        )) => tls_var_2);
    version_switch!(
        rust_version,
        .. (1 . 80) => {
            assert_idents!(tls_var_2 => "vars::THREAD_LOCAL_VAR_2::__getit::__KEY");
        },
        (1 . 80) .. (1 . 92) => {
            assert_idents!(tls_var_2 => "vars::THREAD_LOCAL_VAR_2::{constant#0}::{closure#1}::VAL");
        },
        (1 . 92) .. => {
            assert_idents!(tls_var_2 => "vars::THREAD_LOCAL_VAR_2::{constant#0}::{closure#1}::__RUST_STD_INTERNAL_VAL");
        }
    );
    assert_init_tls(tls_var_2.value(), "Cell<&str>", |inner| {
        assert_cell(inner, "Cell<&str>", |value| assert_str(value, "2"))
    });

    // assert uninit tls variables
    debugger.set_breakpoint_at_line("vars.rs", 199).unwrap();
    debugger.continue_debugee().unwrap();
    assert_eq!(info.line.take(), Some(199));

    version_switch!(
            rust_version,
            .. (1 . 80) => {
                read_var_dqe!(debugger, Dqe::Variable(Selector::by_name(
                    "THREAD_LOCAL_VAR_1",
                    false,
                )) => tls_var_1);
                assert_idents!(tls_var_1 => "vars::THREAD_LOCAL_VAR_1::__getit::__KEY");
                assert_uninit_tls(tls_var_1.value(), "Cell<i32>");
            },
            (1 . 80) .. => {
                let vars = debugger.read_variable(Dqe::Variable(Selector::by_name(
                    "THREAD_LOCAL_VAR_1",
                    false,
                ))).unwrap();
                assert!(vars.is_empty());
            },
    );

    // assert tls variables changes in another thread
    debugger.set_breakpoint_at_line("vars.rs", 203).unwrap();
    debugger.continue_debugee().unwrap();
    assert_eq!(info.line.take(), Some(203));

    read_var_dqe!(debugger, Dqe::Variable(Selector::by_name(
            "THREAD_LOCAL_VAR_1",
            false,
        )) => tls_var_1);
    assert_init_tls(tls_var_1.value(), "Cell<i32>", |inner| {
        assert_cell(inner, "Cell<i32>", |value| {
            assert_scalar(value, "i32", Some(SupportedScalar::I32(1)))
        })
    });

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_read_tls_const_variables() {
    let rust_version = rust_version(VARS_APP).unwrap();
    if rust_version < Version((1, 79, 0)) {
        return;
    }

    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 538).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(538));

    read_var_dqe!(debugger, Dqe::Variable(Selector::by_name(
            "CONSTANT_THREAD_LOCAL",
            false,
        )) => const_tls);
    // Darwin's dsymutil numbers anonymous closures starting at 1
    // (Linux rustc emits 0). Same DIE, different index — accept either.
    let ident = const_tls.identity().to_string();
    let normalised = ident.replace("{closure#1}", "{closure#0}");
    version_switch!(
        rust_version,
        .. (1 . 80) => {
            assert_eq!(normalised, "vars::thread_local_const_init::CONSTANT_THREAD_LOCAL::__getit::VAL");
        },
        (1 . 80) .. (1 . 92) => {
            assert_eq!(normalised, "vars::thread_local_const_init::CONSTANT_THREAD_LOCAL::{constant#0}::{closure#0}::VAL");
        },
        (1 . 92) .. => {
            assert_eq!(normalised, "vars::thread_local_const_init::CONSTANT_THREAD_LOCAL::{constant#0}::{closure#0}::__RUST_STD_INTERNAL_VAL");
        }
    );
    assert_init_tls(const_tls.value(), "i32", |value| {
        assert_scalar(value, "i32", Some(SupportedScalar::I32(1337)))
    });

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_read_closures() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 223).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(223));

    read_locals!(debugger => inc, inc_mut, _outer, closure, _a, _b, _c, trait_once, trait_mut, trait_fn, fn_ptr);
    assert_idents!(
        inc => "inc", inc_mut => "inc_mut", closure => "closure", trait_once => "trait_once",
        trait_mut => "trait_mut", trait_fn => "trait_fn", fn_ptr => "fn_ptr"
    );

    assert_struct(inc.value(), "{closure_env#0}", |_, _| {
        panic!("no members expected")
    });
    assert_struct(inc_mut.value(), "{closure_env#1}", |_, _| {
        panic!("no members expected")
    });
    assert_struct(closure.value(), "{closure_env#2}", |_, member| {
        assert_member(member, "outer", |val| assert_string(val, "outer val"))
    });
    let rust_version = rust_version(VARS_APP).unwrap();
    assert_struct(
        trait_once.value(),
        "alloc::boxed::Box<dyn core::ops::function::FnOnce<(), Output=()>, alloc::alloc::Global>",
        |i, member| match i {
            0 => {
                assert_member(member, "pointer", |val| {
                    assert_pointer(val, "*dyn core::ops::function::FnOnce<(), Output=()>")
                });
                let member_val = member.value.clone();
                let deref = trait_once
                    .clone()
                    .modify_value(|pcx, _| member_val.deref(pcx));
                assert_struct(
                    deref.unwrap().value(),
                    "dyn core::ops::function::FnOnce<(), Output=()>",
                    |_, _| {},
                );
            }
            1 => {
                let exp_type = if rust_version >= Version((1, 80, 0)) {
                    "&[usize; 4]"
                } else {
                    "&[usize; 3]"
                };
                assert_member(member, "vtable", |val| assert_pointer(val, exp_type));
                let member_val = member.value.clone();
                let deref = trait_once
                    .clone()
                    .modify_value(|pcx, _| member_val.deref(pcx));
                assert_array(deref.unwrap().value(), "[usize]", |_, _| {});
            }
            _ => panic!("2 members expected"),
        },
    );
    assert_struct(
        trait_mut.value(),
        "alloc::boxed::Box<dyn core::ops::function::FnMut<(), Output=()>, alloc::alloc::Global>",
        |i, member| match i {
            0 => {
                assert_member(member, "pointer", |val| {
                    assert_pointer(val, "*dyn core::ops::function::FnMut<(), Output=()>")
                });
                let member_val = member.value.clone();
                let deref = trait_mut
                    .clone()
                    .modify_value(|pcx, _| member_val.deref(pcx));
                assert_struct(
                    deref.unwrap().value(),
                    "dyn core::ops::function::FnMut<(), Output=()>",
                    |_, _| {},
                );
            }
            1 => {
                let exp_type = if rust_version >= Version((1, 80, 0)) {
                    "&[usize; 5]"
                } else {
                    "&[usize; 3]"
                };
                assert_member(member, "vtable", |val| assert_pointer(val, exp_type));
                let member_val = member.value.clone();
                let deref = trait_mut
                    .clone()
                    .modify_value(|pcx, _| member_val.deref(pcx));
                assert_array(deref.unwrap().value(), "[usize]", |_, _| {});
            }
            _ => panic!("2 members expected"),
        },
    );
    assert_struct(
        trait_fn.value(),
        "alloc::boxed::Box<dyn core::ops::function::Fn<(), Output=()>, alloc::alloc::Global>",
        |i, member| match i {
            0 => {
                assert_member(member, "pointer", |val| {
                    assert_pointer(val, "*dyn core::ops::function::Fn<(), Output=()>")
                });
                let member_val = member.value.clone();
                let deref = trait_fn
                    .clone()
                    .modify_value(|pcx, _| member_val.deref(pcx));
                assert_struct(
                    deref.unwrap().value(),
                    "dyn core::ops::function::Fn<(), Output=()>",
                    |_, _| {},
                );
            }
            1 => {
                let exp_type = if rust_version >= Version((1, 80, 0)) {
                    "&[usize; 6]"
                } else {
                    "&[usize; 3]"
                };
                assert_member(member, "vtable", |val| assert_pointer(val, exp_type));
                let member_val = member.value.clone();
                let deref = trait_fn
                    .clone()
                    .modify_value(|pcx, _| member_val.deref(pcx));
                assert_array(deref.unwrap().value(), "[usize]", |_, _| {});
            }
            _ => panic!("2 members expected"),
        },
    );
    assert_pointer(fn_ptr.value(), "fn() -> u8");

    let deref_fn_ptr = fn_ptr.clone().modify_value(|ctx, v| v.deref(ctx));
    assert!(deref_fn_ptr.is_none());

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_arguments() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 232).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(232));

    read_arg_dqe!(debugger, Dqe::Variable(Selector::Any) => by_val, by_ref, vec, box_arr);
    assert_idents!(by_val => "by_val", by_ref => "by_ref", vec => "vec", box_arr => "box_arr");

    assert_scalar(by_val.value(), "i32", Some(SupportedScalar::I32(1)));

    assert_pointer(by_ref.value(), "&i32");
    let deref = by_ref.clone().modify_value(|pcx, value| value.deref(pcx));
    assert_scalar(deref.unwrap().value(), "i32", Some(SupportedScalar::I32(2)));

    assert_vec(vec.value(), "Vec<u8, alloc::alloc::Global>", 3, |buf| {
        assert_array(buf, "[u8]", |i, item| match i {
            0 => assert_scalar(item, "u8", Some(SupportedScalar::U8(3))),
            1 => assert_scalar(item, "u8", Some(SupportedScalar::U8(4))),
            2 => assert_scalar(item, "u8", Some(SupportedScalar::U8(5))),
            _ => panic!("3 items expected"),
        })
    });

    // Phase 1 S16: the Vec<u8> render goes through the byte-preview
    // path *only when the bytes look like text* (printable ASCII +
    // common whitespace; see `vec_bytes_are_stringy`). Bytes 3/4/5
    // are valid utf-8 but not printable, so `b"\u{3}\u{4}\u{5}"`
    // would be harder to read than the numeric form — the renderer
    // deliberately falls through to `IndexedList`. Assert that's
    // what we get; the renderer prints these as `Vec<u8> [3, 4, 5]`.
    use bugstalker::debugger::variable::render::ValueLayout;
    let layout = vec.value().value_layout().expect("Vec<u8> layout missing");
    match layout {
        ValueLayout::IndexedList(items) => {
            let got: Vec<u64> = items
                .iter()
                .filter_map(|it| match &it.value {
                    Value::Scalar(s) => match s.value {
                        Some(SupportedScalar::U8(b)) => Some(b as u64),
                        _ => None,
                    },
                    _ => None,
                })
                .collect();
            assert_eq!(
                got,
                vec![3, 4, 5],
                "non-stringy Vec<u8> should render as numeric IndexedList"
            );
        }
        other => panic!("expected numeric IndexedList for non-printable bytes, got {other:?}"),
    }

    assert_struct(
        box_arr.value(),
        "alloc::boxed::Box<[u8], alloc::alloc::Global>",
        |i, member| match i {
            0 => {
                assert_member(member, "data_ptr", |val| assert_pointer(val, "*u8"));
                let data_ptr_val = member.value.clone();
                let deref = box_arr
                    .clone()
                    .modify_value(|pcx, _| data_ptr_val.deref(pcx));
                assert_scalar(deref.unwrap().value(), "u8", Some(SupportedScalar::U8(6)));
            }
            1 => assert_member(member, "length", |val| {
                assert_scalar(val, "usize", Some(SupportedScalar::Usize(3)))
            }),
            _ => panic!("2 members expected"),
        },
    );

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_read_union() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 244).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(244));

    read_locals!(debugger => union);
    assert_idents!(union => "union");
    assert_struct(union.value(), "Union1", |i, member| match i {
        0 => assert_member(member, "f1", |val| {
            assert_scalar(val, "f32", Some(SupportedScalar::F32(1.1)))
        }),
        1 => {}
        2 => {}
        _ => panic!("3 members expected"),
    });

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_read_hashmap() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 290).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(290));

    let rust_version = rust_version(VARS_APP).unwrap();
    let hash_map_type = version_switch!(
            rust_version,
            .. (1 . 76) => "HashMap<bool, i64, std::collections::hash::map::RandomState>",
            (1 . 76) .. (1 . 94) => "HashMap<bool, i64, std::hash::random::RandomState>",
            (1 . 94) .. => "HashMap<bool, i64, std::hash::random::RandomState, alloc::alloc::Global>",
    )
    .unwrap();

    read_locals!(debugger => hm1, hm2, hm3, hm4, _a, b, _hm5, _hm6);
    assert_idents!(hm1 => "hm1", hm2 => "hm2", hm3 => "hm3", hm4 => "hm4");

    assert_hashmap(hm1.value(), hash_map_type, |items| {
        assert_eq!(items.len(), 2);
        assert_scalar(&items[0].0, "bool", Some(SupportedScalar::Bool(false)));
        assert_scalar(&items[0].1, "i64", Some(SupportedScalar::I64(5)));
        assert_scalar(&items[1].0, "bool", Some(SupportedScalar::Bool(true)));
        assert_scalar(&items[1].1, "i64", Some(SupportedScalar::I64(3)));
    });

    let hash_map_type = version_switch!(
            rust_version,
            .. (1 . 76) => "HashMap<&str, alloc::vec::Vec<i32, alloc::alloc::Global>, std::collections::hash::map::RandomState>",
            (1 . 76) .. (1 . 94) => "HashMap<&str, alloc::vec::Vec<i32, alloc::alloc::Global>, std::hash::random::RandomState>",
            (1 . 94) .. => "HashMap<&str, alloc::vec::Vec<i32, alloc::alloc::Global>, std::hash::random::RandomState, alloc::alloc::Global>",
    ).unwrap();
    assert_hashmap(hm2.value(), hash_map_type, |items| {
        assert_eq!(items.len(), 2);
        assert_str(&items[0].0, "abc");
        assert_vec(&items[0].1, "Vec<i32, alloc::alloc::Global>", 3, |buf| {
            assert_array(buf, "[i32]", |i, item| match i {
                0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(1))),
                1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(2))),
                2 => assert_scalar(item, "i32", Some(SupportedScalar::I32(3))),
                _ => panic!("3 items expected"),
            })
        });
        assert_str(&items[1].0, "efg");
        assert_vec(&items[1].1, "Vec<i32, alloc::alloc::Global>", 3, |buf| {
            assert_array(buf, "[i32]", |i, item| match i {
                0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(11))),
                1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(12))),
                2 => assert_scalar(item, "i32", Some(SupportedScalar::I32(13))),
                _ => panic!("3 items expected"),
            })
        });
    });

    let hash_map_type = version_switch!(
            rust_version,
            .. (1 . 76) => "HashMap<i32, i32, std::collections::hash::map::RandomState>",
            (1 . 76) .. (1 . 94) => "HashMap<i32, i32, std::hash::random::RandomState>",
            (1 . 94) .. => "HashMap<i32, i32, std::hash::random::RandomState, alloc::alloc::Global>",
    )
    .unwrap();
    assert_hashmap(hm3.value(), hash_map_type, |items| {
        assert_eq!(items.len(), 100);

        let mut exp_items = (0..100).collect::<Vec<_>>();
        exp_items.sort_by_key(|i1| i1.to_string());

        for i in 0..100 {
            assert_scalar(&items[i].0, "i32", Some(SupportedScalar::I32(exp_items[i])));
        }
        for i in 0..100 {
            assert_scalar(&items[i].1, "i32", Some(SupportedScalar::I32(exp_items[i])));
        }
    });

    let hash_map_type = version_switch!(
            rust_version,
            .. (1 . 76) => "HashMap<alloc::string::String, std::collections::hash::map::HashMap<i32, i32, std::collections::hash::map::RandomState>, std::collections::hash::map::RandomState>",
            (1 . 76) .. (1 . 94) => "HashMap<alloc::string::String, std::collections::hash::map::HashMap<i32, i32, std::hash::random::RandomState>, std::hash::random::RandomState>",
            (1 . 94) .. => "HashMap<alloc::string::String, std::collections::hash::map::HashMap<i32, i32, std::hash::random::RandomState, alloc::alloc::Global>, std::hash::random::RandomState, alloc::alloc::Global>",
    ).unwrap();
    let inner_hash_map_type = version_switch!(
            rust_version,
            .. (1 . 76) => "HashMap<i32, i32, std::collections::hash::map::RandomState>",
            (1 . 76) .. (1 . 94) => "HashMap<i32, i32, std::hash::random::RandomState>",
            (1 . 94) .. => "HashMap<i32, i32, std::hash::random::RandomState, alloc::alloc::Global>",
    )
    .unwrap();
    assert_hashmap(hm4.value(), hash_map_type, |items| {
        assert_eq!(items.len(), 2);
        assert_string(&items[0].0, "1");
        assert_hashmap(&items[0].1, inner_hash_map_type, |items| {
            assert_eq!(items.len(), 2);
            assert_scalar(&items[0].0, "i32", Some(SupportedScalar::I32(1)));
            assert_scalar(&items[0].1, "i32", Some(SupportedScalar::I32(1)));
            assert_scalar(&items[1].0, "i32", Some(SupportedScalar::I32(2)));
            assert_scalar(&items[1].1, "i32", Some(SupportedScalar::I32(2)));
        });

        assert_string(&items[1].0, "3");
        assert_hashmap(&items[1].1, inner_hash_map_type, |items| {
            assert_eq!(items.len(), 2);
            assert_scalar(&items[0].0, "i32", Some(SupportedScalar::I32(3)));
            assert_scalar(&items[0].1, "i32", Some(SupportedScalar::I32(3)));
            assert_scalar(&items[1].0, "i32", Some(SupportedScalar::I32(4)));
            assert_scalar(&items[1].1, "i32", Some(SupportedScalar::I32(4)));
        });
    });

    let make_idx_dqe = |var: &str, literal| {
        Dqe::Index(Dqe::Variable(Selector::by_name(var, true)).boxed(), literal)
    };

    // get by bool key
    let dqe = make_idx_dqe("hm1", Literal::Bool(true));
    read_var_dqe!(debugger, dqe => val);
    assert_scalar(val.value(), "i64", Some(SupportedScalar::I64(3)));

    // get by string key
    let dqe = make_idx_dqe("hm2", Literal::String("efg".to_string()));
    read_var_dqe!(debugger, dqe => val);
    assert_vec(val.value(), "Vec<i32, alloc::alloc::Global>", 3, |buf| {
        assert_array(buf, "[i32]", |i, item| match i {
            0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(11))),
            1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(12))),
            2 => assert_scalar(item, "i32", Some(SupportedScalar::I32(13))),
            _ => panic!("3 items expected"),
        })
    });

    // get by int key
    let dqe = make_idx_dqe("hm3", Literal::Int(99));
    read_var_dqe!(debugger, dqe => val);
    assert_scalar(val.value(), "i32", Some(SupportedScalar::I32(99)));

    // get by pointer key
    let Value::Pointer(ptr) = &b.value() else {
        panic!("not a pointer")
    };
    let ptr_val = ptr.value.unwrap() as usize;

    let dqe = make_idx_dqe("hm5", Literal::Address(ptr_val));
    read_var_dqe!(debugger, dqe => val);
    assert_str(val.value(), "b");

    // get by complex object
    let dqe = make_idx_dqe(
        "hm6",
        Literal::AssocArray(HashMap::from([
            (
                "field_1".to_string(),
                LiteralOrWildcard::Literal(Literal::Int(1)),
            ),
            (
                "field_2".to_string(),
                LiteralOrWildcard::Literal(Literal::Array(Box::new([
                    LiteralOrWildcard::Literal(Literal::String("a".to_string())),
                    LiteralOrWildcard::Wildcard,
                ]))),
            ),
            (
                "field_3".to_string(),
                LiteralOrWildcard::Literal(Literal::EnumVariant(
                    "Some".to_string(),
                    Some(Box::new(Literal::Array(Box::new([
                        LiteralOrWildcard::Literal(Literal::Bool(true)),
                    ])))),
                )),
            ),
        ])),
    );
    read_var_dqe!(debugger, dqe => val);
    assert_scalar(val.value(), "i32", Some(SupportedScalar::I32(1)));

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_read_hashset() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 307).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(307));

    let rust_version = rust_version(VARS_APP).unwrap();
    let hashset_type = version_switch!(
            rust_version,
            .. (1 . 76) => "HashSet<i32, std::collections::hash::map::RandomState>",
            (1 . 76) .. (1 . 94) => "HashSet<i32, std::hash::random::RandomState>",
            (1 . 94) .. => "HashSet<i32, std::hash::random::RandomState, alloc::alloc::Global>",
    )
    .unwrap();

    read_locals!(debugger => hs1, hs2, hs3, _a, b, _hs4);
    assert_idents!(hs1 => "hs1", hs2 => "hs2", hs3 => "hs3");

    assert_hashset(hs1.value(), hashset_type, |items| {
        assert_eq!(items.len(), 4);
        assert_scalar(&items[0], "i32", Some(SupportedScalar::I32(1)));
        assert_scalar(&items[1], "i32", Some(SupportedScalar::I32(2)));
        assert_scalar(&items[2], "i32", Some(SupportedScalar::I32(3)));
        assert_scalar(&items[3], "i32", Some(SupportedScalar::I32(4)));
    });
    assert_hashset(hs2.value(), hashset_type, |items| {
        assert_eq!(items.len(), 100);
        let mut exp_items = (0..100).collect::<Vec<_>>();
        exp_items.sort_by_key(|i1| i1.to_string());

        for i in 0..100 {
            assert_scalar(&items[i], "i32", Some(SupportedScalar::I32(exp_items[i])));
        }
    });

    let hashset_type = version_switch!(
            rust_version,
            .. (1 . 76) => "HashSet<alloc::vec::Vec<i32, alloc::alloc::Global>, std::collections::hash::map::RandomState>",
            (1 . 76) .. (1 . 94) => "HashSet<alloc::vec::Vec<i32, alloc::alloc::Global>, std::hash::random::RandomState>",
            (1 . 94) .. => "HashSet<alloc::vec::Vec<i32, alloc::alloc::Global>, std::hash::random::RandomState, alloc::alloc::Global>",
    ).unwrap();
    assert_hashset(hs3.value(), hashset_type, |items| {
        assert_eq!(items.len(), 1);
        assert_vec(&items[0], "Vec<i32, alloc::alloc::Global>", 2, |buf| {
            assert_array(buf, "[i32]", |i, item| match i {
                0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(1))),
                1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(2))),
                _ => panic!("2 items expected"),
            })
        });
    });

    let make_idx_dqe = |var: &str, literal| {
        Dqe::Index(Dqe::Variable(Selector::by_name(var, true)).boxed(), literal)
    };

    // get by int key
    let dqe = make_idx_dqe("hs1", Literal::Int(2));
    read_var_dqe!(debugger, dqe => val);
    assert_scalar(val.value(), "bool", Some(SupportedScalar::Bool(true)));

    let dqe = make_idx_dqe("hs1", Literal::Int(5));
    read_var_dqe!(debugger, dqe => val);
    assert_scalar(val.value(), "bool", Some(SupportedScalar::Bool(false)));

    // get by pointer key
    let Value::Pointer(ptr) = &b.value() else {
        panic!("not a pointer")
    };
    let ptr_val = ptr.value.unwrap() as usize;

    let dqe = make_idx_dqe("hs4", Literal::Address(ptr_val));
    read_var_dqe!(debugger, dqe => val);
    assert_scalar(val.value(), "bool", Some(SupportedScalar::Bool(true)));

    let dqe = make_idx_dqe("hs4", Literal::Address(0));
    read_var_dqe!(debugger, dqe => val);
    assert_scalar(val.value(), "bool", Some(SupportedScalar::Bool(false)));

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_circular_ref_types() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 334).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(334));

    read_locals!(debugger => a_circ, b_circ);
    assert_idents!(a_circ => "a_circ", b_circ => "b_circ");

    assert_rc(
        a_circ.value(),
        "Rc<vars::circular::List, alloc::alloc::Global>",
    );
    assert_rc(
        b_circ.value(),
        "Rc<vars::circular::List, alloc::alloc::Global>",
    );

    let deref = a_circ.clone().modify_value(|pcx, v| v.deref(pcx));
    let rust_version = rust_version(VARS_APP).unwrap();
    let deref_type = version_switch!(
        rust_version,
        .. (1 . 84) => "RcBox<vars::circular::List>",
        (1 . 84) .. => "RcInner<vars::circular::List>",
    )
    .unwrap();
    assert_struct(deref.unwrap().value(), deref_type, |i, member| match i {
        0 => assert_member(member, "strong", |val| {
            assert_cell(val, "Cell<usize>", |inner| {
                assert_scalar(inner, "usize", Some(SupportedScalar::Usize(2)))
            })
        }),
        1 => assert_member(member, "weak", |val| {
            assert_cell(val, "Cell<usize>", |inner| {
                assert_scalar(inner, "usize", Some(SupportedScalar::Usize(1)))
            })
        }),
        2 => {
            assert_member(member, "value", |val| {
                assert_rust_enum(val, "List", |enum_member| {
                    assert_struct(enum_member, "Cons", |i, cons_member| match i {
                        0 => assert_member(cons_member, "__0", |val| {
                            assert_scalar(val, "i32", Some(SupportedScalar::I32(5)))
                        }),
                        1 => assert_member(cons_member, "__1", |val| {
                            assert_refcell(
                                val,
                                "RefCell<alloc::rc::Rc<vars::circular::List, alloc::alloc::Global>>",
                                0,
                                |inner| {
                                    assert_rc(
                                        inner,
                                        "Rc<vars::circular::List, alloc::alloc::Global>",
                                    )
                                },
                            )
                        }),
                        _ => panic!("2 members expected"),
                    });
                })
            });
        }
        _ => panic!("3 members expected"),
    });

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_lexical_blocks() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 340).unwrap();
    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(340));

    read_locals!(debugger => alpha, _beta);
    // WAITFORFIX: https://github.com/rust-lang/rust/issues/113819
    // expected:     assert_eq!(vars.len(), 1);
    // through this bug there is uninitialized variable here
    assert_idents!(alpha => "alpha");

    debugger.set_breakpoint_at_line("vars.rs", 342).unwrap();
    debugger.continue_debugee().unwrap();
    assert_eq!(info.line.take(), Some(342));

    read_locals!(debugger => alpha, beta);
    assert_idents!(alpha => "alpha", beta => "beta");

    debugger.set_breakpoint_at_line("vars.rs", 343).unwrap();
    debugger.continue_debugee().unwrap();
    assert_eq!(info.line.take(), Some(343));

    read_locals!(debugger => alpha, beta, gama);
    assert_idents!(alpha => "alpha", beta => "beta", gama => "gama");

    debugger.set_breakpoint_at_line("vars.rs", 349).unwrap();
    debugger.continue_debugee().unwrap();
    assert_eq!(info.line.take(), Some(349));

    read_locals!(debugger => alpha, delta);
    assert_idents!(alpha => "alpha", delta => "delta");

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_btree_map() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 396).unwrap();
    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(396));

    read_locals!(debugger => hm1, hm2, hm3, hm4, _a, b, _hm5, _hm6);
    assert_idents!(hm1 => "hm1", hm2 => "hm2", hm3 => "hm3", hm4 => "hm4");

    assert_btree_map(
        hm1.value(),
        "BTreeMap<bool, i64, alloc::alloc::Global>",
        |items| {
            assert_eq!(items.len(), 2);
            assert_scalar(&items[0].0, "bool", Some(SupportedScalar::Bool(false)));
            assert_scalar(&items[0].1, "i64", Some(SupportedScalar::I64(5)));
            assert_scalar(&items[1].0, "bool", Some(SupportedScalar::Bool(true)));
            assert_scalar(&items[1].1, "i64", Some(SupportedScalar::I64(3)));
        },
    );

    assert_btree_map(
        hm2.value(),
        "BTreeMap<&str, alloc::vec::Vec<i32, alloc::alloc::Global>, alloc::alloc::Global>",
        |items| {
            assert_eq!(items.len(), 2);
            assert_str(&items[0].0, "abc");
            assert_vec(&items[0].1, "Vec<i32, alloc::alloc::Global>", 3, |buf| {
                assert_array(buf, "[i32]", |i, item| match i {
                    0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(1))),
                    1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(2))),
                    2 => assert_scalar(item, "i32", Some(SupportedScalar::I32(3))),
                    _ => panic!("3 items expected"),
                })
            });
            assert_str(&items[1].0, "efg");
            assert_vec(&items[1].1, "Vec<i32, alloc::alloc::Global>", 3, |buf| {
                assert_array(buf, "[i32]", |i, item| match i {
                    0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(11))),
                    1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(12))),
                    2 => assert_scalar(item, "i32", Some(SupportedScalar::I32(13))),
                    _ => panic!("3 items expected"),
                })
            });
        },
    );

    assert_btree_map(
        hm3.value(),
        "BTreeMap<i32, i32, alloc::alloc::Global>",
        |items| {
            assert_eq!(items.len(), 100);

            let exp_items = (0..100).collect::<Vec<_>>();

            for i in 0..100 {
                assert_scalar(&items[i].0, "i32", Some(SupportedScalar::I32(exp_items[i])));
            }
            for i in 0..100 {
                assert_scalar(&items[i].1, "i32", Some(SupportedScalar::I32(exp_items[i])));
            }
        },
    );

    assert_btree_map(
        hm4.value(),
        "BTreeMap<alloc::string::String, alloc::collections::btree::map::BTreeMap<i32, i32, alloc::alloc::Global>, alloc::alloc::Global>",
        |items| {
            assert_eq!(items.len(), 2);
            assert_string(&items[0].0, "1");
            assert_btree_map(
                &items[0].1,
                "BTreeMap<i32, i32, alloc::alloc::Global>",
                |items| {
                    assert_eq!(items.len(), 2);
                    assert_scalar(&items[0].0, "i32", Some(SupportedScalar::I32(1)));
                    assert_scalar(&items[0].1, "i32", Some(SupportedScalar::I32(1)));
                    assert_scalar(&items[1].0, "i32", Some(SupportedScalar::I32(2)));
                    assert_scalar(&items[1].1, "i32", Some(SupportedScalar::I32(2)));
                },
            );

            assert_string(&items[1].0, "3");
            assert_btree_map(
                &items[1].1,
                "BTreeMap<i32, i32, alloc::alloc::Global>",
                |items| {
                    assert_eq!(items.len(), 2);
                    assert_scalar(&items[0].0, "i32", Some(SupportedScalar::I32(3)));
                    assert_scalar(&items[0].1, "i32", Some(SupportedScalar::I32(3)));
                    assert_scalar(&items[1].0, "i32", Some(SupportedScalar::I32(4)));
                    assert_scalar(&items[1].1, "i32", Some(SupportedScalar::I32(4)));
                },
            );
        },
    );

    let make_idx_dqe = |var: &str, literal| {
        Dqe::Index(Dqe::Variable(Selector::by_name(var, true)).boxed(), literal)
    };

    // get by bool key
    let dqe = make_idx_dqe("hm1", Literal::Bool(true));
    read_var_dqe!(debugger, dqe => val);
    assert_scalar(val.value(), "i64", Some(SupportedScalar::I64(3)));

    // get by string key
    let dqe = make_idx_dqe("hm2", Literal::String("efg".to_string()));
    read_var_dqe!(debugger, dqe => val);
    assert_vec(val.value(), "Vec<i32, alloc::alloc::Global>", 3, |buf| {
        assert_array(buf, "[i32]", |i, item| match i {
            0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(11))),
            1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(12))),
            2 => assert_scalar(item, "i32", Some(SupportedScalar::I32(13))),
            _ => panic!("3 items expected"),
        })
    });

    // get by int key
    let dqe = make_idx_dqe("hm3", Literal::Int(99));
    read_var_dqe!(debugger, dqe => val);
    assert_scalar(val.value(), "i32", Some(SupportedScalar::I32(99)));

    // get by pointer key
    let Value::Pointer(ptr) = b.value() else {
        panic!("not a pointer")
    };
    let ptr_val = ptr.value.unwrap() as usize;

    let dqe = make_idx_dqe("hm5", Literal::Address(ptr_val));
    read_var_dqe!(debugger, dqe => val);
    assert_str(val.value(), "b");

    // get by complex object
    let dqe = make_idx_dqe(
        "hm6",
        Literal::AssocArray(HashMap::from([
            ("field_1".to_string(), LiteralOrWildcard::Wildcard),
            (
                "field_2".to_string(),
                LiteralOrWildcard::Literal(Literal::Array(Box::new([
                    LiteralOrWildcard::Literal(Literal::String("c".to_string())),
                    LiteralOrWildcard::Wildcard,
                    LiteralOrWildcard::Wildcard,
                ]))),
            ),
            (
                "field_3".to_string(),
                LiteralOrWildcard::Literal(Literal::EnumVariant("None".to_string(), None)),
            ),
        ])),
    );
    read_var_dqe!(debugger, dqe => val);
    assert_scalar(val.value(), "i32", Some(SupportedScalar::I32(2)));

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_read_btree_set() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 413).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(413));

    read_locals!(debugger => hs1, hs2, hs3, _a, b, _hs4);
    assert_idents!(hs1 => "hs1", hs2 => "hs2", hs3 => "hs3");

    assert_btree_set(
        hs1.value(),
        "BTreeSet<i32, alloc::alloc::Global>",
        |items| {
            assert_eq!(items.len(), 4);
            assert_scalar(&items[0], "i32", Some(SupportedScalar::I32(1)));
            assert_scalar(&items[1], "i32", Some(SupportedScalar::I32(2)));
            assert_scalar(&items[2], "i32", Some(SupportedScalar::I32(3)));
            assert_scalar(&items[3], "i32", Some(SupportedScalar::I32(4)));
        },
    );

    assert_btree_set(
        hs2.value(),
        "BTreeSet<i32, alloc::alloc::Global>",
        |items| {
            assert_eq!(items.len(), 100);
            let exp_items = (0..100).collect::<Vec<_>>();

            for i in 0..100 {
                assert_scalar(&items[i], "i32", Some(SupportedScalar::I32(exp_items[i])));
            }
        },
    );

    assert_btree_set(
        hs3.value(),
        "BTreeSet<alloc::vec::Vec<i32, alloc::alloc::Global>, alloc::alloc::Global>",
        |items| {
            assert_eq!(items.len(), 1);
            assert_vec(&items[0], "Vec<i32, alloc::alloc::Global>", 2, |buf| {
                assert_array(buf, "[i32]", |i, item| match i {
                    0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(1))),
                    1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(2))),
                    _ => panic!("2 items expected"),
                })
            });
        },
    );

    let make_idx_dqe = |var: &str, literal| {
        Dqe::Index(Dqe::Variable(Selector::by_name(var, true)).boxed(), literal)
    };

    // get by int key
    let dqe = make_idx_dqe("hs1", Literal::Int(2));
    read_var_dqe!(debugger, dqe => val);
    assert_scalar(val.value(), "bool", Some(SupportedScalar::Bool(true)));

    let dqe = make_idx_dqe("hs1", Literal::Int(5));
    read_var_dqe!(debugger, dqe => val);
    assert_scalar(val.value(), "bool", Some(SupportedScalar::Bool(false)));

    // get by pointer key
    let Value::Pointer(ptr) = b.value() else {
        panic!("not a pointer")
    };
    let ptr_val = ptr.value.unwrap() as usize;

    let dqe = make_idx_dqe("hs4", Literal::Address(ptr_val));
    read_var_dqe!(debugger, dqe => val);
    assert_scalar(val.value(), "bool", Some(SupportedScalar::Bool(true)));

    let dqe = make_idx_dqe("hs4", Literal::Address(0));
    read_var_dqe!(debugger, dqe => val);
    assert_scalar(val.value(), "bool", Some(SupportedScalar::Bool(false)));

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_read_vec_deque() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 431).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(431));

    read_locals!(debugger => vd1, vd2);
    assert_idents!(vd1 => "vd1", vd2 => "vd2");

    assert_vec_deque(
        vd1.value(),
        "VecDeque<i32, alloc::alloc::Global>",
        8,
        |buf| {
            assert_array(buf, "[i32]", |i, item| match i {
                0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(9))),
                1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(10))),
                2 => assert_scalar(item, "i32", Some(SupportedScalar::I32(0))),
                3 => assert_scalar(item, "i32", Some(SupportedScalar::I32(1))),
                4 => assert_scalar(item, "i32", Some(SupportedScalar::I32(2))),
                _ => panic!("5 items expected"),
            })
        },
    );

    assert_vec_deque(
        vd2.value(),
        "VecDeque<alloc::collections::vec_deque::VecDeque<i32, alloc::alloc::Global>, alloc::alloc::Global>",
        4,
        |buf| {
            assert_array(
                buf,
                "[VecDeque<i32, alloc::alloc::Global>]",
                |i, item| match i {
                    0 => assert_vec_deque(item, "VecDeque<i32, alloc::alloc::Global>", 3, |buf| {
                        assert_array(buf, "[i32]", |i, item| match i {
                            0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(-2))),
                            1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(-1))),
                            2 => assert_scalar(item, "i32", Some(SupportedScalar::I32(0))),
                            _ => panic!("3 items expected"),
                        })
                    }),
                    1 => assert_vec_deque(item, "VecDeque<i32, alloc::alloc::Global>", 3, |buf| {
                        assert_array(buf, "[i32]", |i, item| match i {
                            0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(1))),
                            1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(2))),
                            2 => assert_scalar(item, "i32", Some(SupportedScalar::I32(3))),
                            _ => panic!("3 items expected"),
                        })
                    }),
                    2 => assert_vec_deque(item, "VecDeque<i32, alloc::alloc::Global>", 3, |buf| {
                        assert_array(buf, "[i32]", |i, item| match i {
                            0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(4))),
                            1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(5))),
                            2 => assert_scalar(item, "i32", Some(SupportedScalar::I32(6))),
                            _ => panic!("3 items expected"),
                        })
                    }),
                    _ => panic!("3 items expected"),
                },
            )
        },
    );

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_read_atomic() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 441).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(441));

    read_locals!(debugger => int32_atomic, _int32, int32_atomic_ptr);
    assert_idents!(int32_atomic => "int32_atomic", int32_atomic_ptr => "int32_atomic_ptr");

    // Phase 1 S3: AtomicI32 is now rendered as the bare scalar payload
    // (peeling the outer Atomic wrapper and the UnsafeCell wrapper).
    // The wrapper type identity is preserved on `Value::r#type()`.
    assert_atomic_scalar(
        int32_atomic.value(),
        "AtomicI32",
        "i32",
        SupportedScalar::I32(1),
    );

    // AtomicPtr<i32> peels to the inner *mut i32 pointer.
    assert_atomic_pointer(int32_atomic_ptr.value(), "AtomicPtr<i32>", "*mut i32");

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

/// Phase 1 S6 helper: assert a Range-family value rendered to text.
fn assert_range_text(val: &Value, exp_outer_type: &str, exp_text: &str) {
    let Value::Specialized {
        value: Some(SpecializedValue::Range(r)),
        ..
    } = val
    else {
        panic!("not a Range spec value: {:?}", val.r#type().name_fmt());
    };
    assert_eq!(val.r#type().name_fmt(), exp_outer_type);
    assert_eq!(r.render(), exp_text);
}

/// Phase 1 S2 helper: assert a lock guard peeled to the guarded T.
fn assert_lock_guard_inner(val: &Value, with_inner: impl FnOnce(&Value)) {
    let Value::Specialized {
        value: Some(SpecializedValue::LockGuard(inner)),
        ..
    } = val
    else {
        panic!("not a LockGuard spec value: {:?}", val.r#type().name_fmt());
    };
    with_inner(inner.as_ref());
}

/// Phase 1 S2 — `MutexGuard<T>` and `RwLockReadGuard<T>` peel
/// through their `lock` reference and the parent's
/// `data: UnsafeCell<T>` field to surface the guarded T directly.
#[test]
#[serial]
fn test_read_lock_guards() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 749).unwrap();
    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(749));

    let vars = debugger.read_local_variables().unwrap();
    let pick = |needle: &str| {
        vars.iter()
            .find(|v| v.identity().to_string().contains(needle))
            .unwrap_or_else(|| panic!("`{needle}` not in locals"))
    };

    assert_lock_guard_inner(pick("mtx_guard").value(), |inner| {
        assert_scalar(inner, "i32", Some(SupportedScalar::I32(123)));
    });
    assert_lock_guard_inner(pick("rwl_read").value(), |inner| {
        assert_scalar(inner, "i32", Some(SupportedScalar::I32(456)));
    });

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

/// Phase 1 S1 helper: assert a Mutex/RwLock peeled to its inner T.
fn assert_mutex_inner(val: &Value, with_inner: impl FnOnce(&Value)) {
    let Value::Specialized {
        value: Some(SpecializedValue::Mutex { inner, .. }),
        ..
    } = val
    else {
        panic!(
            "not a Mutex/RwLock spec value: {:?}",
            val.r#type().name_fmt()
        );
    };
    with_inner(inner.as_ref());
}

/// Phase 1 S1 (poison) helper: assert poison flag matches expectation.
fn assert_mutex_poisoned(val: &Value, exp_poisoned: bool) {
    let Value::Specialized {
        value: Some(SpecializedValue::Mutex { poisoned, .. }),
        ..
    } = val
    else {
        panic!(
            "not a Mutex/RwLock spec value: {:?}",
            val.r#type().name_fmt()
        );
    };
    assert_eq!(*poisoned, exp_poisoned);
}

/// Phase 1 S1 (state) helper: assert lock-state matches expectation.
/// The futex backend (Linux, modern Windows, etc.) reports accurate
/// state including reader counts for RwLock; macOS pthread handles
/// Mutex held/free but reports `Free` for RwLock; Win7 SRWLOCK
/// always reports `Free`.
fn assert_mutex_state(val: &Value, exp_state: LockState) {
    let Value::Specialized {
        value: Some(SpecializedValue::Mutex { state, .. }),
        ..
    } = val
    else {
        panic!(
            "not a Mutex/RwLock spec value: {:?}",
            val.r#type().name_fmt()
        );
    };
    assert_eq!(*state, exp_state);
}

/// Phase 1 S1 — `Mutex<T>` and `RwLock<T>` peel through their `data:
/// UnsafeCell<T>` field to surface the inner T directly.
#[test]
#[serial]
fn test_read_mutex_rwlock() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 749).unwrap();
    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(749));

    let vars = debugger.read_local_variables().unwrap();
    let pick = |needle: &str| {
        vars.iter()
            .find(|v| v.identity().to_string().contains(needle))
            .unwrap_or_else(|| panic!("`{needle}` not in locals"))
    };

    assert_mutex_inner(pick("mtx").value(), |inner| {
        assert_scalar(inner, "i32", Some(SupportedScalar::I32(123)));
    });
    assert_mutex_inner(pick("rwl").value(), |inner| {
        assert_scalar(inner, "i32", Some(SupportedScalar::I32(456)));
    });
    // Phase 1 S1 (poison): a freshly-constructed mutex/rwlock is
    // not poisoned. Future fixtures with deliberately-poisoned
    // locks would assert `true` here.
    assert_mutex_poisoned(pick("mtx").value(), false);
    assert_mutex_poisoned(pick("rwl").value(), false);
    // Phase 1 S1 (state): the fixture *does* hold both locks at
    // the breakpoint — `mtx.lock()` runs at vars.rs:734 and
    // `rwl.read()` at vars.rs:735, both before the bp at 749. The
    // probe behaviour splits by platform:
    //   * Linux / futex backend: Mutex⇒Exclusive, RwLock with one
    //     reader ⇒ Shared(1) — full decoding via libstd's MASK.
    //   * macOS pthread: Mutex⇒Exclusive (owner-field probe), but
    //     RwLock has no probe yet (TODO) so reports Free.
    //   * Win7 SRWLOCK: always Free (no probe).
    assert_mutex_state(pick("mtx").value(), LockState::Exclusive);
    #[cfg(not(target_os = "macos"))]
    assert_mutex_state(pick("rwl").value(), LockState::Shared(1));
    #[cfg(target_os = "macos")]
    assert_mutex_state(pick("rwl").value(), LockState::Free);

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

/// Phase 1 S10 helper: assert a MaybeUninit peeled to its inner T.
fn assert_maybe_uninit_inner(val: &Value, with_inner: impl FnOnce(&Value)) {
    let Value::Specialized {
        value: Some(SpecializedValue::MaybeUninit(inner)),
        ..
    } = val
    else {
        panic!(
            "not a MaybeUninit spec value: {:?}",
            val.r#type().name_fmt()
        );
    };
    with_inner(inner.as_ref());
}

/// Phase 1 S10 — `MaybeUninit<T>` peels through the union's `value`
/// arm and `ManuallyDrop` wrapper to surface the inner T directly.
#[test]
#[serial]
fn test_read_maybe_uninit() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 749).unwrap();
    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(749));

    let vars = debugger.read_local_variables().unwrap();
    let pick = |needle: &str| {
        vars.iter()
            .find(|v| v.identity().to_string().contains(needle))
            .unwrap_or_else(|| panic!("`{needle}` not in locals"))
    };

    assert_maybe_uninit_inner(pick("mu_init").value(), |inner| {
        assert_scalar(inner, "i32", Some(SupportedScalar::I32(99)));
    });

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

/// Phase 1 S12/S13/S14 DST companions — `&CStr`, `&OsStr`, `&Path`
/// route through the same parsers as their owned counterparts.
#[test]
#[serial]
fn test_read_dst_refs() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 749).unwrap();
    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(749));

    let vars = debugger.read_local_variables().unwrap();
    let pick = |needle: &str| {
        vars.iter()
            .find(|v| v.identity().to_string().contains(needle))
            .unwrap_or_else(|| panic!("`{needle}` not in locals"))
    };

    let dst_cs = pick("dst_cs").value();
    let Value::Specialized {
        value: Some(SpecializedValue::CString(s)),
        ..
    } = dst_cs
    else {
        panic!(
            "dst_cs not a CString-spec value: {:?}",
            dst_cs.r#type().name_fmt()
        );
    };
    assert_eq!(s.value, "c\"hi\"");

    let dst_os = pick("dst_os").value();
    let Value::Specialized {
        value: Some(SpecializedValue::OsString(s)),
        ..
    } = dst_os
    else {
        panic!(
            "dst_os not an OsString-spec value: {:?}",
            dst_os.r#type().name_fmt()
        );
    };
    assert_eq!(s.value, "\"hi\"");

    let dst_pa = pick("dst_pa").value();
    let Value::Specialized {
        value: Some(SpecializedValue::OsString(s)),
        ..
    } = dst_pa
    else {
        panic!(
            "dst_pa not an OsString-spec value: {:?}",
            dst_pa.r#type().name_fmt()
        );
    };
    assert_eq!(s.value, "\"/etc\"");

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

/// Phase 1 S13/S14 helper: assert an OsString/PathBuf rendered to a
/// particular pre-rendered string form.
fn assert_os_string(val: &Value, exp_outer_type_contains: &str, exp_text: &str) {
    let Value::Specialized {
        value: Some(SpecializedValue::OsString(s)),
        ..
    } = val
    else {
        panic!("not an OsString spec value: {:?}", val.r#type().name_fmt());
    };
    let actual_type = val.r#type().name_fmt();
    assert!(
        actual_type.contains(exp_outer_type_contains),
        "expected type to contain {exp_outer_type_contains:?}, got {actual_type:?}"
    );
    assert_eq!(s.value, exp_text);
}

/// Phase 1 S13/S14 — `OsString` and `PathBuf` peel through their
/// wrapper chain to the underlying `Vec<u8>` and render as a quoted
/// utf-8 string (or hex preview when not utf-8).
#[test]
#[serial]
fn test_read_os_string_pathbuf() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 749).unwrap();
    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(749));

    let vars = debugger.read_local_variables().unwrap();
    let pick = |needle: &str| {
        vars.iter()
            .find(|v| v.identity().to_string().contains(needle))
            .unwrap_or_else(|| panic!("`{needle}` not in locals"))
    };

    assert_os_string(pick("os_str").value(), "OsString", "\"hello\"");
    assert_os_string(pick("pb").value(), "PathBuf", "\"/tmp/foo\"");

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

/// Phase 1 S12 helper: assert a CString rendered to a particular
/// pre-rendered string form.
fn assert_cstring(val: &Value, exp_outer_type_contains: &str, exp_text: &str) {
    let Value::Specialized {
        value: Some(SpecializedValue::CString(s)),
        ..
    } = val
    else {
        panic!("not a CString spec value: {:?}", val.r#type().name_fmt());
    };
    let actual_type = val.r#type().name_fmt();
    assert!(
        actual_type.contains(exp_outer_type_contains),
        "expected type to contain {exp_outer_type_contains:?}, got {actual_type:?}"
    );
    assert_eq!(s.value, exp_text);
}

/// Phase 1 S12 — `CString` renders as `c"…"` (utf-8) or `c"\\xNN…"`
/// hex preview when not valid utf-8.
#[test]
#[serial]
fn test_read_cstring() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 749).unwrap();
    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(749));

    let vars = debugger.read_local_variables().unwrap();
    let pick = |needle: &str| {
        vars.iter()
            .find(|v| v.identity().to_string().contains(needle))
            .unwrap_or_else(|| panic!("`{needle}` not in locals"))
    };

    assert_cstring(pick("cs_hello").value(), "CString", "c\"hello\"");
    assert_cstring(pick("cs_empty").value(), "CString", "c\"\"");
    // 0x68 0x69 0x80 0xff — the 0x80 0xff bytes break utf-8, expect
    // hex preview that begins with the literal escape pattern.
    let cs_bytes = pick("cs_bytes").value();
    let Value::Specialized {
        value: Some(SpecializedValue::CString(s)),
        ..
    } = cs_bytes
    else {
        panic!("cs_bytes not a CString")
    };
    assert!(
        s.value.starts_with("c\"\\x"),
        "expected hex-preview prefix, got {:?}",
        s.value
    );

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

/// Phase 1 S4 helper: assert a `Duration` peeled to (secs, nanos).
fn assert_duration(val: &Value, exp: (u64, u32)) {
    let Value::Specialized {
        value: Some(SpecializedValue::Duration(got)),
        ..
    } = val
    else {
        panic!("not a Duration spec value: {:?}", val.r#type().name_fmt());
    };
    assert_eq!(*got, exp);
}

/// Phase 1 S4 — `core::time::Duration` peels to `(secs, nanos)` and
/// renders human-readable via `format_duration`.
#[test]
#[serial]
fn test_read_duration() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 749).unwrap();
    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(749));

    let vars = debugger.read_local_variables().unwrap();
    let pick = |needle: &str| {
        vars.iter()
            .find(|v| v.identity().to_string().contains(needle))
            .unwrap_or_else(|| panic!("`{needle}` not in locals"))
    };

    assert_duration(pick("d_zero").value(), (0, 0));
    assert_duration(pick("d_ms").value(), (1, 500_000_000));
    assert_duration(pick("d_s").value(), (7, 0));
    assert_duration(pick("d_h").value(), (3661, 500_000_000));

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

/// Phase 1 S6 — `core::ops::Range*` family renders to canonical
/// Rust source form (`a..b`, `a..=b`, `a..`, `..b`).
#[test]
#[serial]
fn test_read_ranges() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 749).unwrap();
    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(749));

    let vars = debugger.read_local_variables().unwrap();
    let pick = |needle: &str| {
        vars.iter()
            .find(|v| v.identity().to_string().contains(needle))
            .unwrap_or_else(|| panic!("`{needle}` not in locals"))
    };

    assert_range_text(pick("r1").value(), "Range<i32>", "0..10");
    assert_range_text(pick("r2").value(), "RangeInclusive<i32>", "0..=10");
    assert_range_text(pick("r3").value(), "RangeFrom<i32>", "5..");
    assert_range_text(pick("r4").value(), "RangeTo<i32>", "..10");

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

/// Phase 1 S7 — `Pin<P>` peels to the pinnee.
#[test]
#[serial]
fn test_read_pin() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    // Line of `let nop: Option<u8> = None;` inside `phase1_specs_b` —
    // see `examples/vars/src/vars.rs`. If you renumber that function,
    // update this constant in lockstep.
    debugger.set_breakpoint_at_line("vars.rs", 749).unwrap();
    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(749));

    let vars = debugger.read_local_variables().unwrap();
    let pinned_box = vars
        .iter()
        .find(|v| v.identity().to_string().contains("pinned_box"))
        .expect("pinned_box not in locals");
    let pinned_ref = vars
        .iter()
        .find(|v| v.identity().to_string().contains("pinned_ref"))
        .expect("pinned_ref not in locals");

    // Pin<Box<i32>> peels to a Box (still a pointer-shaped Value).
    assert_pin(
        pinned_box.value(),
        "Pin<alloc::boxed::Box<i32, alloc::alloc::Global>>",
        |inner| {
            assert!(
                matches!(inner, Value::Pointer(_)),
                "pinned_box pinnee should be a pointer; got {:?}",
                inner.r#type().name_fmt()
            );
        },
    );

    // Pin<&mut i32> peels to a &mut i32 reference (also pointer-shaped).
    assert_pin(pinned_ref.value(), "Pin<&mut i32>", |inner| {
        assert!(
            matches!(inner, Value::Pointer(_)),
            "pinned_ref pinnee should be a pointer; got {:?}",
            inner.r#type().name_fmt()
        );
    });

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

/// Phase 1 S11 — `NonNull<T>` is rendered as the bare inner pointer.
#[test]
#[serial]
fn test_read_nonnull() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    // Line of `let nop: Option<u8> = None;` inside `phase1_specs` —
    // see `examples/vars/src/vars.rs`. If you renumber that function,
    // update this constant in lockstep.
    debugger.set_breakpoint_at_line("vars.rs", 698).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(698));

    let vars = debugger.read_local_variables().unwrap();
    let nn = vars
        .iter()
        .find(|v| v.identity().to_string().contains("nn"))
        .unwrap_or_else(|| {
            panic!(
                "`nn` not in {:?}",
                vars.iter()
                    .map(|v| v.identity().to_string())
                    .collect::<Vec<_>>()
            )
        });

    assert_nonnull_pointer(nn.value(), "NonNull<i32>", "*const i32");

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

/// Phase 3 Feature C — `Rc<RefCell<Node>>` cycle detection. The
/// renderer must terminate gracefully on a 2-node cycle (no stack
/// overflow) and emit a `[cycle to 0x…]` marker on the second
/// visit. Deep but acyclic chains hit the depth cap with a
/// `[depth limit 64]` marker instead.
#[test]
#[serial]
fn test_rc_cycle_detection() {
    use bugstalker::ui::generic::variable::render_value;

    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    // Line of `let nop: Option<u8> = None;` inside `phase3_rc_cycle`
    // — keep in lockstep with vars.rs.
    debugger.set_breakpoint_at_line("vars.rs", 915).unwrap();
    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(915));

    let vars = debugger.read_local_variables().unwrap();

    let cycle_root = vars
        .iter()
        .find(|v| v.identity().to_string().contains("cycle_root"))
        .expect("cycle_root not in locals");
    let deep = vars
        .iter()
        .find(|v| v.identity().to_string().contains("deep"))
        .expect("deep not in locals");

    // Render must not stack-overflow. If we get here, that's
    // already half the value of Feature C.
    let cycle_str = render_value(cycle_root.value());
    let deep_str = render_value(deep.value());

    eprintln!("[cycle] cycle_root rendered as:\n{cycle_str}\n");
    eprintln!(
        "[cycle] deep (truncated to 200 chars): {}",
        &deep_str[..deep_str.len().min(200)]
    );

    assert!(
        cycle_str.contains("cycle to"),
        "cycle_root missing cycle marker: {cycle_str:?}"
    );
    // Acyclic deep chain should hit the depth cap.
    assert!(
        deep_str.contains("depth limit"),
        "deep chain missing depth-cap marker: {deep_str:?}"
    );

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

/// Phase 3 Feature B — niche-encoded `Option<T>` resolution.
/// Verifies that for every niche pattern (`Option<&T>`,
/// `Option<Box<T>>`, `Option<NonNull<T>>`, `Option<NonZero*>`,
/// `Option<bool>`, `Option<fn(…)>`), the renderer correctly picks
/// `Some(…)` vs `None` from the underlying bytes — no
/// `RUST$ENCODED$ENUM$` fallback, no DWARF-discriminant guessing.
#[test]
#[serial]
fn test_niche_option_recovery() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    // Line of `let nop: Option<u8> = None;` inside
    // `phase3_niche_options` — keep in lockstep with vars.rs.
    debugger.set_breakpoint_at_line("vars.rs", 864).unwrap();
    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(864));

    let vars = debugger.read_local_variables().unwrap();

    let pick_variant = |name: &str| -> String {
        let v = vars
            .iter()
            .find(|v| v.identity().to_string().contains(name))
            .unwrap_or_else(|| panic!("{name} not in locals"));
        match v.value() {
            Value::RustEnum(re) => re
                .value
                .as_ref()
                .map(|m| {
                    m.field_name
                        .clone()
                        .unwrap_or_else(|| String::from("<anonymous>"))
                })
                .unwrap_or_else(|| String::from("<no variant>")),
            other => panic!(
                "{name}: not a RustEnum, got {:?}",
                other.r#type().name_fmt()
            ),
        }
    };

    let cases: &[(&str, &str)] = &[
        ("opt_ref_some", "Some"),
        ("opt_ref_none", "None"),
        ("opt_box_some", "Some"),
        ("opt_box_none", "None"),
        ("opt_nn_some", "Some"),
        ("opt_nn_none", "None"),
        ("opt_nz_some", "Some"),
        ("opt_nz_none", "None"),
        ("opt_bool_some", "Some"),
        ("opt_bool_none", "None"),
        ("opt_fn_some", "Some"),
        ("opt_fn_none", "None"),
        // Result<T, ZST>: niche of T doubles as the discriminant
        // for the ZST error arm.
        ("res_ok", "Ok"),
        ("res_err", "Err"),
        ("res_nz_ok", "Ok"),
        ("res_nz_err", "Err"),
    ];
    let mut failures: Vec<String> = Vec::new();
    for (name, expected) in cases {
        let got = pick_variant(name);
        eprintln!("[niche] {name:14} → {got}");
        if got != *expected {
            failures.push(format!("  {name}: expected {expected}, got {got}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} niche misclassifications:\n{}",
        failures.len(),
        failures.join("\n")
    );

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

/// Phase 3 Feature A — `dyn Trait` fat-pointer detection. Verifies
/// the renderer recognises trait objects (`Box<dyn Error>`,
/// `&dyn Iterator<Item = u32>`, `Arc<dyn Debug + Send + Sync>`)
/// and emits the `[concrete type unavailable; vtable resolution
/// pending — Phase 3A follow-up]` annotation. Concrete type recovery
/// itself lands in a follow-up batch; this test guards the
/// detection layer.
///
/// **macOS 26.4.1 (`xnu-12377.101.15`) — DISABLED ON DARWIN PENDING APPLE FIX.**
/// Two reproducible kernel panics observed on this kernel
/// (2026-05-14 20:09 and 20:20, `/Library/Logs/DiagnosticReports/
/// panic-full-2026-05-14-{200917,202052}.0002.panic`) with byte-
/// identical fingerprint: panicked task = this cargo-test binary,
/// 19 threads, PC = kernel_text_exec_base + 0x66970, caller =
/// +0x956338, ESR=0x96000007 (data abort level-3) with FAR landing
/// inside the kernel Zone Metadata range — a zone-allocator UAF/race
/// in xnu, tripped by the bs darwin harness (mach_vm_read_overwrite /
/// mach_vm_protect / task_for_pid across multiple worker threads).
/// Apple Feedback Assistant report filed 2026-05-15.
///
/// Re-enable when one of:
///   - macOS ships a kernel build past `xnu-12377.101.15`/`25.4.0`
///     and the panic no longer reproduces on a single rerun of this
///     test, OR
///   - `src/debugger/darwin_mach.rs` grows a process-wide serialising
///     mutex around every `mach_vm_*` and `task_for_pid` call so
///     concurrent worker threads can't race the kernel zone code.
/// To undo: delete the `cfg(not(target_os = "macos"))` line below
/// and grep this file for "26.4.1" to find the banner.
#[cfg(not(target_os = "macos"))]
#[test]
#[serial]
fn test_dyn_trait_detection() {
    // Phase 3 Feature A batch A8 — `value_layout()` for trait
    // objects used to return `PreRendered(<multi-line summary>)`.
    // That summary is now built by the ui renderer
    // (`render_value`) because it needs the depth context that
    // `value_layout` can't carry. So this test now drives the
    // public renderer instead of inspecting the layout enum.
    use bugstalker::ui::generic::variable::render_value;

    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    // Line of `let nop: Option<u8> = None;` inside
    // `phase3_dyn_trait` — keep in lockstep with vars.rs.
    debugger.set_breakpoint_at_line("vars.rs", 796).unwrap();
    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(796));

    let vars = debugger.read_local_variables().unwrap();

    let pick = |name: &str| -> String {
        let v = vars
            .iter()
            .find(|v| v.identity().to_string().contains(name))
            .unwrap_or_else(|| panic!("{name} not in locals"));
        render_value(v.value())
    };

    // `Box<dyn Error>` renders as the wrapping struct's two-pointer
    // layout — that's the case our detector catches today.
    let boxed_err = pick("boxed_err");
    eprintln!("[dyn-trait] boxed_err rendered as: {boxed_err}");
    assert!(
        boxed_err.contains("dyn") && boxed_err.contains("vtable"),
        "boxed_err missing dyn / vtable annotation: {boxed_err:?}"
    );
    // Phase 3A batch A2 — vtable resolution should now fire on a
    // standard rustc build; the trait-object summary carries the
    // recovered concrete type as `… [→ Concrete]`.
    assert!(
        boxed_err.contains("→") && boxed_err.contains("MyError"),
        "boxed_err missing concrete-type recovery (`[→ MyError]`): {boxed_err:?}"
    );
    // `Arc<dyn Debug + Send + Sync>` and `&dyn Iterator<…>` route
    // through the smart-pointer / reference-deref paths
    // respectively; their detection lands in a follow-up batch
    // alongside vtable resolution.

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_cell() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 453).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(453));

    read_locals!(debugger => a_cell, b_refcell, _b_refcell_borrow_1, _b_refcell_borrow_2);
    assert_idents!(a_cell => "a_cell", b_refcell => "b_refcell");

    assert_cell(a_cell.value(), "Cell<i32>", |value| {
        assert_scalar(value, "i32", Some(SupportedScalar::I32(1)))
    });

    assert_refcell(
        b_refcell.value(),
        "RefCell<alloc::vec::Vec<i32, alloc::alloc::Global>>",
        2,
        |value| {
            assert_vec(value, "Vec<i32, alloc::alloc::Global>", 3, |buf| {
                assert_array(buf, "[i32]", |i, item| match i {
                    0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(1))),
                    1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(2))),
                    2 => assert_scalar(item, "i32", Some(SupportedScalar::I32(3))),
                    _ => panic!("3 items expected"),
                })
            })
        },
    );

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_shared_ptr() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();
    let rust_version = rust_version(VARS_APP).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 475).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(475));

    read_locals!(debugger => rc0, rc1, weak_rc2, arc0, arc1, weak_arc2);
    assert_idents!(
        rc0 => "rc0", rc1 => "rc1", weak_rc2 => "weak_rc2", arc0 => "arc0", arc1 => "arc1", weak_arc2 => "weak_arc2"
    );

    assert_rc(rc0.value(), "Rc<i32, alloc::alloc::Global>");
    let deref = rc0.clone().modify_value(|pcx, v| v.deref(pcx));
    let deref_type = version_switch!(
        rust_version,
        .. (1 . 84) => "RcBox<i32>",
        (1 . 84) .. => "RcInner<i32>",
    )
    .unwrap();
    assert_struct(deref.unwrap().value(), deref_type, |i, member| match i {
        0 => assert_member(member, "strong", |val| {
            assert_cell(val, "Cell<usize>", |inner| {
                assert_scalar(inner, "usize", Some(SupportedScalar::Usize(2)))
            })
        }),
        1 => assert_member(member, "weak", |val| {
            assert_cell(val, "Cell<usize>", |inner| {
                assert_scalar(inner, "usize", Some(SupportedScalar::Usize(2)))
            })
        }),
        2 => assert_member(member, "value", |val| {
            assert_scalar(val, "i32", Some(SupportedScalar::I32(1)))
        }),
        _ => panic!("3 members expected"),
    });

    assert_rc(rc1.value(), "Rc<i32, alloc::alloc::Global>");
    let deref = rc1.clone().modify_value(|pcx, v| v.deref(pcx));
    assert_struct(deref.unwrap().value(), deref_type, |i, member| match i {
        0 => assert_member(member, "strong", |val| {
            assert_cell(val, "Cell<usize>", |inner| {
                assert_scalar(inner, "usize", Some(SupportedScalar::Usize(2)))
            })
        }),
        1 => assert_member(member, "weak", |val| {
            assert_cell(val, "Cell<usize>", |inner| {
                assert_scalar(inner, "usize", Some(SupportedScalar::Usize(2)))
            })
        }),
        2 => assert_member(member, "value", |val| {
            assert_scalar(val, "i32", Some(SupportedScalar::I32(1)))
        }),
        _ => panic!("3 members expected"),
    });

    // Phase 1 S15: Weak now reports strong/weak counts.
    assert_weak(
        weak_rc2.value(),
        "Weak<i32, alloc::alloc::Global>",
        2, // strong
        2, // weak (rc1 + weak_rc2)
    );
    let deref = weak_rc2.clone().modify_value(|pcx, v| v.deref(pcx));
    assert_struct(deref.unwrap().value(), deref_type, |i, member| match i {
        0 => assert_member(member, "strong", |val| {
            assert_cell(val, "Cell<usize>", |inner| {
                assert_scalar(inner, "usize", Some(SupportedScalar::Usize(2)))
            })
        }),
        1 => assert_member(member, "weak", |val| {
            assert_cell(val, "Cell<usize>", |inner| {
                assert_scalar(inner, "usize", Some(SupportedScalar::Usize(2)))
            })
        }),
        2 => assert_member(member, "value", |val| {
            assert_scalar(val, "i32", Some(SupportedScalar::I32(1)))
        }),
        _ => panic!("3 members expected"),
    });

    assert_arc(arc0.value(), "Arc<i32, alloc::alloc::Global>");
    let deref = arc0.clone().modify_value(|pcx, v| v.deref(pcx));
    assert_struct(
        deref.unwrap().value(),
        "ArcInner<i32>",
        |i, member| match i {
            // Phase 1 S3: AtomicUsize is now a peeled scalar.
            0 => assert_member(member, "strong", |val| {
                assert_atomic_scalar(val, "AtomicUsize", "usize", SupportedScalar::Usize(2))
            }),
            1 => assert_member(member, "weak", |val| {
                assert_atomic_scalar(val, "AtomicUsize", "usize", SupportedScalar::Usize(2))
            }),
            2 => assert_member(member, "data", |val| {
                assert_scalar(val, "i32", Some(SupportedScalar::I32(2)))
            }),
            _ => panic!("3 members expected"),
        },
    );

    assert_arc(arc1.value(), "Arc<i32, alloc::alloc::Global>");
    let deref = arc1.clone().modify_value(|pcx, v| v.deref(pcx));
    assert_struct(
        deref.unwrap().value(),
        "ArcInner<i32>",
        |i, member| match i {
            // Phase 1 S3: AtomicUsize is now a peeled scalar.
            0 => assert_member(member, "strong", |val| {
                assert_atomic_scalar(val, "AtomicUsize", "usize", SupportedScalar::Usize(2))
            }),
            1 => assert_member(member, "weak", |val| {
                assert_atomic_scalar(val, "AtomicUsize", "usize", SupportedScalar::Usize(2))
            }),
            2 => assert_member(member, "data", |val| {
                assert_scalar(val, "i32", Some(SupportedScalar::I32(2)))
            }),
            _ => panic!("3 members expected"),
        },
    );

    // Phase 1 S15: Weak (sync flavour) reports strong/weak counts.
    assert_weak(
        weak_arc2.value(),
        "Weak<i32, alloc::alloc::Global>",
        2, // strong
        2, // weak (arc1 + weak_arc2)
    );
    let deref = weak_arc2
        .clone()
        .modify_value(|pcx, v| v.deref(pcx))
        .unwrap();
    assert_struct(deref.value(), "ArcInner<i32>", |i, member| match i {
        // Phase 1 S3: AtomicUsize is now a peeled scalar.
        0 => assert_member(member, "strong", |val| {
            assert_atomic_scalar(val, "AtomicUsize", "usize", SupportedScalar::Usize(2))
        }),
        1 => assert_member(member, "weak", |val| {
            assert_atomic_scalar(val, "AtomicUsize", "usize", SupportedScalar::Usize(2))
        }),
        2 => assert_member(member, "data", |val| {
            assert_scalar(val, "i32", Some(SupportedScalar::I32(2)))
        }),
        _ => panic!("3 members expected"),
    });

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_zst_types() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 496).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(496));

    read_locals!(
        debugger => ptr_zst, array_zst, vec_zst, slice_zst, struct_zst, enum_zst, vecdeque_zst,
        hash_map_zst_key, hash_map_zst_val, hash_map_zst, hash_set_zst, btree_map_zst_key,
        btree_map_zst_val, btree_map_zst, btree_set_zst
    );
    assert_idents!(
        ptr_zst => "ptr_zst", array_zst => "array_zst", vec_zst => "vec_zst",
        slice_zst => "slice_zst", struct_zst => "struct_zst", enum_zst => "enum_zst",
        vecdeque_zst => "vecdeque_zst", hash_map_zst_key => "hash_map_zst_key",
        hash_map_zst_val => "hash_map_zst_val", hash_map_zst => "hash_map_zst",
        hash_set_zst => "hash_set_zst", btree_map_zst_key => "btree_map_zst_key",
        btree_map_zst_val => "btree_map_zst_val", btree_map_zst => "btree_map_zst",
        btree_set_zst => "btree_set_zst"
    );

    assert_pointer(ptr_zst.value(), "&()");
    let deref = ptr_zst.clone().modify_value(|pcx, v| v.deref(pcx));
    assert_scalar(deref.unwrap().value(), "()", Some(SupportedScalar::Empty()));

    assert_array(array_zst.value(), "[()]", |i, item| match i {
        0 => assert_scalar(item, "()", Some(SupportedScalar::Empty())),
        1 => assert_scalar(item, "()", Some(SupportedScalar::Empty())),
        _ => panic!("2 members expected"),
    });

    assert_vec(vec_zst.value(), "Vec<(), alloc::alloc::Global>", 0, |buf| {
        assert_array(buf, "[()]", |i, item| match i {
            0 => assert_scalar(item, "()", Some(SupportedScalar::Empty())),
            1 => assert_scalar(item, "()", Some(SupportedScalar::Empty())),
            2 => assert_scalar(item, "()", Some(SupportedScalar::Empty())),
            _ => panic!("3 members expected"),
        })
    });

    assert_pointer(slice_zst.value(), "&[(); 4]");
    let deref = slice_zst.clone().modify_value(|pcx, v| v.deref(pcx));
    assert_array(deref.unwrap().value(), "[()]", |i, item| match i {
        0 => assert_scalar(item, "()", Some(SupportedScalar::Empty())),
        1 => assert_scalar(item, "()", Some(SupportedScalar::Empty())),
        2 => assert_scalar(item, "()", Some(SupportedScalar::Empty())),
        3 => assert_scalar(item, "()", Some(SupportedScalar::Empty())),
        _ => panic!("4 members expected"),
    });

    assert_struct(struct_zst.value(), "StructZst", |i, member| match i {
        0 => assert_member(member, "__0", |val| {
            assert_scalar(val, "()", Some(SupportedScalar::Empty()))
        }),
        _ => panic!("1 member expected"),
    });

    assert_rust_enum(enum_zst.value(), "Option<()>", |member| {
        assert_struct(member, "Some", |i, member| match i {
            0 => assert_member(member, "__0", |val| {
                assert_scalar(val, "()", Some(SupportedScalar::Empty()))
            }),
            _ => panic!("1 member expected"),
        })
    });

    assert_vec_deque(
        vecdeque_zst.value(),
        "VecDeque<(), alloc::alloc::Global>",
        0,
        |buf| {
            assert_array(buf, "[()]", |i, item| match i {
                0 => assert_scalar(item, "()", Some(SupportedScalar::Empty())),
                1 => assert_scalar(item, "()", Some(SupportedScalar::Empty())),
                2 => assert_scalar(item, "()", Some(SupportedScalar::Empty())),
                3 => assert_scalar(item, "()", Some(SupportedScalar::Empty())),
                4 => assert_scalar(item, "()", Some(SupportedScalar::Empty())),
                _ => panic!("5 members expected"),
            })
        },
    );

    let rust_version = rust_version(VARS_APP).unwrap();
    let hashmap_type = version_switch!(
            rust_version,
            .. (1 . 76) => "HashMap<(), i32, std::collections::hash::map::RandomState>",
            (1 . 76) .. (1 . 94) => "HashMap<(), i32, std::hash::random::RandomState>",
            (1 . 94) .. => "HashMap<(), i32, std::hash::random::RandomState, alloc::alloc::Global>",
    )
    .unwrap();
    assert_hashmap(hash_map_zst_key.value(), hashmap_type, |items| {
        assert_eq!(items.len(), 1);
        assert_scalar(&items[0].0, "()", Some(SupportedScalar::Empty()));
        assert_scalar(&items[0].1, "i32", Some(SupportedScalar::I32(1)));
    });

    let hashmap_type = version_switch!(
            rust_version,
            .. (1 . 76) => "HashMap<i32, (), std::collections::hash::map::RandomState>",
            (1 . 76) .. (1 . 94) => "HashMap<i32, (), std::hash::random::RandomState>",
            (1 . 94) .. => "HashMap<i32, (), std::hash::random::RandomState, alloc::alloc::Global>",
    )
    .unwrap();
    assert_hashmap(hash_map_zst_val.value(), hashmap_type, |items| {
        assert_eq!(items.len(), 1);
        assert_scalar(&items[0].0, "i32", Some(SupportedScalar::I32(1)));
        assert_scalar(&items[0].1, "()", Some(SupportedScalar::Empty()));
    });

    let hashmap_type = version_switch!(
            rust_version,
            .. (1 . 76) => "HashMap<(), (), std::collections::hash::map::RandomState>",
            (1 . 76) .. (1 . 94) => "HashMap<(), (), std::hash::random::RandomState>",
            (1 . 94) .. => "HashMap<(), (), std::hash::random::RandomState, alloc::alloc::Global>",
    )
    .unwrap();
    assert_hashmap(hash_map_zst.value(), hashmap_type, |items| {
        assert_eq!(items.len(), 1);
        assert_scalar(&items[0].0, "()", Some(SupportedScalar::Empty()));
        assert_scalar(&items[0].1, "()", Some(SupportedScalar::Empty()));
    });

    let hashset_type = version_switch!(
            rust_version,
            .. (1 . 76) => "HashSet<(), std::collections::hash::map::RandomState>",
            (1 . 76) .. (1 . 94) => "HashSet<(), std::hash::random::RandomState>",
            (1 . 94) .. => "HashSet<(), std::hash::random::RandomState, alloc::alloc::Global>",
    )
    .unwrap();
    assert_hashset(hash_set_zst.value(), hashset_type, |items| {
        assert_eq!(items.len(), 1);
        assert_scalar(&items[0], "()", Some(SupportedScalar::Empty()));
    });

    assert_btree_map(
        btree_map_zst_key.value(),
        "BTreeMap<(), i32, alloc::alloc::Global>",
        |items| {
            assert_eq!(items.len(), 1);
            assert_scalar(&items[0].0, "()", Some(SupportedScalar::Empty()));
            assert_scalar(&items[0].1, "i32", Some(SupportedScalar::I32(1)));
        },
    );

    assert_btree_map(
        btree_map_zst_val.value(),
        "BTreeMap<i32, (), alloc::alloc::Global>",
        |items| {
            assert_eq!(items.len(), 2);
            assert_scalar(&items[0].0, "i32", Some(SupportedScalar::I32(1)));
            assert_scalar(&items[0].1, "()", Some(SupportedScalar::Empty()));
            assert_scalar(&items[1].0, "i32", Some(SupportedScalar::I32(2)));
            assert_scalar(&items[1].1, "()", Some(SupportedScalar::Empty()));
        },
    );

    assert_btree_map(
        btree_map_zst.value(),
        "BTreeMap<(), (), alloc::alloc::Global>",
        |items| {
            assert_eq!(items.len(), 1);
            assert_scalar(&items[0].0, "()", Some(SupportedScalar::Empty()));
            assert_scalar(&items[0].1, "()", Some(SupportedScalar::Empty()));
        },
    );

    assert_btree_set(
        btree_set_zst.value(),
        "BTreeSet<(), alloc::alloc::Global>",
        |items| {
            assert_eq!(items.len(), 1);
            assert_scalar(&items[0], "()", Some(SupportedScalar::Empty()));
        },
    );

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_read_static_in_fn_variable() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    // brkpt in function where static is declared
    debugger.set_breakpoint_at_line("vars.rs", 504).unwrap();
    // brkpt outside function where static is declared
    debugger.set_breakpoint_at_line("vars.rs", 678).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(504));

    read_var_dqe!(debugger, Dqe::Variable(Selector::by_name("INNER_STATIC", false)) => inner_static);
    assert_idents!(inner_static => "vars::inner_static::INNER_STATIC");
    assert_scalar(inner_static.value(), "u32", Some(SupportedScalar::U32(1)));

    debugger.continue_debugee().unwrap();
    assert_eq!(info.line.take(), Some(678));

    read_var_dqe!(debugger, Dqe::Variable(Selector::by_name("INNER_STATIC", false)) => inner_static);
    assert_idents!(inner_static => "vars::inner_static::INNER_STATIC");
    assert_scalar(inner_static.value(), "u32", Some(SupportedScalar::U32(1)));

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_slice_operator() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 61).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(61));

    read_var_dqe!(debugger, Dqe::Slice(
            Dqe::Variable(Selector::by_name("arr_1", true)).boxed(),
            None,
            None,
        ) => arr_1);
    assert_idents!(arr_1 => "arr_1");
    assert_array(arr_1.value(), "[i32]", |i, item| match i {
        0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(1))),
        1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(-1))),
        2 => assert_scalar(item, "i32", Some(SupportedScalar::I32(2))),
        3 => assert_scalar(item, "i32", Some(SupportedScalar::I32(-2))),
        4 => assert_scalar(item, "i32", Some(SupportedScalar::I32(3))),
        _ => panic!("5 items expected"),
    });

    read_var_dqe!(debugger, Dqe::Slice(
            Dqe::Variable(Selector::by_name("arr_1", true)).boxed(),
            Some(3),
            None,
        ) => arr_1);
    assert_idents!(arr_1 => "arr_1");
    assert_array(arr_1.value(), "[i32]", |i, item| match i {
        0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(-2))),
        1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(3))),
        _ => panic!("2 items expected"),
    });

    read_var_dqe!(debugger, Dqe::Slice(
            Dqe::Variable(Selector::by_name("arr_1", true)).boxed(),
            None,
            Some(2),
        ) => arr_1);
    assert_idents!(arr_1 => "arr_1");
    assert_array(arr_1.value(), "[i32]", |i, item| match i {
        0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(1))),
        1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(-1))),
        _ => panic!("2 items expected"),
    });

    read_var_dqe!(debugger, Dqe::Slice(
            Dqe::Variable(Selector::by_name("arr_1", true)).boxed(),
            Some(1),
            Some(4),
        ) => arr_1);
    assert_idents!(arr_1 => "arr_1");
    assert_array(arr_1.value(), "[i32]", |i, item| match i {
        0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(-1))),
        1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(2))),
        2 => assert_scalar(item, "i32", Some(SupportedScalar::I32(-2))),
        _ => panic!("3 items expected"),
    });

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_cast_pointers() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 119).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(119));

    read_locals!(debugger => a, ref_a, _ptr_a, _ptr_ptr_a, _b, _mut_ref_b, _c, _mut_ptr_c, _box_d, _f, _ref_f);

    assert_scalar(a.value(), "i32", Some(SupportedScalar::I32(2)));
    let Value::Pointer(pointer) = ref_a.value() else {
        panic!("expect a pointer");
    };

    let raw_ptr = pointer.value.unwrap();

    read_var_dqe!(debugger, Dqe::Deref(
            Dqe::PtrCast(PointerCast::new(raw_ptr as usize, "*const i32")).boxed(),
        ) => val);
    assert_scalar(val.value(), "i32", Some(SupportedScalar::I32(2)));

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_read_uuid() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 519).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(519));

    read_locals!(debugger => uuid_v4, uuid_v7);
    assert_idents!(uuid_v4 => "uuid_v4", uuid_v7 => "uuid_v7");
    assert_uuid(uuid_v4.value(), "Uuid");
    assert_uuid(uuid_v7.value(), "Uuid");

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_address_operator() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 119).unwrap();
    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(119));

    fn addr_of(name: &str, loc: bool) -> Dqe {
        Dqe::Address(Dqe::Variable(Selector::by_name(name, loc)).boxed())
    }
    fn addr_of_index(name: &str, index: i32) -> Dqe {
        Dqe::Address(
            Dqe::Index(
                Dqe::Variable(Selector::by_name(name, true)).boxed(),
                Literal::Int(index as i64),
            )
            .boxed(),
        )
    }
    fn addr_of_field(name: &str, field: &str) -> Dqe {
        Dqe::Address(
            Dqe::Field(
                Dqe::Variable(Selector::by_name(name, true)).boxed(),
                field.to_string(),
            )
            .boxed(),
        )
    }

    // get address of scalar variable and deref it
    let addr_a_dqe = addr_of("a", true);
    read_var_dqe!(debugger, addr_a_dqe.clone() => a);
    assert_pointer(a.value(), "&i32");
    read_var_dqe!(debugger, Dqe::Deref(addr_a_dqe.boxed()) => a);
    assert_scalar(a.value(), "i32", Some(SupportedScalar::I32(2)));

    read_var_dqe!(debugger, addr_of("ref_a", true) => addr_ptr_a);
    assert_pointer(addr_ptr_a.value(), "&&i32");
    read_var_dqe!(debugger, Dqe::Deref(
            Dqe::Deref(addr_of("ref_a", true).boxed()).boxed(),
        ) => a);
    assert_scalar(a.value(), "i32", Some(SupportedScalar::I32(2)));

    // get address of structure field and deref it
    read_var_dqe!(debugger, addr_of("f", true) => addr_f);
    assert_pointer(addr_f.value(), "&Foo");
    read_var_dqe!(debugger, Dqe::Deref(addr_of("f", true).boxed()) => f);
    assert_struct(f.value(), "Foo", |i, member| match i {
        0 => assert_member(member, "bar", |val| {
            assert_scalar(val, "i32", Some(SupportedScalar::I32(1)))
        }),
        1 => assert_member(member, "baz", |val| {
            assert_array(val, "[i32]", |i, item| match i {
                0 => assert_scalar(item, "i32", Some(SupportedScalar::I32(1))),
                1 => assert_scalar(item, "i32", Some(SupportedScalar::I32(2))),
                _ => panic!("2 items expected"),
            })
        }),
        2 => {
            assert_member(member, "foo", |val| assert_pointer(val, "&i32"));
            let member_val = member.value.clone();
            let deref = f.clone().modify_value(|pcx, _| member_val.deref(pcx));
            assert_scalar(deref.unwrap().value(), "i32", Some(SupportedScalar::I32(2)));
        }
        _ => panic!("3 members expected"),
    });

    read_var_dqe!(debugger, addr_of_field("f", "bar") => addr_f_bar);
    assert_pointer(addr_f_bar.value(), "&i32");
    read_var_dqe!(debugger, Dqe::Deref(addr_of_field("f", "bar").boxed()) => f_bar);
    assert_scalar(f_bar.value(), "i32", Some(SupportedScalar::I32(1)));

    // get address of an array element and deref it
    debugger.set_breakpoint_at_line("vars.rs", 151).unwrap();
    debugger.continue_debugee().unwrap();
    assert_eq!(info.line.take(), Some(151));

    read_var_dqe!(debugger, addr_of("vec1", true) => addr_vec1);
    assert_pointer(addr_vec1.value(), "&Vec<i32, alloc::alloc::Global>");
    read_var_dqe!(debugger, addr_of_index("vec1", 1) => addr_el_1);
    assert_pointer(addr_el_1.value(), "&i32");
    read_var_dqe!(debugger, Dqe::Deref(addr_of_index("vec1", 1).boxed()) => el_1);
    assert_scalar(el_1.value(), "i32", Some(SupportedScalar::I32(2)));

    // get an address of a hashmap element and deref it
    debugger.set_breakpoint_at_line("vars.rs", 290).unwrap();
    debugger.continue_debugee().unwrap();
    assert_eq!(info.line.take(), Some(290));

    read_var_dqe!(debugger, addr_of("hm3", true) => addr_hm3);
    let inner_hash_map_type = version_switch!(
            rust_version(VARS_APP).unwrap(),
            .. (1 . 76) => "&HashMap<i32, i32, std::collections::hash::map::RandomState>",
            (1 . 76) .. (1 . 94) => "&HashMap<i32, i32, std::hash::random::RandomState>",
            (1 . 94) .. => "&HashMap<i32, i32, std::hash::random::RandomState, alloc::alloc::Global>",
    )
    .unwrap();
    assert_pointer(addr_hm3.value(), inner_hash_map_type);

    read_var_dqe!(debugger, addr_of_index("hm3", 11) => addr_el_11);
    assert_pointer(addr_el_11.value(), "&i32");
    read_var_dqe!(debugger, Dqe::Deref(addr_of_index("hm3", 11).boxed()) => el_11);
    assert_scalar(el_11.value(), "i32", Some(SupportedScalar::I32(11)));

    // get address of global variable and deref it
    read_var_dqe!(debugger, addr_of("GLOB_1", false) => addr_glob_1);
    assert_pointer(addr_glob_1.value(), "&&str");
    read_var_dqe!(debugger, Dqe::Deref(addr_of("GLOB_1", false).boxed()) => glob_1);
    assert_str(glob_1.value(), "glob_1");

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_read_time() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 529).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(529));

    read_locals!(debugger => system_time, instant);
    assert_idents!(system_time => "system_time", instant => "instant");
    assert_system_time(system_time.value(), (0, 0));
    assert_instant(instant.value());

    // Phase 1 S5: SystemTime renders as ISO-8601 / RFC3339 UTC.
    // The debugee constructs `SystemTime::UNIX_EPOCH` so the format
    // is fixed across platforms: "1970-01-01T00:00:00Z".
    use bugstalker::debugger::variable::render::ValueLayout;
    let st_layout = system_time
        .value()
        .value_layout()
        .expect("SystemTime layout missing");
    match st_layout {
        ValueLayout::PreRendered(s) => {
            assert_eq!(s.as_ref(), "1970-01-01T00:00:00Z");
        }
        other => panic!("expected PreRendered ISO-8601, got {other:?}"),
    }

    // Phase 1 S5: Instant renders as `now ± HH:MM:SS.mmm`. The
    // direction sign and digit-shape are stable; the wall-clock
    // delta against `now` isn't, so we just check the prefix +
    // shape (`now ` + sign + 8-digit time + 4-digit fractional).
    let inst_layout = instant
        .value()
        .value_layout()
        .expect("Instant layout missing");
    match inst_layout {
        ValueLayout::PreRendered(s) => {
            let s = s.as_ref();
            assert!(
                s.starts_with("now + ") || s.starts_with("now - "),
                "Instant should render as `now ± …`, got {s:?}"
            );
            // After `now ± `, expect "HH:MM:SS.mmm" — 12 chars.
            let suffix = &s[6..];
            assert_eq!(
                suffix.len(),
                12,
                "Instant time format should be 12 chars (HH:MM:SS.mmm), got {suffix:?}"
            );
        }
        other => panic!("expected PreRendered Instant delta, got {other:?}"),
    }

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_debug_trait_repr_vars() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_line("vars.rs", 641).unwrap();

    debugger.start_debugee().unwrap();
    assert_eq!(info.line.take(), Some(641));

    read_locals!(debugger => v1, v2, v3, s0, s1, s2, s3, s4, str_array, c_enum, r_enum1, r_enum2, opt, my_str, my_string);
    let fmt_string = call_debug_fmt(&debugger, v1).unwrap();
    assert_eq!(fmt_string, "[]");
    let fmt_string = call_debug_fmt(&debugger, v2).unwrap();
    assert_eq!(fmt_string, "[]");
    let fmt_string = call_debug_fmt(&debugger, v3).unwrap();
    assert_eq!(fmt_string, "[1, 23, 3]");
    let fmt_string = call_debug_fmt(&debugger, s0).unwrap();
    assert_eq!(fmt_string, "Struct0 { a: 1 }");
    let fmt_string = call_debug_fmt(&debugger, s1).unwrap();
    assert_eq!(fmt_string, "Struct1 { field1: 1, field2: 3 }");
    let fmt_string = call_debug_fmt(&debugger, s2).unwrap();
    assert_eq!(fmt_string, "Struct1 { field1: 1, field2: \"44\" }");
    let fmt_string = call_debug_fmt(&debugger, s3).unwrap();
    assert_eq!(fmt_string, "Struct2 { field1: \"66\", field2: 55 }");
    let fmt_string = call_debug_fmt(&debugger, s4).unwrap();
    assert_eq!(fmt_string, "Struct3 { field1: 11, field2: 12 }");
    let fmt_string = call_debug_fmt(&debugger, str_array).unwrap();
    assert_eq!(fmt_string, "[\"abc\", \"ef\", \"g\"]");
    let fmt_string = call_debug_fmt(&debugger, c_enum).unwrap();
    assert_eq!(fmt_string, "A");
    let fmt_string = call_debug_fmt(&debugger, r_enum1).unwrap();
    assert_eq!(fmt_string, "S1(Struct1 { field1: 100, field2: \"100\" })");
    let fmt_string = call_debug_fmt(&debugger, r_enum2).unwrap();
    assert_eq!(fmt_string, "S2(Struct2 { field1: 1, field2: 2 })");
    let fmt_string = call_debug_fmt(&debugger, opt).unwrap();
    assert_eq!(fmt_string, "Some(1)");
    let fmt_string = call_debug_fmt(&debugger, my_str).unwrap();
    assert_eq!(fmt_string, "\"some str\"");
    let fmt_string = call_debug_fmt(&debugger, my_string).unwrap();
    assert_eq!(fmt_string, "\"some string\"");

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}

#[test]
#[serial]
fn test_debug_trait_repr_args() {
    let process = prepare_debugee_process(VARS_APP, &[]);
    let debugee_pid = process.pid();
    let info = TestInfo::default();
    let builder = DebuggerBuilder::new()
        .with_auto_traps(false)
        .with_hooks(TestHooks::new(info.clone()));
    let mut debugger = builder.build(process).unwrap();

    debugger.set_breakpoint_at_fn("debug_fmt_args").unwrap();

    debugger.start_debugee().unwrap();

    read_arg_dqe!(debugger, Dqe::Variable(Selector::Any) => arg1, arg2);
    let fmt_string = call_debug_fmt(&debugger, arg1).unwrap();
    assert_eq!(fmt_string, "\"one\"");
    let fmt_string = call_debug_fmt(&debugger, arg2).unwrap();
    assert_eq!(fmt_string, "[\"two\", \"three\"]");

    debugger.continue_debugee().unwrap();
    assert_no_proc!(debugee_pid);
}
