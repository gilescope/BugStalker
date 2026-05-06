// SPDX-License-Identifier: MIT
//! Snapshot of `/proc/<pid>/maps`.
//!
//! Each line of `/proc/<pid>/maps` describes one VMA (virtual
//! memory area) in the target process: address range,
//! permissions, file backing (if any), and inode. The plan's
//! Tier 2 fork-checkpoint payload includes this layout so replay
//! can decide which regions need restoring vs. which can be
//! re-mapped from the same file backing.
//!
//! Pure-Rust text parser. The parser itself is platform-agnostic;
//! the read function is Linux-only.

use std::fs;

use nix::unistd::Pid;

/// One virtual memory area as reported by the kernel.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct MemoryRegion {
    /// Start virtual address (inclusive).
    pub start: u64,
    /// End virtual address (exclusive).
    pub end: u64,
    /// Per-region access permissions.
    pub perms: Permissions,
    /// Byte offset into the backing file. 0 for anonymous regions.
    pub offset: u64,
    /// Device the backing file lives on, e.g. `fd:00`. The
    /// kernel emits this as a `MAJOR:MINOR` pair in hex; we
    /// store it verbatim because consumers downstream don't
    /// need to interpret it.
    pub dev: String,
    /// Inode of the backing file, 0 for anonymous regions.
    pub inode: u64,
    /// Pathname of the backing file (e.g. `/usr/lib/libc.so.6`)
    /// or a kernel pseudo-name (`[heap]`, `[stack]`, `[vdso]`).
    /// `None` for anonymous regions with no kernel name.
    pub pathname: Option<String>,
}

/// Per-VMA access permissions decoded from the four-char field
/// (e.g. `r-xp` → read+execute, private).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Permissions {
    /// Region readable by the owning process.
    pub read: bool,
    /// Region writable by the owning process.
    pub write: bool,
    /// Region executable.
    pub execute: bool,
    /// Region is private (copy-on-write); `false` for shared.
    pub private: bool,
}

/// Errors arising from the snapshotter.
#[derive(thiserror::Error, Debug)]
pub enum ProcMapsError {
    /// Couldn't open or read `/proc/<pid>/maps`.
    #[error("/proc/{pid}/maps I/O: {source}")]
    Io {
        /// PID we tried to read.
        pid: i32,
        /// Underlying io::Error.
        source: std::io::Error,
    },
    /// A line in `/proc/<pid>/maps` failed to parse.
    #[error("/proc/{pid}/maps line {line_no}: {detail}")]
    Parse {
        /// PID we read.
        pid: i32,
        /// 1-based line number that failed.
        line_no: usize,
        /// Human-readable detail.
        detail: String,
    },
}

/// Parse one `/proc/<pid>/maps` line. Format:
///
/// ```text
/// START-END PERMS OFFSET DEV INODE [PATHNAME]
/// ```
///
/// Pathname is optional (anonymous regions have no name) and may
/// contain spaces (file paths). The parser splits on the first
/// five whitespace runs and treats everything after as the
/// pathname.
pub fn parse_line(line: &str) -> Result<MemoryRegion, String> {
    let mut parts = line.splitn(6, ' ').filter(|s| !s.is_empty());
    let range = parts.next().ok_or("missing address range")?;
    let perms_s = parts.next().ok_or("missing perms")?;
    let offset_s = parts.next().ok_or("missing offset")?;
    let dev = parts.next().ok_or("missing dev")?.to_owned();
    let inode_s = parts.next().ok_or("missing inode")?;
    let pathname = parts
        .next()
        .map(|s| s.trim_start().trim_end_matches('\n').to_owned())
        .filter(|s| !s.is_empty());

    let (start_s, end_s) = range
        .split_once('-')
        .ok_or_else(|| format!("address range `{range}` missing `-`"))?;
    let start = u64::from_str_radix(start_s, 16)
        .map_err(|e| format!("bad start `{start_s}`: {e}"))?;
    let end = u64::from_str_radix(end_s, 16)
        .map_err(|e| format!("bad end `{end_s}`: {e}"))?;

    let pb = perms_s.as_bytes();
    if pb.len() != 4 {
        return Err(format!("perms `{perms_s}` must be 4 chars"));
    }
    let perms = Permissions {
        read: pb[0] == b'r',
        write: pb[1] == b'w',
        execute: pb[2] == b'x',
        private: pb[3] == b'p',
    };

    let offset = u64::from_str_radix(offset_s, 16)
        .map_err(|e| format!("bad offset `{offset_s}`: {e}"))?;
    let inode: u64 = inode_s
        .parse()
        .map_err(|e| format!("bad inode `{inode_s}`: {e}"))?;

    Ok(MemoryRegion { start, end, perms, offset, dev, inode, pathname })
}

