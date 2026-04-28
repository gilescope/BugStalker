#![allow(unused)]
#![allow(clippy::let_unit_value)]
#![allow(clippy::redundant_clone)]
#![allow(clippy::disallowed_names)]

fn scalar_types() {
    let int8 = 1_i8;
    let int16 = -1_i16;
    let int32 = 2_i32;
    let int64 = -2_i64;
    let int128 = 3_i128;
    let isize = -3_isize;

    let uint8 = 1_u8;
    let uint16 = 2_u16;
    let uint32 = 3_u32;
    let uint64 = 4_u64;
    let uint128 = 5_u128;
    let usize = 6_usize;

    let f32 = 1.1_f32;
    let f64 = 1.2_f64;

    let boolean_true = true;
    let boolean_false = false;

    let char_ascii = 'a';
    let char_non_ascii = '😊';

    let nop: Option<u8> = None;
}

fn compound_types() {
    let tuple_0 = ();
    let tuple_1 = (0f64, 1.1f64);
    let tuple_2 = (1u64, -1i64, 'a', false);

    struct Foo {
        bar: i32,
        baz: char,
    };
    let foo = Foo { bar: 100, baz: '9' };

    struct Foo2 {
        foo: Foo,
        additional: bool,
    };
    let foo2 = Foo2 {
        foo,
        additional: true,
    };

    let nop: Option<u8> = None;
}

fn array() {
    let arr_1 = [1, -1, 2, -2, 3];

    let arr_2 = [[1, -1, 2, -2, 3], [0, 1, 2, 3, 4], [0, -1, -2, -3, -4]];

    let nop: Option<u8> = None;
}

fn enums() {
    enum EnumA {
        A,
        B,
    }
    let enum_1 = EnumA::B;

    enum EnumC {
        C(char),
        D(f64, f32),
        E,
    }
    let enum_2 = EnumC::C('b');
    let enum_3 = EnumC::D(1.1, 1.2);
    let enum_4 = EnumC::E;

    struct Foo {
        a: i32,
        b: char,
    }
    enum EnumF {
        F(EnumC),
        G(Foo),
        J(EnumA),
    }
    let enum_5 = EnumF::F(EnumC::C('f'));
    let enum_6 = EnumF::G(Foo { a: 1, b: '1' });
    let enum_7 = EnumF::J(EnumA::A);

    let nop: Option<u8> = None;
}

fn references() {
    let a = 2;
    let ref_a = &a;
    let ptr_a: *const i32 = &a;
    let ptr_ptr_a: *const *const i32 = &ptr_a;
    let mut b = 2;
    let mut_ref_b = &mut b;
    let mut c = 2;
    let mut_ptr_c: *mut i32 = &mut b;
    let box_d = Box::new(2);

    struct Foo<'a> {
        bar: i32,
        baz: [i32; 2],
        foo: &'a i32,
    }
    let f = Foo {
        bar: 1,
        baz: [1, 2],
        foo: &a,
    };
    let ref_f = &f;

    let nop: Option<u8> = None;
}

fn type_alias() {
    type I32Alias = i32;
    let a_alias: I32Alias = 1;

    let nop: Option<u8> = None;
}

fn type_params() {
    struct Foo<T> {
        bar: T,
    };
    let a = Foo { bar: 1 };

    let nop: Option<u8> = None;
}

fn vec_and_slice_types() {
    let vec1 = vec![1, 2, 3];

    struct Foo {
        foo: i32,
    }
    let vec2 = vec![Foo { foo: 1 }, Foo { foo: 2 }];

    let vec3 = vec![vec1.clone(), vec1.clone()];

    let slice1 = &[1, 2, 3];
    let slice2 = &[slice1, slice1];

    let nop: Option<u8> = None;
}

fn string_types() {
    let s1 = "hello world".to_string();
    let s2 = s1.as_str();
    let s3 = "hello world";

    let nop: Option<u8> = None;
}

static GLOB_1: &str = "glob_1";
static GLOB_2: i32 = 2;

fn static_vars() {
    println!("{GLOB_1}");
    println!("{GLOB_2}");
    let nop: Option<u8> = None;
}

static GLOB_3: i32 = 3;
mod ns_1 {
    pub static GLOB_3: &str = "glob_3";
}

