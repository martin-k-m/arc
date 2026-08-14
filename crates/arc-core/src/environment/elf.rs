//! Just enough ELF to answer "what does this binary need at runtime".
//!
//! Parsing the file beats running `ldd`, which executes the dynamic loader and
//! — for a binary Arc did not produce — is a way to run untrusted code while
//! merely inspecting it. Everything here is bounds-checked reads of a byte
//! slice; a malformed file yields `None` or an error, never a panic.

use std::path::Path;

const MAGIC: &[u8; 4] = b"\x7fELF";
const CLASS64: u8 = 2;
const DATA_LE: u8 = 1;

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;

const DT_NULL: i64 = 0;
const DT_NEEDED: i64 = 1;
const DT_STRTAB: i64 = 5;
const DT_SONAME: i64 = 14;
const DT_RPATH: i64 = 15;
const DT_RUNPATH: i64 = 29;

/// What a dynamically linked ELF object asks the loader for.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Needs {
    /// `PT_INTERP`: the absolute path of the dynamic loader, as recorded in the
    /// binary. Not relocatable — see `docs/environments.md`.
    pub interpreter: Option<String>,
    pub soname: Option<String>,
    pub needed: Vec<String>,
    /// `DT_RUNPATH`, else `DT_RPATH`. `$ORIGIN` is left in place; resolution
    /// needs the containing directory, which is the caller's business.
    pub runpath: Vec<String>,
}

/// `None` when the file is not an ELF64 little-endian object, which is not an
/// error: a shell script or a Windows binary simply has no closure to compute.
pub fn needs(path: &Path) -> Option<Needs> {
    let bytes = std::fs::read(path).ok()?;
    parse(&bytes)
}

pub fn is_elf(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && &bytes[..4] == MAGIC
}

pub fn parse(b: &[u8]) -> Option<Needs> {
    if b.len() < 64 || &b[..4] != MAGIC || b[4] != CLASS64 || b[5] != DATA_LE {
        return None;
    }
    let phoff = u64_at(b, 0x20)? as usize;
    let phentsize = u16_at(b, 0x36)? as usize;
    let phnum = u16_at(b, 0x38)? as usize;
    if phentsize < 56 || phnum > 4096 {
        return None;
    }

    let mut out = Needs::default();
    let mut loads: Vec<(u64, u64, u64)> = Vec::new();
    let mut dynamic: Option<(usize, usize)> = None;

    for i in 0..phnum {
        let off = phoff.checked_add(i.checked_mul(phentsize)?)?;
        let ty = u32_at(b, off)?;
        let p_offset = u64_at(b, off.checked_add(0x08)?)?;
        let p_vaddr = u64_at(b, off.checked_add(0x10)?)?;
        let p_filesz = u64_at(b, off.checked_add(0x20)?)?;
        match ty {
            PT_LOAD => loads.push((p_vaddr, p_offset, p_filesz)),
            PT_INTERP => {
                out.interpreter = cstr(b, p_offset as usize, p_filesz as usize);
            }
            PT_DYNAMIC => dynamic = Some((p_offset as usize, p_filesz as usize)),
            _ => {}
        }
    }

    let Some((dyn_off, dyn_len)) = dynamic else {
        return Some(out);
    };
    let entries = read_dynamic(b, dyn_off, dyn_len)?;
    let strtab_vaddr = entries
        .iter()
        .find(|(t, _)| *t == DT_STRTAB)
        .map(|(_, v)| *v)?;
    let strtab = vaddr_to_offset(&loads, strtab_vaddr)? as usize;

    for (tag, val) in &entries {
        match *tag {
            DT_NEEDED => {
                if let Some(s) = strz(b, strtab.checked_add(*val as usize)?) {
                    out.needed.push(s);
                }
            }
            DT_SONAME => out.soname = strz(b, strtab.checked_add(*val as usize)?),
            DT_RUNPATH => {
                if let Some(s) = strz(b, strtab.checked_add(*val as usize)?) {
                    out.runpath = split_paths(&s);
                }
            }
            // RUNPATH wins over RPATH wherever both exist, so a later RPATH
            // entry never displaces one already read.
            DT_RPATH if out.runpath.is_empty() => {
                if let Some(s) = strz(b, strtab.checked_add(*val as usize)?) {
                    out.runpath = split_paths(&s);
                }
            }
            _ => {}
        }
    }
    out.needed.sort();
    out.needed.dedup();
    Some(out)
}

