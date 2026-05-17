// SPDX-License-Identifier: MIT
//! Rust port of `tests/integration/test_command.py`. All 28 tests
//! port 1:1. Helpers used: `cmd`, `cmd_re`, `search_in_output`,
//! `quit`. Two tests (`write_register`, `address_breakpoint_set`)
//! spawn a second `Debugger` mid-test after using
//! `search_in_output` to scrape addresses out of bp output.

use crate::helper::Debugger;
use serial_test::serial;

const HELLO: &str = "./examples/target/debug/hello_world";
const CALC: &str = "./examples/target/debug/calc";
const VARS: &str = "./examples/target/debug/vars";
const PANIC_BIN: &str = "./examples/target/debug/panic";

fn fresh() -> Debugger {
    Debugger::spawn(HELLO)
}

#[test]
#[serial]
fn debugee_execute() {
    let mut dbg = fresh();
    dbg.cmd("run", &["Hello, world!", "bye!"]);
}

#[test]
#[serial]
fn function_breakpoint() {
    let mut dbg = fresh();
    dbg.cmd("break main", &["New breakpoint"]);
    dbg.cmd("run", &["myprint(\"Hello, world!\");"]);
    dbg.cmd("break myprint", &["New breakpoint"]);
    dbg.cmd("continue", &["Hit breakpoint 2"]);
    dbg.cmd("continue", &["Hello, world!", "Hit breakpoint 2"]);
    dbg.cmd("continue", &["bye"]);
}

#[test]
#[serial]
fn line_breakpoint() {
    let mut dbg = fresh();
    dbg.cmd("break hello_world.rs:15", &["New breakpoint"]);
    dbg.cmd("run", &["15     println!(\"{}\", s)"]);
    dbg.cmd("continue", &["Hello, world!", "15     println!(\"{}\", s)"]);
    dbg.cmd("continue", &["bye!"]);
}

#[test]
#[serial]
fn multiple_breakpoints_set() {
    let mut dbg = fresh();
    dbg.cmd("break hello_world.rs:5", &["New breakpoint"]);
    dbg.cmd("break hello_world.rs:9", &["New breakpoint"]);
    dbg.cmd(
        "run",
        &["Hit breakpoint 1 at", "myprint(\"Hello, world!\")"],
    );
    dbg.cmd(
        "continue",
        &["Hello, world!", "Hit breakpoint 2 at", "myprint(\"bye!\")"],
    );
    dbg.cmd("continue", &["bye!"]);
}

#[test]
#[serial]
fn address_breakpoint_set() {
    let mut dbg = fresh();
    dbg.cmd("break hello_world.rs:5", &["New breakpoint"]);
    dbg.cmd("run", &[]);
    let addr = dbg
        .search_in_output(r"Hit breakpoint 1 at .*0x(.*):", 10)
        .map(|s| {
            let trimmed: String = s.chars().take(14).collect();
            format!("0x{trimmed}")
        })
        .expect("could not scrape bp1 address");
    dbg.quit();

    // Respawn debugger and test address breakpoint.
    let mut dbg = fresh();
    dbg.cmd(&format!("break {addr}"), &["New breakpoint"]);
    dbg.cmd("run", &["Hit breakpoint 1 at"]);
    dbg.cmd("continue", &["Hello, world!", "bye!"]);
}

#[test]
#[serial]
fn write_register() {
    let mut dbg = fresh();
    dbg.cmd("break hello_world.rs:4", &["New breakpoint"]);
    dbg.cmd("break hello_world.rs:10", &["New breakpoint"]);
    dbg.cmd("run", &[]);

    let start_addr = dbg
        .search_in_output(r"Hit breakpoint 1 at .*0x(.*):", 10)
        .map(|s| {
            let trimmed: String = s.chars().take(14).collect();
            format!("0x{trimmed}")
        })
        .expect("scrape bp1");
    assert!(!start_addr.is_empty());

    dbg.cmd("continue", &[]);
    let addr = dbg
        .search_in_output(r"Hit breakpoint 2 at .*0x(.*):", 10)
        .map(|s| {
            let trimmed: String = s.chars().take(14).collect();
            format!("0x{trimmed}")
        })
        .expect("scrape bp2");
    assert!(!addr.is_empty());
    dbg.quit();

    let addr_as_int = u64::from_str_radix(addr.trim_start_matches("0x"), 16).unwrap();
    let ret_addr = format!("0x{:x}", addr_as_int + 1);

    let mut dbg = fresh();
    dbg.cmd(&format!("break {ret_addr}"), &["New breakpoint"]);
    dbg.cmd("run", &["Hello, world!", "bye!"]);
    dbg.cmd(&format!("register write rip {start_addr}"), &[]);
    dbg.cmd("continue", &["Hello, world!", "bye!"]);
}

