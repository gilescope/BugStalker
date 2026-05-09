// SPDX-License-Identifier: MIT
//! Intel PT instruction decode boundary.
//!
//! Default builds keep this module as typed plumbing only. The actual
//! libipt-backed decoder is compiled only for Linux x86/x86_64 when
//! the crate is built with `--features intel-pt`.

use std::path::{Path, PathBuf};

use crate::PerfError;

#[cfg(all(
    feature = "intel-pt",
    target_os = "linux",
    any(target_arch = "x86", target_arch = "x86_64")
))]
use libipt::enc_dec_builder::{Cpu, PtEncoderDecoder};
#[cfg(all(
    feature = "intel-pt",
    target_os = "linux",
    any(target_arch = "x86", target_arch = "x86_64")
))]
use libipt::error::{PtError, PtErrorCode};
#[cfg(all(
    feature = "intel-pt",
    target_os = "linux",
    any(target_arch = "x86", target_arch = "x86_64")
))]
use libipt::image::Image;
#[cfg(all(
    feature = "intel-pt",
    target_os = "linux",
    any(target_arch = "x86", target_arch = "x86_64")
))]
use libipt::insn::InsnDecoder;

/// CPU identity used by libipt for processor-specific errata.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct IntelPtCpu {
    /// Intel CPU family.
    pub family: u16,
    /// Intel CPU model.
    pub model: u8,
    /// Intel CPU stepping.
    pub stepping: u8,
}

impl IntelPtCpu {
    /// Create an Intel CPU identity.
    #[must_use]
    pub const fn intel(family: u16, model: u8, stepping: u8) -> Self {
        Self {
            family,
            model,
            stepping,
        }
    }
}

/// File-backed executable bytes mapped into the traced process.
///
/// These are the sections libipt consults when reconstructing the
/// instruction stream from packets. The live collector will usually
/// build these from executable `/proc/<pid>/maps` entries.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct IntelPtImageSection {
    filename: PathBuf,
    file_offset: u64,
    size: u64,
    virtual_address: u64,
}

impl IntelPtImageSection {
    /// Create a file-backed image section.
    #[must_use]
    pub fn new(
        filename: impl Into<PathBuf>,
        file_offset: u64,
        size: u64,
        virtual_address: u64,
    ) -> Self {
        Self {
            filename: filename.into(),
            file_offset,
            size,
            virtual_address,
        }
    }

    /// Path of the mapped file.
    #[must_use]
    pub fn filename(&self) -> &Path {
        &self.filename
    }

    /// Offset in the file where this executable mapping begins.
    #[must_use]
    pub const fn file_offset(&self) -> u64 {
        self.file_offset
    }

    /// Number of bytes in this mapping.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// Virtual address where the mapping was loaded.
    #[must_use]
    pub const fn virtual_address(&self) -> u64 {
        self.virtual_address
    }
}

/// Configuration for decoding a captured Intel PT AUX byte window.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct IntelPtDecodeConfig {
    /// Optional display/debug name for the traced image.
    pub image_name: Option<String>,
    /// Optional CPU identity for libipt errata handling.
    pub cpu: Option<IntelPtCpu>,
    /// Executable file mappings available to the traced process.
    pub image_sections: Vec<IntelPtImageSection>,
    /// Optional cap used by callers that want a bounded preview.
    pub max_instructions: Option<usize>,
}

impl IntelPtDecodeConfig {
    /// Create an empty decode configuration.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a file-backed image section.
    #[must_use]
    pub fn with_image_section(mut self, section: IntelPtImageSection) -> Self {
        self.image_sections.push(section);
        self
    }

    /// Set the CPU identity used by libipt.
    #[must_use]
    pub const fn with_cpu(mut self, cpu: IntelPtCpu) -> Self {
        self.cpu = Some(cpu);
        self
    }

    /// Set a maximum number of decoded instructions to return.
    #[must_use]
    pub const fn with_max_instructions(mut self, max_instructions: usize) -> Self {
        self.max_instructions = Some(max_instructions);
        self
    }
}

/// One reconstructed instruction from an Intel PT packet stream.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct DecodedPtInstruction {
    /// Virtual instruction pointer in the traced process.
    pub ip: u64,
    /// Length of the raw instruction bytes reported by libipt.
    pub size: u8,
    /// Whether libipt marked the instruction as speculative.
    pub speculative: bool,
    /// Whether libipt marked the instruction as truncated across image sections.
    pub truncated: bool,
}

/// Decoded Intel PT instruction window.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct DecodedPtTrace {
    /// Instructions reconstructed from the packet stream.
    pub instructions: Vec<DecodedPtInstruction>,
    /// Number of synchronization points consumed while decoding.
    pub sync_points: usize,
    /// Decode errors skipped by resynchronizing forward.
    pub skipped_errors: Vec<String>,
    /// True when `IntelPtDecodeConfig::max_instructions` truncated output.
    pub truncated: bool,
}

/// Decode raw Intel PT AUX bytes into an instruction stream.
///
/// Without `--features intel-pt` on Linux x86/x86_64 this returns
/// [`PerfError::Unsupported`]. The typed config/output remain
/// available in default builds so callers can wire the boundary
/// without pulling in libipt.
pub fn decode_intel_pt_instructions(
    trace: &[u8],
    config: &IntelPtDecodeConfig,
) -> Result<DecodedPtTrace, PerfError> {
    decode_intel_pt_instructions_impl(trace, config)
}

