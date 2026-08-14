//! The cBPF program that decides which syscalls Arc must see.
//!
//! The rule is the inverse of what a sandbox would write. Arc does not
//! enumerate what to trap; it enumerates what is *provably irrelevant* — the
//! same list the ptrace backend uses — and traps everything else. A syscall a
//! future kernel adds is therefore trapped by default and downgrades the trace,
//! rather than being silently allowed past an incomplete allowlist.
//!
//! Performance follows from the same choice: the overwhelming majority of
//! syscalls a build issues are descriptor I/O, memory and futex operations,
//! which are on the irrelevant list and never leave the kernel's BPF
//! evaluation.

use super::sys::*;
use crate::trace::linux::syscalls;

const BPF_LD: u16 = 0x00;
const BPF_JMP: u16 = 0x05;
const BPF_RET: u16 = 0x06;
const BPF_W: u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_JEQ: u16 = 0x10;
const BPF_JA: u16 = 0x00;
const BPF_K: u16 = 0x00;

const ARCH_OFFSET: u32 = 4;
const NR_OFFSET: u32 = 0;

/// cBPF conditional jumps carry 8-bit displacements, so the equality chain is
/// emitted in blocks small enough that every jump inside one fits.
const BLOCK: usize = 200;

fn stmt(code: u16, k: u32) -> sock_filter {
    sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

fn jump(code: u16, k: u32, jt: u8, jf: u8) -> sock_filter {
    sock_filter { code, jt, jf, k }
}

/// Highest syscall number Arc has an opinion about. Numbers beyond it are, by
/// definition, ones this build has never heard of.
fn highest_known() -> u32 {
    (0..1024)
        .filter(|nr| syscalls::decode(*nr).is_some() || syscalls::is_irrelevant(*nr))
        .max()
        .unwrap_or(0) as u32
}

/// Every syscall number that cannot affect what an execution depends on.
pub fn allowed() -> Vec<u32> {
    let mut v: Vec<u32> = (0..=highest_known() as i64)
        .filter(|nr| syscalls::is_irrelevant(*nr))
        .map(|nr| nr as u32)
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Build the program: notify unless the syscall is on the irrelevant list *and*
/// came from the native architecture.
pub fn program() -> Vec<sock_filter> {
    let allow_list = allowed();
    let blocks: Vec<&[u32]> = allow_list.chunks(BLOCK).collect();

    // Instruction count, so the unconditional jumps can be resolved:
    // arch load, arch compare, the non-native escape, the nr load, then each
    // block plus its two trampolines, then notify and allow.
    let prologue = 4usize;
    let body: usize = blocks.iter().map(|b| b.len() + 2).sum();
    let notify_at = prologue + body;
    let allow_at = notify_at + 1;

    let mut prog = Vec::with_capacity(allow_at + 1);
    prog.push(stmt(BPF_LD | BPF_W | BPF_ABS, ARCH_OFFSET));
    // A non-native architecture means the numbers below mean something else, so
    // every such syscall is trapped and reported as unmodelled.
    prog.push(jump(BPF_JMP | BPF_JEQ | BPF_K, NATIVE_ARCH, 1, 0));
    prog.push(stmt(BPF_JMP | BPF_JA | BPF_K, (notify_at - 2 - 1) as u32));
    prog.push(stmt(BPF_LD | BPF_W | BPF_ABS, NR_OFFSET));

    let mut at = prog.len();
    for block in &blocks {
        let n = block.len();
        for (i, nr) in block.iter().enumerate() {
            prog.push(jump(BPF_JMP | BPF_JEQ | BPF_K, *nr, (n - i) as u8, 0));
        }
        // Not in this block: skip the trampoline and try the next one.
        let after_trampoline = at + n + 2;
        prog.push(stmt(BPF_JMP | BPF_JA | BPF_K, 1));
        // Matched: allowed.
        prog.push(stmt(
            BPF_JMP | BPF_JA | BPF_K,
            (allow_at - (at + n + 1) - 1) as u32,
        ));
        at = after_trampoline;
    }
    prog.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_USER_NOTIF));
    prog.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW));
    debug_assert_eq!(prog.len(), allow_at + 1);
    prog
}

