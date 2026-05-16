//! BugStalker showcase. Single `fn main`, one variable per logical
//! step — set a breakpoint at the first `let` and step (Step Over)
//! down. Each section heading marks a category of Rust value the
//! debugger renders distinctly.
//!
//! cargo run --bin showcase     # run normally
//! bugstalker --bin showcase    # debug under BugStalker
//! F5 in VS Code (BugStalker extension) for the IDE flow.

#![allow(unused, dead_code)]

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::future::Future;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

fn main() {
    // 1. primitives
    let int_signed: i64 = -42;
    let int_unsigned: u128 = 1u128 << 100;
    let float: f64 = std::f64::consts::PI;
    let boolean: bool = true;
    let character: char = '🦀';

    // 2. strings — borrowed, owned, OS-flavoured
    let borrowed: &str = "hello";
    let owned: String = String::from("hello");
    let os: OsString = OsString::from("hello.txt");

    // 3. tuples, arrays, slices
    let unit: () = ();
    let pair: (i32, &str) = (1, "one");
    let array: [i32; 4] = [10, 20, 30, 40];
    let slice: &[i32] = &array[1..3];

    // 4. collections
    let v: Vec<u8> = vec![1, 2, 3];
    let map: HashMap<&str, i32> = HashMap::from([("a", 1), ("b", 2)]);
    let bmap: BTreeMap<i32, &str> = BTreeMap::from([(1, "x"), (2, "y")]);

    // 5. niche-optimised Option / Result — same size as the inner
    //    type; BugStalker decodes the absent variant from the niche.
    let nz_some: Option<NonZeroU32> = NonZeroU32::new(42);
    let nz_none: Option<NonZeroU32> = None;
    let opt_ref: Option<&i32> = Some(&array[0]);
    let result_ok: Result<i32, String> = Ok(7);
    let result_err: Result<i32, String> = Err("oops".into());

    // 6. structs — named, tuple, unit
    #[derive(Debug)]
    struct Point {
        x: f64,
        y: f64,
    }
    struct Wrap(i32, i32);
    struct Marker;
    let point = Point { x: 1.0, y: 2.5 };
    let wrap = Wrap(7, 8);
    let _marker = Marker;

    // 7. enums — C-like, data-bearing, mixed
    #[derive(Debug)]
    enum Direction {
        N,
        S,
        E,
        W,
    }
    #[derive(Debug)]
    enum Shape {
        Circle { r: f64 },
        Square(f64),
        Empty,
    }
    let dir = Direction::E;
    let shape_circle = Shape::Circle { r: 2.0 };
    let shape_square = Shape::Square(3.0);
    let shape_empty = Shape::Empty;

    // 8. smart pointers
    let boxed: Box<i32> = Box::new(99);
    let rc: Rc<String> = Rc::new(String::from("shared"));
    let _rc_clone = Rc::clone(&rc); // refcount goes to 2
    let arc: Arc<u64> = Arc::new(0xDEAD_BEEF);

    // 9. interior mutability
    let cell = Cell::new(7i32);
    cell.set(8);
    let refcell = RefCell::new(vec![1i32, 2, 3]);
    refcell.borrow_mut().push(4);
    let mutex = Mutex::new(String::from("locked"));

    // 10. references — shared, mutable, raw
    let n = 100i32;
    let shared_ref: &i32 = &n;
    if let Ok(unlock) = mutex.lock() {
        println!("{}", *unlock);
        let raw_ptr: *const i32 = &n;
        let mut m = 200i32;
        let mut_ref: &mut i32 = &mut m;
    }

    // 11. trait objects — vtable-driven concrete-type recovery
    trait Greeter {
        fn greet(&self) -> String;
    }
    impl Greeter for Point {
        fn greet(&self) -> String {
            format!("({}, {})", self.x, self.y)
        }
    }
    let dyn_ref: &dyn Greeter = &point;
    let dyn_box: Box<dyn Greeter> = Box::new(Point { x: 0.0, y: 0.0 });

    // 11b. multi-bound trait object — `dyn Trait + Send + Sync`.
    //      Auto-trait markers (Send, Sync) carry no vtable entries,
    //      so the fat-pointer layout is identical to a plain
    //      `dyn Error`. The bound list is purely a type-system
    //      constraint, which the debugger should still surface.
    use std::error::Error;
    #[derive(Debug)]
    struct ShowcaseErr(&'static str);
    impl std::fmt::Display for ShowcaseErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }
    impl Error for ShowcaseErr {}
    let multi_bound: Box<dyn Error + Send + Sync> = Box::new(ShowcaseErr("boom"));

    // 12. closures — capturing both Copy and non-Copy state
    let captured_copy = 10;
    let captured_string = String::from("state");
    let closure = move |x: i32| x + captured_copy + captured_string.len() as i32;
    let _ = closure(1);

    // 13. iterator chain — closures inside adapter types
    let squares: Vec<i32> = (1..=5).map(|x| x * x).filter(|&x| x > 2).collect();

    // 14. recursive type — `Box<Self>` cycle, depth-bounded render
    enum List {
        Cons(i32, Box<List>),
        Nil,
    }
    let list = List::Cons(1, Box::new(List::Cons(2, Box::new(List::Nil))));

    // 15. async future — built but not polled. The pin'd box is a
    //     `dyn Future` trait object; BugStalker recovers the concrete
    //     coroutine type from its DWARF-emitted layout.
    let pinned: Pin<Box<dyn Future<Output = u32>>> = Box::pin(async { 1u32 + 2 });

    // 16. break here for the final view
    println!(
        "done. {dir:?} {shape_circle:?} {shape_square:?} {shape_empty:?} {point:?} squares={squares:?}"
    );

    // 17. demonstrate the panic auto-trap. Pass `--panic` to see
    //     BugStalker stop at `core::panicking::panic_fmt` with the
    //     full backtrace through here. Without the flag, the program
    //     exits cleanly — the exit auto-trap stops there instead.
    if std::env::args().any(|a| a == "--panic") {
        let cause = multi_bound;
        panic!("showcase panic — cause was {cause}");
    }
    // Reference multi_bound so it survives DCE when --panic is absent.
    std::hint::black_box(&multi_bound);
}