#[test]
#[serial]
fn step_in() {
    let mut dbg = Debugger::spawn(&format!("{CALC} -- 1 2 3 --description result"));
    dbg.cmd("break main.rs:10", &["New breakpoint"]);
    dbg.cmd("run", &["10     let s: i64"]);
    dbg.cmd("step", &["calc::sum3", "25     let ab = sum2"]);
    dbg.cmd("step", &["calc::sum2", "21     a + b"]);
    dbg.cmd("step", &["22 }"]);
    dbg.cmd("step", &["calc::sum3", "26     sum2(ab, c)"]);
    dbg.cmd("step", &["calc::sum2", "21     a + b"]);
    dbg.cmd("step", &["22 }"]);
    dbg.cmd("step", &["calc::sum3", "27 }"]);
    dbg.cmd("step", &["calc::main", "15     print(s, &args[5]);"]);
}

#[test]
#[serial]
fn step_out() {
    let mut dbg = fresh();
    dbg.cmd("break hello_world.rs:15", &["New breakpoint"]);
    dbg.cmd("run", &["15     println!(\"{}\", s)"]);
    dbg.cmd("stepout", &["7     sleep(Duration::from_secs(1));"]);
}

#[test]
#[serial]
fn step_over() {
    let mut dbg = fresh();
    dbg.cmd("break hello_world.rs:5", &["New breakpoint"]);
    dbg.cmd("run", &["myprint(\"Hello, world!\");"]);
    dbg.cmd("next", &["7     sleep(Duration::from_secs(1));"]);
    dbg.cmd("next", &["9     myprint(\"bye!\")"]);
    dbg.cmd("next", &["10 }"]);
}

#[test]
#[serial]
fn step_over_on_fn_decl() {
    let mut dbg = fresh();
    dbg.cmd("break hello_world.rs:14", &["New breakpoint"]);
    dbg.cmd("run", &["Hit breakpoint 1 at"]);
    dbg.cmd("next", &["15     println!(\"{}\", s)"]);
}

#[test]
#[serial]
fn get_symbol() {
    let mut dbg = fresh();
    dbg.cmd_re(
        "symbol main",
        &["__libc_start_main", r"main - Text 0x[0-9A-F]{1,16}"],
    );
}

#[test]
#[serial]
fn backtrace() {
    let mut dbg = fresh();
    dbg.cmd("break hello_world.rs:15", &["New breakpoint"]);
    dbg.cmd("run", &["15     println!(\"{}\", s)"]);
    dbg.cmd("bt", &["myprint", "hello_world::main"]);
}

#[test]
#[serial]
fn args_for_executable() {
    let mut dbg = Debugger::spawn(&format!("{CALC} -- 1 1 1 --description three"));
    dbg.cmd("run", &["three: 3"]);
}

#[test]
#[serial]
fn read_value_u64() {
    let mut dbg = Debugger::spawn(&format!("{CALC} -- 1 2 3 --description result"));
    dbg.cmd("break main.rs:15", &["New breakpoint"]);
    dbg.cmd("run", &["15     print(s, &args[5]);"]);
    dbg.cmd("var locals", &["s = i64(6)"]);
}

#[test]
#[serial]
fn function_breakpoint_remove() {
    let mut dbg = fresh();
    dbg.cmd("break main", &["New breakpoint"]);
    dbg.cmd("break remove main", &["Removed breakpoint"]);
    dbg.cmd("run", &["bye!"]);
}

#[test]
#[serial]
fn line_breakpoint_remove() {
    let mut dbg = fresh();
    dbg.cmd("break hello_world.rs:15", &["New breakpoint"]);
    dbg.cmd("run", &["15     println!(\"{}\", s)"]);
    dbg.cmd("break remove hello_world.rs:15", &["Removed breakpoint"]);
    dbg.cmd("continue", &["bye!"]);
}