fn static_vars_same_name() {
    println!("{GLOB_3}");
    println!("{}", ns_1::GLOB_3);
    let nop: Option<u8> = None;
}

thread_local! {
    static THREAD_LOCAL_VAR_1: std::cell::Cell<i32> = std::cell::Cell::new(0);
    static THREAD_LOCAL_VAR_2: std::cell::Cell<&'static str> = std::cell::Cell::new("0");
}

fn thread_local() {
    THREAD_LOCAL_VAR_1.with(|tl1| tl1.set(1));
    THREAD_LOCAL_VAR_2.with(|tl2| tl2.set("1"));

    let t1 = std::thread::spawn(|| {
        THREAD_LOCAL_VAR_1.with(|tl1| tl1.set(2));
        THREAD_LOCAL_VAR_2.with(|tl2| tl2.set("2"));
        let nop: Option<u8> = None;
    });
    t1.join();

    let t2 = std::thread::spawn(|| {
        let nop: Option<u8> = None;
    });
    t2.join();

    let nop: Option<u8> = None;
}

fn fn_and_closure() {
    let inc = |a: i32| -> i32 { a + 1 };
    let inc_mut = |a: &mut i32| *a += 1;

    let outer = "outer val".to_string();
    let closure = move || println!("{outer}");

    let (a, b, c) = ("a".to_string(), "b".to_string(), "c".to_string());
    let trait_once: Box<dyn FnOnce()> = Box::new(move || println!("{a}"));
    let trait_mut: Box<dyn FnMut()> = Box::new(move || println!("{b}"));
    let trait_fn: Box<dyn Fn()> = Box::new(move || println!("{c}"));

    fn some_fn() -> u8 {
        1
    }
    let fn_ptr = some_fn;

    let nop: Option<u8> = None;
}

fn arguments(by_val: i32, by_ref: &i32, vec: Vec<u8>, box_arr: Box<[u8]>) {
    println!("{by_val}");
    println!("{by_ref}");
    println!("{vec:?}");
    println!("{box_arr:?}");

    let nop: Option<u8> = None;
}

fn unions() {
    #[repr(C)]
    union Union1 {
        f1: f32,
        u2: u64,
        u3: u8,
    }
    let union = Union1 { f1: 1.1 };

    let nop: Option<u8> = None;
}

fn hashmap() {
    use std::collections::HashMap;

    let hm1 = HashMap::from([(true, 3i64), (false, 5i64)]);
    let hm2 = HashMap::from([("abc", vec![1, 2, 3]), ("efg", vec![11, 12, 13])]);
    let mut hm3 = HashMap::new();
    for i in 0..100 {
        hm3.insert(i, i);
    }
    let hm4 = HashMap::from([
        ("1".to_string(), HashMap::from([(1, 1), (2, 2)])),
        ("3".to_string(), HashMap::from([(3, 3), (4, 4)])),
    ]);

    let a = &1;
    let b = &2;
    let hm5 = HashMap::from([(a, "a"), (b, "b")]);

    #[derive(Hash, PartialEq, Eq)]
    struct Index {
        field_1: i32,
        field_2: Vec<&'static str>,
        field_3: Option<bool>,
    }
    let hm6 = HashMap::from([
        (
            Index {
                field_1: 1,
                field_2: vec!["a", "b"],
                field_3: Some(true),
            },
            1,
        ),
        (
            Index {
                field_1: 2,
                field_2: vec!["c", "d", "e"],
                field_3: None,
            },
            2,
        ),
    ]);

    let nop: Option<u8> = None;
}

fn hashset() {
    use std::collections::HashSet;

    let hs1 = HashSet::from([1, 2, 3, 4]);
    let mut hs2 = HashSet::new();
    for i in 0..100 {
        hs2.insert(i);
    }
    let hs3 = HashSet::from([vec![1, 2]]);

    let a = &1;
    let b = &2;
    let hs4 = HashSet::from([a, b]);

    let nop: Option<u8> = None;
}

fn circular() {
    use std::cell::RefCell;
    use std::rc::Rc;

    enum List {
        Cons(i32, RefCell<Rc<List>>),
        Nil,
    }
    impl List {
        fn tail(&self) -> Option<&RefCell<Rc<List>>> {
            match self {
                List::Cons(_, item) => Some(item),
                List::Nil => None,
            }
        }
    }

    let a_circ = Rc::new(List::Cons(5, RefCell::new(Rc::new(List::Nil))));
    let b_circ = Rc::new(List::Cons(10, RefCell::new(Rc::clone(&a_circ))));

    if let Some(link) = a_circ.tail() {
        *link.borrow_mut() = Rc::clone(&b_circ);
    }

    let nop: Option<u8> = None;
}

