// Minimal Solana test program for SBF compilation.
//
// This program is compiled with `cargo-build-sbf` to produce an ELF
// with DWARF debug info. The resulting .so file is used by the
// recorder's DWARF parsing tests.
//
// When the Solana SDK is available (via cargo-build-sbf), this compiles
// as a real BPF program. For unit-test purposes, we keep a cfg-gated
// fallback that compiles as a regular Rust lib.

#[cfg(target_os = "solana")]
use solana_program::{
    account_info::AccountInfo, entrypoint, entrypoint::ProgramResult, msg, pubkey::Pubkey,
};

#[cfg(target_os = "solana")]
entrypoint!(process_instruction);

#[cfg(target_os = "solana")]
fn process_instruction(
    _program_id: &Pubkey,
    _accounts: &[AccountInfo],
    _instruction_data: &[u8],
) -> ProgramResult {
    let a: u64 = 10;
    let b: u64 = 32;
    let sum_val: u64 = a + b;
    let doubled: u64 = sum_val * 2;
    let final_result: u64 = doubled + a;
    msg!("result: {}", final_result);
    Ok(())
}

// When not compiling for Solana/SBF, expose a plain Rust function
// so the file compiles as a regular library (used in host-side tests).
#[cfg(not(target_os = "solana"))]
pub fn process_instruction_native() -> u64 {
    let a: u64 = 10;
    let b: u64 = 32;
    let sum_val: u64 = a + b;
    let doubled: u64 = sum_val * 2;
    let final_result: u64 = doubled + a;
    final_result
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_native_arithmetic() {
        let result = super::process_instruction_native();
        assert_eq!(result, 94, "10 + 32 = 42, 42*2 = 84, 84+10 = 94");
    }
}
