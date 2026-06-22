//! Post-link fixup: copy `component-type:*` custom sections
//! from a pre-link wasm into a post-link wasm AND add an
//! export alias `(export "memory" (memory 0))` so
//! `wasm-tools component new` accepts the merged module.
//!
//! Usage:
//!   postlink-fixup <pre-link.wasm> <post-link.wasm> <out.wasm>

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{anyhow, Context, Result};
use wasm_encoder::{CustomSection, ExportKind, ExportSection, Module, RawSection, Section};
use wasmparser::{Parser, Payload};

/// Custom-section names with these prefixes get forwarded from
/// the pre-link wasm. We deliberately do NOT forward `name`,
/// `producers`, etc. — those reference function indices that the
/// linker has renumbered.
const FORWARD_PREFIXES: &[&str] = &["component-type:", "target_features"];

/// Name `wasm-tools component new` expects for the component's
/// default linear memory.
const MEMORY_ALIAS_NAME: &str = "memory";

/// Pool index that doubles as the workload's default memory in
/// the tvm-guest-mm shell.
const DEFAULT_MEMORY_INDEX: u32 = 0;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("postlink-fixup: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 4 {
        return Err(anyhow!(
            "usage: postlink-fixup <pre-link.wasm> <post-link.wasm> <out.wasm>"
        ));
    }
    let pre_path = PathBuf::from(&args[1]);
    let post_path = PathBuf::from(&args[2]);
    let out_path = PathBuf::from(&args[3]);

    let pre_bytes =
        fs::read(&pre_path).with_context(|| format!("read pre-link wasm {}", pre_path.display()))?;
    let post_bytes = fs::read(&post_path)
        .with_context(|| format!("read post-link wasm {}", post_path.display()))?;

    let forwarded = collect_custom_sections(&pre_bytes)?;
    if forwarded.is_empty() {
        return Err(anyhow!(
            "no forwardable custom sections found in {}; expected at least one \
             `component-type:*` section emitted by wit-bindgen",
            pre_path.display()
        ));
    }

    let merged = rewrite_post_link(&post_bytes, &forwarded)?;
    fs::write(&out_path, &merged)
        .with_context(|| format!("write out wasm {}", out_path.display()))?;

    eprintln!(
        "postlink-fixup: wrote {} ({} bytes, +{} custom section(s), +1 memory alias `{}`)",
        out_path.display(),
        merged.len(),
        forwarded.len(),
        MEMORY_ALIAS_NAME,
    );
    for (name, payload) in &forwarded {
        eprintln!("  + {name} ({} bytes)", payload.len());
    }
    Ok(())
}

fn collect_custom_sections(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    for payload in Parser::new(0).parse_all(bytes) {
        if let Payload::CustomSection(reader) = payload? {
            let name = reader.name();
            if FORWARD_PREFIXES.iter().any(|p| name.starts_with(p)) {
                out.push((name.to_string(), reader.data().to_vec()));
            }
        }
    }
    Ok(out)
}

/// Walk every section of the post-link module; pass non-export
/// sections through verbatim; rebuild the export section to
/// append `memory` alias; append forwarded custom sections at
/// end.
fn rewrite_post_link(
    bytes: &[u8],
    forwarded: &[(String, Vec<u8>)],
) -> Result<Vec<u8>> {
    let mut module = Module::new();
    let mut wrote_export_section = false;

    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload?;
        match payload {
            Payload::Version { .. } | Payload::End(_) => {
                // wasm-encoder's Module handles the version
                // prologue / end implicitly.
            }
            Payload::ExportSection(reader) => {
                let mut exports = ExportSection::new();
                let mut saw_memory_alias = false;
                for export in reader {
                    let export = export?;
                    let kind = match export.kind {
                        wasmparser::ExternalKind::Func => ExportKind::Func,
                        wasmparser::ExternalKind::Table => ExportKind::Table,
                        wasmparser::ExternalKind::Memory => ExportKind::Memory,
                        wasmparser::ExternalKind::Global => ExportKind::Global,
                        wasmparser::ExternalKind::Tag => ExportKind::Tag,
                    };
                    exports.export(export.name, kind, export.index);
                    if export.name == MEMORY_ALIAS_NAME
                        && export.kind == wasmparser::ExternalKind::Memory
                    {
                        saw_memory_alias = true;
                    }
                }
                if !saw_memory_alias {
                    exports.export(
                        MEMORY_ALIAS_NAME,
                        ExportKind::Memory,
                        DEFAULT_MEMORY_INDEX,
                    );
                }
                module.section(&exports);
                wrote_export_section = true;
            }
            // Pass every other section through raw — we don't
            // touch type/func/code/etc.
            other => {
                if let Some((id, range)) = section_raw_passthrough(&other) {
                    let body = &bytes[range];
                    module.section(&RawSection { id, data: body });
                }
            }
        }
    }

    if !wrote_export_section {
        // No export section at all? Synthesize one carrying just
        // the memory alias.
        let mut exports = ExportSection::new();
        exports.export(MEMORY_ALIAS_NAME, ExportKind::Memory, DEFAULT_MEMORY_INDEX);
        module.section(&exports);
    }

    // Append forwarded custom sections at the end.
    let mut out = module.finish();
    for (name, payload) in forwarded {
        let section = CustomSection {
            name: name.as_str().into(),
            data: payload.as_slice().into(),
        };
        section.append_to(&mut out);
    }

    // Re-parse to catch any corruption we introduced.
    for payload in Parser::new(0).parse_all(&out) {
        payload.context("validate merged wasm")?;
    }

    Ok(out)
}

/// Returns `(section_id, body_range)` for sections we want to
/// pass through verbatim. Skips synthetic payloads (version,
/// end, custom sections — custom sections are dropped here and
/// re-added by the caller via `forwarded`).
fn section_raw_passthrough(payload: &Payload<'_>) -> Option<(u8, std::ops::Range<usize>)> {
    use wasmparser::Payload::*;
    match payload {
        TypeSection(r) => Some((1, r.range())),
        ImportSection(r) => Some((2, r.range())),
        FunctionSection(r) => Some((3, r.range())),
        TableSection(r) => Some((4, r.range())),
        MemorySection(r) => Some((5, r.range())),
        GlobalSection(r) => Some((6, r.range())),
        // ExportSection: handled above (we rebuild it).
        StartSection { range, .. } => Some((8, range.clone())),
        ElementSection(r) => Some((9, r.range())),
        DataCountSection { range, .. } => Some((12, range.clone())),
        CodeSectionStart { range, .. } => Some((10, range.clone())),
        DataSection(r) => Some((11, r.range())),
        TagSection(r) => Some((13, r.range())),
        _ => None,
    }
}