fn lexical_blocks() {
    let alpha = 1;
    {
        let beta = 2;
        {
            let mut gama = 3;
            gama += 1;
        }
    }
    let mut delta = 4;
    delta += 1;

    let nop: Option<u8> = None;
}

fn btree_map() {
    use std::collections::BTreeMap;

    let hm1 = BTreeMap::from([(true, 3i64), (false, 5i64)]);
    let hm2 = BTreeMap::from([("abc", vec![1, 2, 3]), ("efg", vec![11, 12, 13])]);
    let mut hm3 = BTreeMap::new();
    for i in 0..100 {
        hm3.insert(i, i);
    }

    let hm4 = BTreeMap::from([
        ("1".to_string(), BTreeMap::from([(1, 1), (2, 2)])),
        ("3".to_string(), BTreeMap::from([(3, 3), (4, 4)])),
    ]);

    let a = &1;
    let b = &2;
    let hm5 = BTreeMap::from([(a, "a"), (b, "b")]);

    #[derive(Hash, PartialEq, Eq, PartialOrd, Ord)]
    struct Index {
        field_1: i32,
        field_2: Vec<&'static str>,
        field_3: Option<bool>,
    }
    let hm6 = BTreeMap::from([
        (
            Index {
                field_1: 1,
                field_2: vec!["a", "b"],
                field_3: Some(true),
            },
            1,
        ),
        (
            Index {
                field_1: 2,
                field_2: vec!["c", "d", "e"],
                field_3: None,
            },
            2,
        ),
    ]);

    let nop: Option<u8> = None;
}

fn btreeset() {
    use std::collections::BTreeSet;

    let hs1 = BTreeSet::from([1, 2, 3, 4]);
    let mut hs2 = BTreeSet::new();
    for i in 0..100 {
        hs2.insert(i);
    }
    let hs3 = BTreeSet::from([vec![1, 2]]);

    let a = &1;
    let b = &2;
    let hs4 = BTreeSet::from([a, b]);

    let nop: Option<u8> = None;
}

fn vecdeque() {
    use std::collections::VecDeque;

    let mut vd1 = VecDeque::new();
    vd1.push_back(0);
    vd1.push_back(1);
    vd1.push_back(2);
    vd1.push_front(10);
    vd1.push_front(9);

    let mut vd2 = VecDeque::new();
    vd2.push_back(VecDeque::from([1, 2, 3]));
    vd2.push_back(VecDeque::from([4, 5, 6]));
    vd2.push_front(VecDeque::from([-2, -1, 0]));

    let nop: Option<u8> = None;
}

fn atomics() {
    use std::sync::atomic;

    let int32_atomic = atomic::AtomicI32::new(1);
    let mut int32 = 2;
    let int32_atomic_ptr = atomic::AtomicPtr::new(&mut int32 as *mut i32);

    let nop: Option<u8> = None;
}

fn inner_mut() {
    use std::cell::Cell;
    use std::cell::RefCell;

    let a_cell = Cell::new(1);
    let b_refcell = RefCell::new(vec![1, 2, 3]);
    let b_refcell_borrow_1 = b_refcell.borrow();
    let b_refcell_borrow_2 = b_refcell.borrow();

    let nop: Option<u8> = None;
}

fn ptr_to_array() {
    let arr = [1, 2, 3, 4];
    let ptr = arr.as_ptr();

    let nop: Option<u8> = None;
}

fn shared_ptrs() {
    use std::rc::Rc;
    use std::sync::Arc;

    let rc0 = Rc::new(1);
    let rc1 = rc0.clone();
    let weak_rc2 = Rc::downgrade(&rc1);

    let arc0 = Arc::new(2);
    let arc1 = arc0.clone();
    let weak_arc2 = Arc::downgrade(&arc1);

    let nop: Option<u8> = None;
}