#[cfg(all(
    feature = "intel-pt",
    target_os = "linux",
    any(target_arch = "x86", target_arch = "x86_64")
))]
fn decode_intel_pt_instructions_impl(
    trace: &[u8],
    config: &IntelPtDecodeConfig,
) -> Result<DecodedPtTrace, PerfError> {
    if trace.is_empty() || config.max_instructions == Some(0) {
        return Ok(DecodedPtTrace::default());
    }

    let mut trace_bytes = trace.to_vec();
    let mut image = Image::new(config.image_name.as_deref())
        .map_err(|e| pt_decode_error("image allocation", e))?;
    for section in &config.image_sections {
        image
            .add_file(
                section
                    .filename()
                    .to_str()
                    .ok_or_else(|| PerfError::PtDecode("image path is not UTF-8".to_owned()))?,
                section.file_offset(),
                section.size(),
                None,
                section.virtual_address(),
            )
            .map_err(|e| pt_decode_error("image section", e))?;
    }

    let mut builder = InsnDecoder::builder();
    // SAFETY: `trace_bytes` lives until the decoder is dropped below.
    // libipt treats the trace buffer as input bytes; the Rust wrapper
    // accepts `*mut u8` because the C config uses mutable pointers.
    builder = unsafe { builder.buffer_from_raw(trace_bytes.as_mut_ptr(), trace_bytes.len()) };
    if let Some(cpu) = config.cpu {
        builder = builder.cpu(Cpu::intel(cpu.family, cpu.model, cpu.stepping));
    }

    let mut decoder = builder
        .build()
        .map_err(|e| pt_decode_error("decoder allocation", e))?;
    decoder
        .set_image(Some(&mut image))
        .map_err(|e| pt_decode_error("set image", e))?;

    let mut decoded = DecodedPtTrace::default();
    match decoder.sync_forward() {
        Ok(_) => decoded.sync_points = 1,
        Err(e) if e.code() == PtErrorCode::Eos => return Ok(decoded),
        Err(e) => return Err(pt_decode_error("initial sync", e)),
    }

    loop {
        if config
            .max_instructions
            .is_some_and(|limit| decoded.instructions.len() >= limit)
        {
            decoded.truncated = true;
            return Ok(decoded);
        }

        match decoder.decode_next() {
            Ok((insn, _status)) => decoded.instructions.push(DecodedPtInstruction {
                ip: insn.ip(),
                size: insn.raw().len() as u8,
                speculative: insn.speculative(),
                truncated: insn.truncated(),
            }),
            Err(e) if e.code() == PtErrorCode::Eos => return Ok(decoded),
            Err(e) => {
                decoded.skipped_errors.push(format_pt_error(e));
                match decoder.sync_forward() {
                    Ok(_) => decoded.sync_points += 1,
                    Err(e) if e.code() == PtErrorCode::Eos => return Ok(decoded),
                    Err(e) => return Err(pt_decode_error("resync", e)),
                }
            }
        }
    }
}

#[cfg(not(all(
    feature = "intel-pt",
    target_os = "linux",
    any(target_arch = "x86", target_arch = "x86_64")
)))]
fn decode_intel_pt_instructions_impl(
    _trace: &[u8],
    _config: &IntelPtDecodeConfig,
) -> Result<DecodedPtTrace, PerfError> {
    Err(PerfError::Unsupported)
}

#[cfg(all(
    feature = "intel-pt",
    target_os = "linux",
    any(target_arch = "x86", target_arch = "x86_64")
))]
fn pt_decode_error(stage: &'static str, err: PtError) -> PerfError {
    PerfError::PtDecode(format!("{stage}: {}", format_pt_error(err)))
}

#[cfg(all(
    feature = "intel-pt",
    target_os = "linux",
    any(target_arch = "x86", target_arch = "x86_64")
))]
fn format_pt_error(err: PtError) -> String {
    format!("{} ({:?})", err, err.code())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_section_accessors_round_trip() {
        let section = IntelPtImageSection::new("/bin/example", 0x1000, 0x2000, 0x400000);

        assert_eq!(section.filename(), Path::new("/bin/example"));
        assert_eq!(section.file_offset(), 0x1000);
        assert_eq!(section.size(), 0x2000);
        assert_eq!(section.virtual_address(), 0x400000);
    }

    #[test]
    fn decode_config_builders_preserve_fields() {
        let config = IntelPtDecodeConfig::new()
            .with_cpu(IntelPtCpu::intel(6, 0x9e, 11))
            .with_image_section(IntelPtImageSection::new("/bin/example", 0, 4096, 0x400000))
            .with_max_instructions(32);

        assert_eq!(config.cpu, Some(IntelPtCpu::intel(6, 0x9e, 11)));
        assert_eq!(config.image_sections.len(), 1);
        assert_eq!(config.max_instructions, Some(32));
    }

    #[cfg(not(all(
        feature = "intel-pt",
        target_os = "linux",
        any(target_arch = "x86", target_arch = "x86_64")
    )))]
    #[test]
    fn decode_returns_unsupported_without_matching_feature_target() {
        let err = decode_intel_pt_instructions(&[0x02, 0x82], &IntelPtDecodeConfig::new())
            .expect_err("decoder should be unavailable");

        assert!(matches!(err, PerfError::Unsupported));
    }

    #[cfg(all(
        feature = "intel-pt",
        target_os = "linux",
        any(target_arch = "x86", target_arch = "x86_64")
    ))]
    #[test]
    fn empty_trace_decodes_empty() {
        let decoded = decode_intel_pt_instructions(&[], &IntelPtDecodeConfig::new())
            .expect("empty trace should decode without touching libipt");

        assert!(decoded.instructions.is_empty());
        assert_eq!(decoded.sync_points, 0);
    }
}
