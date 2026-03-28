// NOTE: This program is meant to be compiled with cargo-build-sbf.
// It won't compile as a regular Rust library without the Solana SDK.
// The source is here for reference and will be compiled in the M1 Nix shell.

// use solana_program_entrypoint::entrypoint;
// use solana_account_info::AccountInfo;
// use solana_program_error::ProgramResult;
// use solana_pubkey::Pubkey;
//
// entrypoint!(process_instruction);
//
// fn process_instruction(
//     _program_id: &Pubkey,
//     _accounts: &[AccountInfo],
//     _instruction_data: &[u8],
// ) -> ProgramResult {
//     let a: u64 = 10;
//     let b: u64 = 32;
//     let sum_val: u64 = a + b;
//     let doubled: u64 = sum_val * 2;
//     let final_result: u64 = doubled + a;
//     solana_msg::msg!("result: {}", final_result);
//     Ok(())
// }