#[test]
#[serial]
fn breakpoint_remove_by_number() {
    let mut dbg = fresh();
    dbg.cmd("break main", &["New breakpoint"]);
    dbg.cmd("break remove 1", &["Removed breakpoint"]);
    dbg.cmd("run", &["bye!"]);
}

#[test]
#[serial]
fn breakpoint_info() {
    let mut dbg = fresh();
    dbg.cmd("break hello_world.rs:9", &["New breakpoint"]);
    dbg.cmd("break myprint", &["New breakpoint"]);
    dbg.cmd("break main", &["New breakpoint"]);
    dbg.cmd("break hello_world.rs:7", &["New breakpoint"]);
    dbg.cmd_re(
        "break info",
        &[
            r"- Breakpoint 1 at .*0x[0-9A-F]{14,16}.*: .*/hello_world\.rs.*:9",
            r"- Breakpoint 2 at .*0x[0-9A-F]{14,16}.*: .*/hello_world\.rs.*:15",
            r"- Breakpoint 3 at .*0x[0-9A-F]{14,16}.*: .*/hello_world\.rs.*:5",
            r"- Breakpoint 4 at .*0x[0-9A-F]{14,16}.*: .*/hello_world\.rs.*:7",
        ],
    );
    dbg.cmd("run", &[]);
    dbg.cmd_re(
        "break info",
        &[
            r"- Breakpoint 1 at .*0x[0-9A-F]{14,16}.*: .*/hello_world\.rs.*:9 ",
            r"- Breakpoint 2 at .*0x[0-9A-F]{14,16}.*: .*/hello_world\.rs.*:15",
            r"- Breakpoint 3 at .*0x[0-9A-F]{14,16}.*: .*/hello_world\.rs.*:5",
            r"- Breakpoint 4 at .*0x[0-9A-F]{14,16}.*: .*/hello_world\.rs.*:7",
        ],
    );
    dbg.cmd("break remove main", &["Removed breakpoint"]);
    dbg.cmd_re(
        "break info",
        &[
            r"- Breakpoint 1 at .*0x[0-9A-F]{14,16}.*: .*/hello_world\.rs.*:9 ",
            r"- Breakpoint 2 at .*0x[0-9A-F]{14,16}.*: .*/hello_world\.rs.*:15",
            r"- Breakpoint 4 at .*0x[0-9A-F]{14,16}.*: .*/hello_world\.rs.*:7",
        ],
    );
}

#[test]
#[serial]
fn debugee_restart() {
    let mut dbg = fresh();
    dbg.cmd("run", &["Hello, world!", "bye!"]);
    dbg.cmd("run", &["Restart a program?"]);
    dbg.cmd("y", &["Hello, world!", "bye!"]);
}

#[test]
#[serial]
fn debugee_restart_at_bp() {
    let mut dbg = fresh();
    dbg.cmd("break hello_world.rs:9", &["New breakpoint"]);
    dbg.cmd("run", &["Hello, world!"]);
    dbg.cmd("run", &["Restart a program?"]);
    dbg.cmd("y", &["Hello, world!"]);
    dbg.cmd("continue", &["bye!"]);
}

#[test]
#[serial]
fn debugee_restart_at_end() {
    let mut dbg = fresh();
    dbg.cmd("break hello_world.rs:9", &["New breakpoint"]);
    dbg.cmd("run", &["Hello, world!", "Hit breakpoint 1"]);
    dbg.cmd("continue", &["bye!"]);
    dbg.cmd("run", &["Restart a program?"]);
    dbg.cmd("y", &["Hello, world!", "Hit breakpoint 1"]);
    dbg.cmd("quit", &[]);
}

#[test]
#[serial]
fn frame_switch() {
    let mut dbg = Debugger::spawn(&format!("{CALC} -- 1 2 3 --description result"));
    dbg.cmd("break main.rs:21", &["New breakpoint 1"]);
    dbg.cmd("run", &["Hit breakpoint 1"]);
    dbg.cmd("arg all", &["a = i64(1)", "b = i64(2)"]);
    dbg.cmd("frame switch 1", &[]);
    dbg.cmd("arg all", &["a = i64(1)", "b = i64(2)", "c = i64(3)"]);
}

#[test]
#[serial]
fn disasm() {
    let mut dbg = fresh();
    dbg.cmd("break main", &["New breakpoint"]);
    dbg.cmd("run", &[]);
    dbg.cmd(
        "source asm",
        &["Assembler code for function hello_world::main", "mov"],
    );
    dbg.cmd("break myprint", &["New breakpoint"]);
    dbg.cmd("continue", &[]);
    dbg.cmd(
        "source asm",
        &["Assembler code for function hello_world::myprint", "mov"],
    );
}