/// A one-syscall program, for the availability probe: notify on `getpid` and
/// allow everything else. Deliberately not the real filter — the probe is about
/// whether the *mechanism* works, and a probe that traps hundreds of syscalls
/// would be both slower and harder to reason about.
pub fn probe_program(nr: u32) -> Vec<sock_filter> {
    vec![
        stmt(BPF_LD | BPF_W | BPF_ABS, ARCH_OFFSET),
        jump(BPF_JMP | BPF_JEQ | BPF_K, NATIVE_ARCH, 0, 3),
        stmt(BPF_LD | BPF_W | BPF_ABS, NR_OFFSET),
        jump(BPF_JMP | BPF_JEQ | BPF_K, nr, 0, 1),
        stmt(BPF_RET | BPF_K, SECCOMP_RET_USER_NOTIF),
        stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal cBPF interpreter, enough to prove the generated program
    /// classifies every syscall the way the syscall table says it should.
    fn run(prog: &[sock_filter], arch: u32, nr: u32) -> u32 {
        let mut pc = 0usize;
        let mut a: u32 = 0;
        for _ in 0..100_000 {
            let i = prog[pc];
            pc += 1;
            match i.code {
                c if c == BPF_LD | BPF_W | BPF_ABS => {
                    a = match i.k {
                        NR_OFFSET => nr,
                        ARCH_OFFSET => arch,
                        _ => panic!("unexpected load offset {}", i.k),
                    }
                }
                c if c == BPF_JMP | BPF_JEQ | BPF_K => {
                    pc += if a == i.k {
                        i.jt as usize
                    } else {
                        i.jf as usize
                    }
                }
                c if c == BPF_JMP | BPF_JA | BPF_K => pc += i.k as usize,
                c if c == BPF_RET | BPF_K => return i.k,
                other => panic!("unhandled opcode {other:#x}"),
            }
        }
        panic!("program did not terminate");
    }

    #[test]
    fn irrelevant_syscalls_are_allowed_and_everything_else_is_observed() {
        let prog = program();
        for nr in 0i64..512 {
            let expect = if syscalls::is_irrelevant(nr) {
                SECCOMP_RET_ALLOW
            } else {
                SECCOMP_RET_USER_NOTIF
            };
            assert_eq!(
                run(&prog, NATIVE_ARCH, nr as u32),
                expect,
                "syscall {nr} classified wrongly"
            );
        }
    }

    #[test]
    fn every_modelled_syscall_is_observed() {
        let prog = program();
        for nr in 0i64..512 {
            if syscalls::decode(nr).is_some() {
                assert_eq!(
                    run(&prog, NATIVE_ARCH, nr as u32),
                    SECCOMP_RET_USER_NOTIF,
                    "modelled syscall {nr} would not be seen"
                );
            }
        }
    }

    #[test]
    fn a_syscall_from_the_future_is_observed_rather_than_allowed() {
        let prog = program();
        for nr in [900u32, 1000, 4095, u32::MAX] {
            assert_eq!(run(&prog, NATIVE_ARCH, nr), SECCOMP_RET_USER_NOTIF);
        }
    }

    #[test]
    fn a_foreign_architecture_is_observed_whatever_the_number() {
        let prog = program();
        // The 32-bit x86 token. Its numbers mean different syscalls, so none of
        // the equality tests below may be reached.
        for nr in [0u32, 1, 2, 60, 257] {
            assert_eq!(run(&prog, 0x4000_0003, nr), SECCOMP_RET_USER_NOTIF);
        }
    }

    #[test]
    fn the_program_is_within_the_kernels_instruction_limit() {
        // BPF_MAXINSNS for seccomp is 4096.
        assert!(program().len() < 4096, "{}", program().len());
    }

    #[test]
    fn the_probe_program_traps_only_its_one_syscall() {
        let prog = probe_program(39);
        assert_eq!(run(&prog, NATIVE_ARCH, 39), SECCOMP_RET_USER_NOTIF);
        assert_eq!(run(&prog, NATIVE_ARCH, 40), SECCOMP_RET_ALLOW);
        assert_eq!(run(&prog, 0x4000_0003, 39), SECCOMP_RET_ALLOW);
    }
}
