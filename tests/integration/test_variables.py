# SPDX-License-Identifier: MIT
#
# Variable-rendering integration tests.
#
# **Status (2026-05-15):** 14 tests were migrated to `tests/scripts/*.json5`
# and now run under `cargo test --test scripts`. The remaining tests
# below exercise behaviours the V1 script runner doesn't yet model —
# multi-step lexical-block visibility, TLS state across threads, the
# custom-select DQE slicing syntax. Migrate them when the relevant
# gap closes.
#
# Migrated (use these .json5 scripts via `bs --test`):
#   * test_read_scalar_variables      → tests/scripts/read_scalars.json5
#   * test_read_struct                → tests/scripts/read_struct.json5
#   * test_read_array                 → tests/scripts/read_array.json5
#   * test_read_enum                  → tests/scripts/read_enum.json5
#   * test_read_pointers              → tests/scripts/read_pointers.json5
#   * test_deref_pointers             → tests/scripts/read_deref_pointers.json5
#   * test_read_type_alias            → tests/scripts/read_type_alias.json5
#   * test_read_strings               → tests/scripts/read_strings.json5
#   * test_read_vec_and_slice         → tests/scripts/read_vec_and_slice.json5
#   * test_read_arguments             → tests/scripts/read_arguments.json5
#   * test_zst_types                  → tests/scripts/read_zst.json5
#   * test_read_static_variables      → tests/scripts/read_statics.json5
#   * test_read_time                  → tests/scripts/read_time.json5
#   * test_address                    → tests/scripts/read_address_op.json5

import unittest
import pexpect
from helper import Debugger