/// Read and parse `/proc/<pid>/maps` for the given pid.
pub fn read_proc_maps(pid: Pid) -> Result<Vec<MemoryRegion>, ProcMapsError> {
    let path = format!("/proc/{}/maps", pid.as_raw());
    let text = fs::read_to_string(&path).map_err(|e| ProcMapsError::Io {
        pid: pid.as_raw(),
        source: e,
    })?;
    let mut out = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        if raw.trim().is_empty() {
            continue;
        }
        match parse_line(raw) {
            Ok(r) => out.push(r),
            Err(detail) => {
                return Err(ProcMapsError::Parse {
                    pid: pid.as_raw(),
                    line_no: idx + 1,
                    detail,
                });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::fork_self::LinuxForkSelfMechanism;
    use crate::ring::CheckpointMechanism;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn parse_file_backed_line() {
        let line =
            "55ed8b3a4000-55ed8b3a8000 r-xp 00000000 fd:00 1234567   /usr/bin/cat";
        let r = parse_line(line).unwrap();
        assert_eq!(r.start, 0x55ed8b3a4000);
        assert_eq!(r.end, 0x55ed8b3a8000);
        assert!(r.perms.read);
        assert!(!r.perms.write);
        assert!(r.perms.execute);
        assert!(r.perms.private);
        assert_eq!(r.offset, 0);
        assert_eq!(r.dev, "fd:00");
        assert_eq!(r.inode, 1234567);
        assert_eq!(r.pathname.as_deref(), Some("/usr/bin/cat"));
    }

    #[test]
    fn parse_anonymous_line_has_no_pathname() {
        let line = "7f9c00000000-7f9c00021000 rw-p 00000000 00:00 0";
        let r = parse_line(line).unwrap();
        assert!(r.perms.read && r.perms.write && !r.perms.execute);
        assert_eq!(r.dev, "00:00");
        assert_eq!(r.inode, 0);
        assert_eq!(r.pathname, None);
    }

    #[test]
    fn parse_kernel_pseudo_names() {
        for (line, name) in [
            ("7ffe9f5b3000-7ffe9f5d4000 rw-p 00000000 00:00 0   [stack]", "[stack]"),
            ("7ffe9f5fa000-7ffe9f5fc000 r-xp 00000000 00:00 0   [vdso]", "[vdso]"),
            ("563000000000-563000050000 rw-p 00000000 00:00 0   [heap]", "[heap]"),
        ] {
            let r = parse_line(line).unwrap();
            assert_eq!(r.pathname.as_deref(), Some(name));
        }
    }

    #[test]
    fn parse_pathname_with_spaces() {
        let line =
            "55ed8b3a4000-55ed8b3a8000 r--p 00000000 fd:00 0   /tmp/file with spaces";
        let r = parse_line(line).unwrap();
        assert_eq!(r.pathname.as_deref(), Some("/tmp/file with spaces"));
    }

    #[test]
    fn parse_malformed_lines_error() {
        for bad in [
            "",
            "no-dash here",
            "55ed8b3a4000-55ed8b3a8000 r-xp",
            "55ed8b3a4000-55ed8b3a8000 r-xpQ 00000000 fd:00 0",
            "ZZZZ-AAAA r-xp 0 fd:00 0",
        ] {
            assert!(parse_line(bad).is_err(), "should reject: {bad:?}");
        }
    }

    #[test]
    fn read_self_returns_non_empty_with_an_executable_region() {
        let pid = nix::unistd::getpid();
        let maps = read_proc_maps(pid).expect("read_proc_maps failed");
        assert!(!maps.is_empty(), "self maps should not be empty");
        // We're a Rust binary running under cargo-test; at least
        // one executable region should exist (the test binary).
        assert!(
            maps.iter().any(|r| r.perms.execute),
            "expected at least one executable region in self maps",
        );
    }

    #[test]
    fn read_forked_child_mirrors_parent_layout() {
        // After fork, the child's address space is a COW copy of
        // the parent's. The first few pages should match in
        // count and start addresses (later pages may diverge as
        // the parent allocates, but the child is frozen at
        // SIGSTOP so it can't have moved on).
        let mut mech = LinuxForkSelfMechanism::new();
        let h = mech.take(0).expect("fork failed");
        sleep(Duration::from_millis(50));
        let parent_maps = read_proc_maps(nix::unistd::getpid())
            .expect("parent maps failed");
        let child_maps = read_proc_maps(h.pid).expect("child maps failed");
        // Child should have *at least* something, and its first
        // few file-backed regions should match the parent's.
        // We can't assert exact equality because the parent has
        // been doing test work since fork (so its later anon
        // regions diverge), but the executable + libc text
        // segments share the same start address.
        assert!(!child_maps.is_empty(), "child maps empty");
        let common_starts: Vec<u64> = parent_maps
            .iter()
            .filter_map(|r| {
                r.pathname.as_ref().filter(|_| r.perms.execute).map(|_| r.start)
            })
            .collect();
        let child_executables: Vec<u64> = child_maps
            .iter()
            .filter_map(|r| {
                r.pathname.as_ref().filter(|_| r.perms.execute).map(|_| r.start)
            })
            .collect();
        // Every executable region the child has, the parent has too.
        for s in &child_executables {
            assert!(
                common_starts.contains(s),
                "child executable region 0x{s:x} not in parent's map",
            );
        }

        mech.kill(h).expect("kill failed");
    }
}