fn zst() {
    let ptr_zst = &();
    let array_zst = [(); 2];
    let vec_zst: Vec<()> = vec![(); 3];
    let slice_zst = &[(), (), (), ()];
    struct StructZst(());
    let struct_zst = StructZst(());
    let enum_zst = Option::Some(());
    let vecdeque_zst = std::collections::VecDeque::from(vec![(); 5]);
    let hash_map_zst_key = std::collections::HashMap::from([((), 1)]);
    let hash_map_zst_val = std::collections::HashMap::from([(1, ())]);
    let hash_map_zst = std::collections::HashMap::from([((), ())]);
    let hash_set_zst = std::collections::HashSet::from([(), (), ()]);
    let btree_map_zst_key = std::collections::BTreeMap::from([((), 1)]);
    let btree_map_zst_val = std::collections::BTreeMap::from([(1, ()), (2, ())]);
    let btree_map_zst = std::collections::BTreeMap::from([((), ())]);
    let btree_set_zst = std::collections::BTreeSet::from([(), (), ()]);

    let nop: Option<u8> = None;
}

fn inner_static() {
    static mut INNER_STATIC: u32 = 0;

    unsafe { INNER_STATIC += 1 };

    let nop: Option<u8> = None;
}

fn shadowing() {
    let var1 = 0_i32;
    let var1 = 1_i32;
    let var1 = "some str";

    let nop: Option<u8> = None;
}

fn uuid() {
    let uuid_v4 = uuid::Uuid::new_v4();
    let uuid_v7 = uuid::Uuid::new_v7(uuid::timestamp::Timestamp::from_gregorian(1, 1));

    let nop: Option<u8> = None;
}

fn datetime() {
    use std::ops::Add;
    use std::time::{Duration, Instant, SystemTime};

    let system_time = SystemTime::UNIX_EPOCH;
    let instant = Instant::now().add(Duration::from_secs(10));

    let nop: Option<u8> = None;
}

fn thread_local_const_init() {
    thread_local! {
        static CONSTANT_THREAD_LOCAL: i32 = const { 1337 }
    }
    CONSTANT_THREAD_LOCAL.with(|ctx| println!("const tls: {ctx}"));

    let nop: Option<u8> = None;
}

fn boxed_array() {
    let v = vec![1, 2, 3, 4, 5];
    let box_v = v.into_boxed_slice();

    let nop: Option<u8> = None;
}
// TODO supports boxed arrays

fn debug_fmt_vars() {
    use core::fmt::Debug;

    let v1: Vec<u32> = vec![];
    let v2 = Vec::<u64>::new();
    let v3: Vec<i32> = vec![1, 23, 3];

    #[derive(Debug)]
    struct Struct0 {
        a: u64,
    }

    #[derive(Debug)]
    struct Struct1<T: Debug> {
        field1: i32,
        field2: T,
    }

    #[derive(Debug)]
    struct Struct2<A: Debug, B: Debug> {
        field1: A,
        field2: B,
    }

    #[derive(Debug)]
    struct Struct3 {
        field1: i32,
        field2: u64,
    }

    let s0: Struct0 = Struct0 { a: 1 };
    let s1: Struct1<u64> = Struct1::<u64> {
        field1: 1,
        field2: 3,
    };
    let s2: Struct1<String> = Struct1::<String> {
        field1: 1,
        field2: "44".to_string(),
    };
    let s3: Struct2<String, u64> = Struct2::<String, u64> {
        field1: "66".to_string(),
        field2: 55,
    };
    let s4 = Struct3 {
        field1: 11,
        field2: 12,
    };

    let str_array = ["abc", "ef", "g"];

    #[derive(Debug)]
    enum Enum1 {
        A,
        B,
    }
    let c_enum = Enum1::A;

    #[derive(Debug)]
    enum Enum2<T1: Debug, T2: Debug> {
        S1(Struct1<T1>),
        S2(Struct2<T1, T2>),
    }
    let r_enum1: Enum2<&str, ()> = Enum2::S1::<&str, ()>(Struct1 {
        field1: 100,
        field2: "100",
    });
    let r_enum2: Enum2<u64, u32> = Enum2::S2(Struct2 {
        field1: 1,
        field2: 2,
    });

    let opt = Some(1);

    let my_str = "some str";
    let my_string = "some string".to_string();

    _ = format!("{:?}", s0);
    _ = format!("{:?}", v1);
    _ = format!("{:?}", v2);
    _ = format!("{:?}", v3);
    _ = format!("{:?}", s1);
    _ = format!("{:?}", s2);
    _ = format!("{:?}", s3);
    _ = format!("{:?}", s4);
    _ = format!("{:?}", str_array);
    _ = format!("{:?}", c_enum);
    _ = format!("{:?}", r_enum1);
    _ = format!("{:?}", r_enum2);
    _ = format!("{:?}", opt);
    _ = format!("{:?}", my_str);
    _ = format!("{:?}", my_string);

    let nop: Option<u8> = None;
}