class VariablesTestCase(unittest.TestCase):
    def setUp(self):
        self.debugger = Debugger("./examples/target/debug/vars")

    def test_read_scalar_variables_at_place(self):
        """Local variables reading only from the current lexical block"""
        self.debugger.cmd("break vars.rs:11", "New breakpoint")
        self.debugger.cmd("run", "11     let int128 = 3_i128;")
        self.debugger.cmd(
            "var locals",
            "int8 = i8(1)",
            "int16 = i16(-1)",
            "int64 = i64(-2)",
        )
        with self.assertRaises(pexpect.exceptions.TIMEOUT):
            self.debugger.expect_in_output("int128 = i128(3)", timeout=1)

    def test_read_static_variables_different_modules(self):
        """Reading rust static's from another module"""
        self.debugger.cmd("break vars.rs:179", "New breakpoint")
        self.debugger.cmd("run", "179     let nop: Option<u8> = None;")
        self.debugger.cmd_re(
            "var GLOB_3",
            r"vars::(ns_1::)?GLOB_3",
            r"vars::(ns_1::)?GLOB_3",
        )

    def test_read_tls_variables(self):
        """Reading rust tls variables"""
        self.debugger.cmd("break vars.rs:194", "New breakpoint")
        self.debugger.cmd("run", "194         let nop: Option<u8> = None;")
        self.debugger.cmd("var THREAD_LOCAL_VAR_1", "= Cell<i32>(2)")
        self.debugger.cmd("var THREAD_LOCAL_VAR_2", "= Cell<&str>(2)")
        # assert uninit tls variables
        self.debugger.cmd("break vars.rs:199", "New breakpoint")
        self.debugger.cmd("continue", "199         let nop: Option<u8> = None;")
        self.debugger.cmd("var THREAD_LOCAL_VAR_1")
        # assert tls variables changes in another thread
        self.debugger.cmd("break vars.rs:203", "New breakpoint")
        self.debugger.cmd("continue", "203     let nop: Option<u8> = None;")
        self.debugger.cmd("var THREAD_LOCAL_VAR_1", " = Cell<i32>(1)")

    def test_custom_select(self):
        """Reading memory by select expressions"""
        self.debugger.cmd("break vars.rs:61", "New breakpoint")
        self.debugger.cmd("run", "61     let nop: Option<u8> = None;")
        self.debugger.cmd("var arr_2[0][2]", "i32(2)")
        self.debugger.cmd(
            "var arr_1[2..4]",
            "[i32] {",
            "2: 2",
            "3: -2",
            "}",
        )
        self.debugger.cmd(
            "var arr_1[..]",
            "[i32] {",
            "0: 1",
            "1: -1",
            "2: 2",
            "3: -2",
            "4: 3",
            "}",
        )
        self.debugger.cmd(
            "var arr_1[..2]",
            "[i32] {",
            "0: 1",
            "1: -1",
            "}",
        )
        self.debugger.cmd(
            "var arr_1[3..]",
            "[i32] {",
            "3: -2",
            "4: 3",
            "}",
        )
        self.debugger.cmd(
            "var arr_1[4..6]",
            "[i32] {",
            "4: 3",
            "}",
        )
        self.debugger.cmd(
            "var arr_1[2..4][1..]",
            "[i32] {",
            "3: -2",
            "}",
        )

        self.debugger.cmd("break vars.rs:93", "New breakpoint")
        self.debugger.cmd("continue", "93     let nop: Option<u8> = None;")
        self.debugger.cmd("var enum_6.__0.a", "i32(1)")

        self.debugger.cmd("break vars.rs:119", "New breakpoint")
        self.debugger.cmd("continue", "119     let nop: Option<u8> = None;")
        self.debugger.cmd("var *((*ref_f).foo)", "i32(2)")

        self.debugger.cmd("break vars.rs:290", "New breakpoint")
        self.debugger.cmd("continue", "290     let nop: Option<u8> = None;")
        self.debugger.cmd(
            "var hm2.abc",
            "Vec<i32, alloc::alloc::Global> {",
            "buf: [i32] {",
            "0: 1",
            "1: 2",
            "2: 3",
            "}",
            "cap: usize(3)",
            "}",
        )
        self.debugger.cmd("var hm1[false]", "i64(5)")
        self.debugger.cmd('var hm2["abc"]', "Vec<i32, alloc::alloc::Global> {")
        self.debugger.cmd("var hm3[55]", "i32(55)")
        self.debugger.cmd('var hm4["1"][1]', "i32(1)")

        self.debugger.cmd("var a")
        addr = self.debugger.search_in_output(r"a = &i32 \[(.*)\]")
        self.debugger.cmd(f"var hm5[{addr}]", "&str(a)")

        self.debugger.cmd("break vars.rs:307", "New breakpoint")
        self.debugger.cmd("continue", "307     let nop: Option<u8> = None;")
        self.debugger.cmd("var hs1[1]", "bool(true)")
        self.debugger.cmd("var hs2[22]", "bool(true)")
        self.debugger.cmd("var hs2[222]", "bool(false)")

        self.debugger.cmd("var b")
        addr = self.debugger.search_in_output(r"b = &i32 \[(.*)\]")
        self.debugger.cmd(f"var hs4[{addr}]", "bool(true)")
        self.debugger.cmd("var hs4[0x000]", "bool(false)")

        self.debugger.cmd("break vars.rs:460", "New breakpoint")
        self.debugger.cmd("continue", "460     let nop: Option<u8> = None;")
        self.debugger.cmd(
            "var ptr[..4]",
            "[i32] {",
            "0: 1",
            "1: 2",
            "2: 3",
            "3: 4",
            "}",
        )

    def test_ptr_cast(self):
        """Cast const address to a typed pointer"""
        self.debugger.cmd("break vars.rs:119", "New breakpoint")
        self.debugger.cmd("run", "let nop: Option<u8> = None;")
        self.debugger.cmd("var ref_a")
        addr = self.debugger.search_in_output(r"ref_a = &i32 \[0x(.*)\]")
        addr = "0x" + addr[:14]
        self.debugger.cmd(f"var *(*const i32){addr}", "i32(2)")

    # test_address migrated → tests/scripts/read_address_op.json5
    # (the hm6 slice DQE block from the python test isn't migrated
    # because the slicing-into-HashMap syntax overlaps with
    # `test_custom_select`'s still-Python coverage; revisit in the
    # custom-select migration.)
