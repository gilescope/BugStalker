//! Tiny dependency carrying a file-scope static, for
//! `test_thin_crate_sees_dep_statics` (variables-view): a thin binary
//! with no statics of its own must still surface this one under the
//! all-crates Statics default.
pub static DEP_STATIC: u64 = 1234;