fn read_dynamic(b: &[u8], off: usize, len: usize) -> Option<Vec<(i64, u64)>> {
    let end = off.checked_add(len)?.min(b.len());
    let mut out = Vec::new();
    let mut p = off;
    while p + 16 <= end && out.len() < 4096 {
        let tag = u64_at(b, p)? as i64;
        let val = u64_at(b, p + 8)?;
        if tag == DT_NULL {
            break;
        }
        out.push((tag, val));
        p += 16;
    }
    Some(out)
}

fn vaddr_to_offset(loads: &[(u64, u64, u64)], vaddr: u64) -> Option<u64> {
    loads
        .iter()
        .find(|(v, _, sz)| vaddr >= *v && vaddr < v.saturating_add(*sz))
        .map(|(v, o, _)| o + (vaddr - v))
}

fn split_paths(s: &str) -> Vec<String> {
    s.split(':')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

fn cstr(b: &[u8], off: usize, len: usize) -> Option<String> {
    let end = off.checked_add(len)?.min(b.len());
    let slice = b.get(off..end)?;
    let s = slice.split(|c| *c == 0).next()?;
    Some(String::from_utf8_lossy(s).to_string())
}

fn strz(b: &[u8], off: usize) -> Option<String> {
    let slice = b.get(off..)?;
    let end = slice.iter().position(|c| *c == 0).unwrap_or(slice.len());
    if end == 0 || end > 4096 {
        return None;
    }
    Some(String::from_utf8_lossy(&slice[..end]).to_string())
}

/// Every offset in an ELF file is attacker-controlled, so every read is
/// checked-add then bounds-checked. `off + n` would panic on overflow in debug
/// and silently wrap in release; neither is an acceptable way to reject a
/// malformed file.
fn at<const N: usize>(b: &[u8], off: usize) -> Option<[u8; N]> {
    b.get(off..off.checked_add(N)?)?.try_into().ok()
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(at::<2>(b, off)?))
}
fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(at::<4>(b, off)?))
}
fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(at::<8>(b, off)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_elf_file_is_not_an_error() {
        assert!(parse(b"#!/bin/sh\necho hi\n").is_none());
        assert!(parse(&[]).is_none());
    }

    #[test]
    fn truncated_and_absurd_headers_are_refused_without_panicking() {
        let mut b = vec![0u8; 64];
        b[..4].copy_from_slice(MAGIC);
        b[4] = CLASS64;
        b[5] = DATA_LE;
        // Program headers point far outside the file.
        b[0x20..0x28].copy_from_slice(&u64::MAX.to_le_bytes());
        b[0x36..0x38].copy_from_slice(&56u16.to_le_bytes());
        b[0x38..0x3a].copy_from_slice(&8u16.to_le_bytes());
        assert!(parse(&b).is_none());

        for cut in [1usize, 4, 20, 63] {
            let mut t = b.clone();
            t.truncate(cut);
            assert!(parse(&t).is_none());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_real_dynamic_binary_reports_a_loader_and_libraries() {
        // Arc's own test binary. If it is statically linked there is nothing to
        // assert, and that is a legitimate configuration.
        let me = std::env::current_exe().unwrap();
        let Some(n) = needs(&me) else { return };
        if n.interpreter.is_some() {
            assert!(!n.needed.is_empty(), "{n:?}");
            assert!(n.needed.iter().any(|s| s.starts_with("lib")));
        }
    }

    #[test]
    fn runpath_entries_split_on_colons() {
        assert_eq!(
            split_paths("$ORIGIN/../lib:/opt/x/lib:"),
            vec!["$ORIGIN/../lib".to_string(), "/opt/x/lib".to_string()]
        );
    }
}