#[test]
#[serial]
fn source_fn() {
    let mut dbg = fresh();
    dbg.cmd("break main", &["New breakpoint"]);
    dbg.cmd("run", &[]);
    dbg.cmd(
        "source fn",
        &[
            "hello_world::main at",
            "4 fn main() {",
            "7     sleep(Duration::from_secs(1));",
            "10 }",
        ],
    );
}

#[test]
#[serial]
fn source_fn_with_frame_switch() {
    let mut dbg = Debugger::spawn(&format!("{CALC} -- 1 2 3 --description result"));
    dbg.cmd("break main.rs:21", &["New breakpoint 1"]);
    dbg.cmd("run", &["Hit breakpoint 1"]);
    dbg.cmd(
        "source fn",
        &["fn sum2(a: i64, b: i64) -> i64 {", "a + b", "}"],
    );
    dbg.cmd("frame switch 1", &[]);
    dbg.cmd(
        "source fn",
        &[
            "fn sum3(a: i64, b: i64, c: i64) -> i64 {",
            "let ab = sum2(a, b);",
            "sum2(ab, c)",
            "}",
        ],
    );
    dbg.cmd("frame switch 2", &[]);
    dbg.cmd(
        "source fn",
        &[
            "fn main() {",
            "let args: Vec<String> = env::args().collect();",
            "let v1 = &args[1];",
            "let v2 = &args[2];",
            "}",
        ],
    );
}

#[test]
#[serial]
fn source_bounds() {
    let mut dbg = fresh();
    dbg.cmd("break main", &["New breakpoint"]);
    dbg.cmd("run", &[]);
    dbg.cmd(
        "source 4",
        &[
            "1 use std::thread::sleep;",
            "4 fn main() {",
            "9     myprint(\"bye!\")",
        ],
    );
}

#[test]
#[serial]
fn breakpoint_at_rust_panic() {
    let mut dbg = Debugger::spawn(&format!("{PANIC_BIN} -- user"));
    dbg.cmd("break rust_panic", &["New breakpoint"]);
    dbg.cmd("run", &["then panic!"]);
    dbg.cmd("bt", &["rust_panic"]);
    dbg.cmd("continue", &[]);

    let mut dbg = Debugger::spawn(&format!("{PANIC_BIN} -- system"));
    dbg.cmd("break rust_panic", &["New breakpoint"]);
    dbg.cmd("run", &["attempt to divide by zero"]);
    dbg.cmd("bt", &["rust_panic"]);
}

#[test]
#[serial]
fn trigger() {
    let mut dbg = Debugger::spawn(VARS);
    dbg.cmd("trigger any", &[]);
    dbg.cmd("backtrace", &[]);
    dbg.cmd("var int8", &[]);
    dbg.cmd("end", &[]);
    dbg.cmd("break vars.rs:19", &["New breakpoint 1"]);
    dbg.cmd(
        "run",
        &["Hit breakpoint 1", "vars::scalar_types", "int8 = i8(1)"],
    );

    dbg.cmd("trigger", &[]);
    dbg.cmd("var int16", &[]);
    dbg.cmd("end", &[]);

    dbg.cmd(
        "trigger info",
        &[
            "Any breakpoint or watchpoint",
            "backtrace, var int8",
            "Breakpoint 1",
            "var int16",
        ],
    );

    dbg.cmd("trigger any", &[]);
    dbg.cmd("end", &[]);

    dbg.cmd("run", &[]);
    dbg.cmd("y", &["Hit breakpoint 1", "int16 = i16(-1)"]);

    dbg.cmd("break vars.rs:30", &["New breakpoint 2"]);
    dbg.cmd("trigger b 2", &[]);
    dbg.cmd("var f32", &[]);
    dbg.cmd("var f64", &[]);
    dbg.cmd("end", &[]);

    dbg.cmd("watch int16", &["New watchpoint 1"]);
    dbg.cmd("trigger w 1", &[]);
    dbg.cmd("backtrace", &[]);
    dbg.cmd("end", &[]);

    dbg.cmd("c", &["f32 = f32(1.1)", "f64 = f64(1.2)"]);
    dbg.cmd("c", &["vars::scalar_types"]);
}