fn debug_fmt_args(arg1: String, arg2: Vec<String>) {
    _ = format!("{:?}", arg1);
    _ = format!("{:?}", arg2);
}

pub fn main() {
    scalar_types();
    compound_types();
    array();
    enums();
    references();
    type_alias();
    type_params();
    vec_and_slice_types();
    string_types();
    static_vars();
    static_vars_same_name();
    thread_local();
    fn_and_closure();
    arguments(1, &2, vec![3, 4, 5], Box::new([6, 7, 8]));
    unions();
    hashmap();
    hashset();
    circular();
    lexical_blocks();
    btree_map();
    btreeset();
    vecdeque();
    atomics();
    inner_mut();
    ptr_to_array();
    shared_ptrs();
    zst();
    inner_static();
    shadowing();
    uuid();
    datetime();
    thread_local_const_init();
    boxed_array();
    debug_fmt_vars();
    debug_fmt_args(
        "one".to_string(),
        vec!["two".to_string(), "three".to_string()],
    );
    phase1_specs();
    phase1_specs_b(); phase3_dyn_trait(); phase3_niche_options();
}

fn phase1_specs() {
    use std::ptr::NonNull;
    let mut x: i32 = 42;
    let nn: NonNull<i32> = NonNull::from(&mut x);
    let _y = unsafe { nn.as_ref() };

    let nop: Option<u8> = None;
}

fn phase1_specs_b() {
    use std::pin::Pin;
    let pinned_box: Pin<Box<i32>> = Box::pin(7);
    let mut x: i32 = 13;
    let pinned_ref: Pin<&mut i32> = Pin::new(&mut x);

    let r1 = 0i32..10;
    let r2 = 0i32..=10;
    let r3 = 5i32..;
    let r4 = ..10i32;

    use std::time::Duration;
    let d_zero = Duration::new(0, 0);
    let d_ms = Duration::from_millis(1500);
    let d_s = Duration::from_secs(7);
    let d_h = Duration::new(3661, 500_000_000);

    use std::ffi::CString;
    let cs_hello: CString = CString::new("hello").unwrap();
    let cs_empty: CString = CString::new("").unwrap();
    let cs_bytes: CString = CString::new(vec![0x68u8, 0x69, 0x80, 0xff]).unwrap();

    use std::ffi::OsString;
    use std::path::PathBuf;
    let os_str: OsString = OsString::from("hello");
    let pb: PathBuf = PathBuf::from("/tmp/foo");

    use std::mem::MaybeUninit;
    let mu_init: MaybeUninit<i32> = MaybeUninit::new(99);

    use std::sync::{Mutex, RwLock};
    let mtx: Mutex<i32> = Mutex::new(123);
    let rwl: RwLock<i32> = RwLock::new(456);
    let mtx_guard = mtx.lock().unwrap();
    let rwl_read = rwl.read().unwrap();

    // DST companions to S12/S13/S14: &CStr, &OsStr, &Path.
    use std::ffi::{CStr, OsStr};
    use std::path::Path;
    let dst_cs: &CStr = CStr::from_bytes_with_nul(b"hi\0").unwrap();
    let dst_os: &OsStr = OsStr::new("hi");
    let dst_pa: &Path = Path::new("/etc");
    // Keep them live across the breakpoint by using them in
    // observable side effects.
    std::hint::black_box(dst_cs);
    std::hint::black_box(dst_os);
    std::hint::black_box(dst_pa);

    let nop: Option<u8> = None;
}

/// Phase 3 Feature A — `dyn Trait` recovery fixtures. The debugger
/// should resolve each fat-pointer trait object back to its concrete
/// type via the vtable symbol.
fn phase3_dyn_trait() {
    use std::error::Error;

    // Concrete error type the trait object holds — we expect the
    // debugger to recover this name from the vtable symbol.
    #[derive(Debug)]
    struct MyError {
        code: i32,
        msg: &'static str,
    }
    impl std::fmt::Display for MyError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "MyError(code={}, msg={:?})", self.code, self.msg)
        }
    }
    impl Error for MyError {}

    let boxed_err: Box<dyn Error> = Box::new(MyError {
        code: 42,
        msg: "boom",
    });

    // &dyn Iterator over a small concrete iterator type.
    let owned: Vec<u32> = vec![10, 20, 30];
    let iter_obj: &dyn Iterator<Item = u32> = &owned.iter().copied();
    // Force iter_obj to actually live across the breakpoint.
    std::hint::black_box(iter_obj);

    // Arc<dyn Send + Sync> — multi-bound trait object, distinct
    // dyn-bound layout.
    use std::sync::Arc;
    struct Counter(u32);
    let arc_obj: Arc<dyn std::fmt::Debug + Send + Sync> = Arc::new(Counter(7));
    impl std::fmt::Debug for Counter {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "Counter({})", self.0)
        }
    }
    std::hint::black_box(&arc_obj);
    std::hint::black_box(&boxed_err);

    let nop: Option<u8> = None;
}

/// Phase 3 Feature B — niche-encoded Option / Result fixtures.
/// Rust uses the inner type's invalid bit-patterns (null pointer,
/// zero `NonZero*`, byte ≥ 2 for `bool`, …) to encode `None`
/// without an extra discriminant byte. DWARF can't unambiguously
/// describe these enums; the debugger applies the language rules
/// directly.
fn phase3_niche_options() {
    use std::num::NonZeroU32;
    use std::ptr::NonNull;

    // Option<&T>: null pointer = None, anything else = Some(&T).
    let host: i32 = 99;
    let opt_ref_some: Option<&i32> = Some(&host);
    let opt_ref_none: Option<&i32> = None;

    // Option<Box<T>>: same niche — null inner pointer = None.
    let opt_box_some: Option<Box<i32>> = Some(Box::new(7));
    let opt_box_none: Option<Box<i32>> = None;

    // Option<NonNull<T>>: same niche.
    let mut x: i32 = 13;
    let opt_nn_some: Option<NonNull<i32>> = NonNull::new(&mut x);
    let opt_nn_none: Option<NonNull<i32>> = None;

    // Option<NonZeroU32>: zero = None, anything else = Some(N).
    let opt_nz_some: Option<NonZeroU32> = NonZeroU32::new(42);
    let opt_nz_none: Option<NonZeroU32> = None;

    // Option<bool>: niche is `byte ≥ 2 = None`.
    let opt_bool_some: Option<bool> = Some(true);
    let opt_bool_none: Option<bool> = None;

    // Option<fn(i32) -> i32>: null fn pointer = None.
    fn double_it(x: i32) -> i32 {
        x.wrapping_mul(2)
    }
    let opt_fn_some: Option<fn(i32) -> i32> = Some(double_it);
    let opt_fn_none: Option<fn(i32) -> i32> = None;

    // Result<&T, ()>: ZST error arm; the niche of `&T` doubles as
    // the discriminant for `Err(())`.
    let res_ok: Result<&i32, ()> = Ok(&host);
    let res_err: Result<&i32, ()> = Err(());

    // Result<NonZeroU32, ()>: same shape, ZST error arm.
    let res_nz_ok: Result<NonZeroU32, ()> = Ok(NonZeroU32::new(7).unwrap());
    let res_nz_err: Result<NonZeroU32, ()> = Err(());

    std::hint::black_box(&opt_ref_some);
    std::hint::black_box(&opt_ref_none);
    std::hint::black_box(&opt_box_some);
    std::hint::black_box(&opt_box_none);
    std::hint::black_box(&opt_nn_some);
    std::hint::black_box(&opt_nn_none);
    std::hint::black_box(&opt_nz_some);
    std::hint::black_box(&opt_nz_none);
    std::hint::black_box(&opt_bool_some);
    std::hint::black_box(&opt_bool_none);
    std::hint::black_box(&opt_fn_some);
    std::hint::black_box(&opt_fn_none);
    std::hint::black_box(&res_ok);
    std::hint::black_box(&res_err);
    std::hint::black_box(&res_nz_ok);
    std::hint::black_box(&res_nz_err);

    let nop: Option<u8> = None;
}
