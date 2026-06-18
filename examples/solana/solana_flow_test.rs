// Solana flow test program (comprehensive language construct coverage)
//
// This is the source file used for trace display; the actual SBF execution is
// handled by the recorder with register tracing. The file exercises a wide
// range of Rust language constructs that the debugger needs to handle correctly
// when displaying variable values, stepping through code, and resolving types.
//
// It intentionally avoids Solana SDK imports (solana_program, etc.) since those
// require the full Solana build environment. All constructs here are pure Rust
// that the Solana recorder can compile and trace.

// ---------------------------------------------------------------------------
// Structs
// ---------------------------------------------------------------------------

/// A simple account-like struct with multiple typed fields.
struct AccountInfo {
    balance: u64,
    owner: u32,
    is_signer: bool,
    data_len: usize,
}

/// A ledger entry combining nested struct data with a tag.
struct LedgerEntry {
    id: u32,
    account: AccountInfo,
    memo: [u8; 8],
}

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Instruction discriminator, mimicking a typical Solana program instruction set.
enum Instruction {
    Transfer { amount: u64, fee: u64 },
    Initialize(u32),
    Close,
}

/// Result-like enum for internal error handling.
enum ProcessResult {
    Success(u64),
    InsufficientFunds,
    InvalidOwner(u32),
}

// ---------------------------------------------------------------------------
// Pure arithmetic helpers
// ---------------------------------------------------------------------------

/// Adds two u64 values. Serves as a simple call target for nested-call tests.
fn add_u64(a: u64, b: u64) -> u64 {
    a + b
}

/// Multiplies two u64 values.
fn mul_u64(a: u64, b: u64) -> u64 {
    a * b
}

/// Performs saturating subtraction (returns 0 instead of underflowing).
fn saturating_sub(a: u64, b: u64) -> u64 {
    if a >= b {
        a - b
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// Bitwise operations
// ---------------------------------------------------------------------------

/// Demonstrates bitwise AND, OR, XOR, and shifts on u64 values.
fn bitwise_ops(x: u64, y: u64) -> (u64, u64, u64, u64, u64) {
    let and_result = x & y;
    let or_result = x | y;
    let xor_result = x ^ y;
    let shl_result = x << 2;
    let shr_result = y >> 1;
    (and_result, or_result, xor_result, shl_result, shr_result)
}

// ---------------------------------------------------------------------------
// Multi-width integer arithmetic
// ---------------------------------------------------------------------------

/// Exercises u8, u16, u32, and u64 arithmetic to verify the debugger handles
/// different integer widths correctly.
fn multi_width_arithmetic() -> u64 {
    let a_u8: u8 = 200;
    let b_u8: u8 = 55;
    let sum_u8: u8 = a_u8.wrapping_add(b_u8); // 255 (wraps on overflow)

    let a_u16: u16 = 40_000;
    let b_u16: u16 = 25_535;
    let sum_u16: u16 = a_u16.wrapping_add(b_u16); // 65535

    let a_u32: u32 = 3_000_000_000;
    let b_u32: u32 = 1_000_000_000;
    let sum_u32: u32 = a_u32.wrapping_add(b_u32); // 4000000000

    let a_u64: u64 = 10_000_000_000;
    let b_u64: u64 = 5_000_000_000;
    let sum_u64: u64 = a_u64 + b_u64; // 15000000000

    // Combine all widths into a single u64 checksum.
    (sum_u8 as u64) + (sum_u16 as u64) + (sum_u32 as u64) + sum_u64
    // = 255 + 65535 + 4000000000 + 15000000000 = 19000065790
}

// ---------------------------------------------------------------------------
// Struct operations
// ---------------------------------------------------------------------------

/// Creates an AccountInfo struct and performs field-level operations.
fn create_account(balance: u64, owner: u32) -> AccountInfo {
    AccountInfo {
        balance,
        owner,
        is_signer: balance > 0,
        data_len: 128,
    }
}

/// Reads and manipulates struct fields, returning a derived value.
fn process_account(account: &AccountInfo) -> u64 {
    let base = account.balance;
    let owner_bonus = account.owner as u64 * 10;
    let signer_bonus: u64 = if account.is_signer { 100 } else { 0 };
    let total = base + owner_bonus + signer_bonus;
    total
}

// ---------------------------------------------------------------------------
// Enum + pattern matching
// ---------------------------------------------------------------------------

/// Processes an instruction using exhaustive pattern matching.
fn execute_instruction(instruction: &Instruction, balance: u64) -> ProcessResult {
    match instruction {
        Instruction::Transfer { amount, fee } => {
            let total_debit = amount + fee;
            if balance >= total_debit {
                ProcessResult::Success(balance - total_debit)
            } else {
                ProcessResult::InsufficientFunds
            }
        }
        Instruction::Initialize(owner_id) => {
            if *owner_id == 0 {
                ProcessResult::InvalidOwner(*owner_id)
            } else {
                ProcessResult::Success(balance)
            }
        }
        Instruction::Close => ProcessResult::Success(0),
    }
}

/// Extracts the numeric result from ProcessResult, or returns a default.
fn unwrap_result(result: &ProcessResult, default: u64) -> u64 {
    match result {
        ProcessResult::Success(value) => *value,
        ProcessResult::InsufficientFunds => default,
        ProcessResult::InvalidOwner(_owner) => default,
    }
}

// ---------------------------------------------------------------------------
// Option and Result types
// ---------------------------------------------------------------------------

/// Searches for a value in a slice, returning Some(index) or None.
fn find_value(data: &[u64], target: u64) -> Option<usize> {
    let mut i = 0;
    while i < data.len() {
        if data[i] == target {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Demonstrates if-let pattern with Option.
fn option_handling(values: &[u64], target: u64) -> u64 {
    if let Some(index) = find_value(values, target) {
        (index as u64) * 100 + target
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// Loops: for, while, loop/break
// ---------------------------------------------------------------------------

/// Sums elements using a while loop with index tracking.
fn sum_while(data: &[u64]) -> u64 {
    let mut total: u64 = 0;
    let mut index: usize = 0;
    while index < data.len() {
        total += data[index];
        index += 1;
    }
    total
}

/// Sums elements using a for loop over an iterator.
fn sum_for(data: &[u64]) -> u64 {
    let mut total: u64 = 0;
    for value in data.iter() {
        total += value;
    }
    total
}

/// Finds the first element greater than a threshold using loop/break.
fn find_first_above(data: &[u64], threshold: u64) -> u64 {
    let mut index: usize = 0;
    let result: u64;
    loop {
        if index >= data.len() {
            result = 0;
            break;
        }
        if data[index] > threshold {
            result = data[index];
            break;
        }
        index += 1;
    }
    result
}

// ---------------------------------------------------------------------------
// Array and tuple operations
// ---------------------------------------------------------------------------

/// Creates and manipulates a fixed-size array, returning summary values.
fn array_operations() -> (u64, u64, u64) {
    let mut arr: [u64; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

    // In-place mutation
    arr[0] = 10;
    arr[7] = 80;

    // Compute min, max, sum over the array
    let mut min_val = arr[0];
    let mut max_val = arr[0];
    let mut sum: u64 = 0;
    let mut i = 0;
    while i < 8 {
        if arr[i] < min_val {
            min_val = arr[i];
        }
        if arr[i] > max_val {
            max_val = arr[i];
        }
        sum += arr[i];
        i += 1;
    }
    // arr = [10, 2, 3, 4, 5, 6, 7, 80]
    // min = 2, max = 80, sum = 117
    (min_val, max_val, sum)
}

/// Demonstrates tuple creation, destructuring, and element access.
fn tuple_operations() -> u64 {
    let coords: (u64, u64, u64) = (100, 200, 300);
    let (x, y, z) = coords;
    let magnitude = x + y + z; // 600

    let pair = (magnitude, magnitude * 2);
    let combined = pair.0 + pair.1; // 600 + 1200 = 1800
    combined
}

// ---------------------------------------------------------------------------
// Byte array manipulation (common in Solana programs)
// ---------------------------------------------------------------------------

/// Demonstrates byte-level operations typical of Solana data serialization.
fn byte_manipulation() -> u64 {
    // Simulate encoding a u64 into a byte array (little-endian)
    let value: u64 = 0x0102030405060708;
    let mut bytes: [u8; 8] = [0; 8];
    bytes[0] = (value & 0xFF) as u8;
    bytes[1] = ((value >> 8) & 0xFF) as u8;
    bytes[2] = ((value >> 16) & 0xFF) as u8;
    bytes[3] = ((value >> 24) & 0xFF) as u8;
    bytes[4] = ((value >> 32) & 0xFF) as u8;
    bytes[5] = ((value >> 40) & 0xFF) as u8;
    bytes[6] = ((value >> 48) & 0xFF) as u8;
    bytes[7] = ((value >> 56) & 0xFF) as u8;

    // Decode back from bytes (little-endian)
    let decoded: u64 = (bytes[0] as u64)
        | ((bytes[1] as u64) << 8)
        | ((bytes[2] as u64) << 16)
        | ((bytes[3] as u64) << 24)
        | ((bytes[4] as u64) << 32)
        | ((bytes[5] as u64) << 40)
        | ((bytes[6] as u64) << 48)
        | ((bytes[7] as u64) << 56);

    // Build a memo-style byte array
    let memo: [u8; 8] = [b'C', b'o', b'd', b'e', b'T', b'r', b'c', b'r'];
    let mut memo_sum: u64 = 0;
    let mut j = 0;
    while j < 8 {
        memo_sum += memo[j] as u64;
        j += 1;
    }

    // decoded should equal value (round-trip check)
    decoded + memo_sum
}

// ---------------------------------------------------------------------------
// Recursive computation
// ---------------------------------------------------------------------------

/// Computes the n-th Fibonacci number recursively.
fn fibonacci(n: u64) -> u64 {
    if n <= 1 {
        return n;
    }
    fibonacci(n - 1) + fibonacci(n - 2)
}

/// Iterative Fibonacci for comparison and loop testing.
fn fibonacci_iterative(n: u64) -> u64 {
    if n <= 1 {
        return n;
    }
    let mut prev: u64 = 0;
    let mut curr: u64 = 1;
    let mut i: u64 = 2;
    while i <= n {
        let next = prev + curr;
        prev = curr;
        curr = next;
        i += 1;
    }
    curr
}

// ---------------------------------------------------------------------------
// Nested function calls
// ---------------------------------------------------------------------------

/// Chains multiple helper functions to test nested call tracing.
fn nested_computation(base: u64, multiplier: u64, offset: u64) -> u64 {
    let step1 = add_u64(base, offset);       // base + offset
    let step2 = mul_u64(step1, multiplier);   // (base + offset) * multiplier
    let step3 = saturating_sub(step2, base);  // step2 - base
    let step4 = add_u64(step3, step3);        // step3 * 2
    step4
}

/// Double nesting: calls nested_computation and combines results.
fn deeply_nested(a: u64, b: u64) -> u64 {
    let left = nested_computation(a, 2, b);
    let right = nested_computation(b, 3, a);
    add_u64(left, right)
}

// ---------------------------------------------------------------------------
// References and borrowing
// ---------------------------------------------------------------------------

/// Takes a mutable reference and modifies the value in place.
fn double_in_place(value: &mut u64) {
    *value *= 2;
}

/// Demonstrates borrowing patterns: shared refs, mutable refs, and re-borrowing.
fn borrowing_demo() -> u64 {
    let mut x: u64 = 50;

    // Shared (immutable) borrow
    let x_ref: &u64 = &x;
    let snapshot = *x_ref;  // 50

    // Mutable borrow (x_ref no longer used after this point)
    double_in_place(&mut x);    // x = 100
    double_in_place(&mut x);    // x = 200

    let final_val = x + snapshot;  // 200 + 50 = 250
    final_val
}

// ---------------------------------------------------------------------------
// Iterator patterns
// ---------------------------------------------------------------------------

/// Demonstrates iterator chaining: filter, map, and fold.
fn iterator_demo() -> u64 {
    let data: [u64; 10] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];

    // Sum of squares of even numbers: 4 + 16 + 36 + 64 + 100 = 220
    let sum_of_even_squares: u64 = data
        .iter()
        .filter(|&&v| v % 2 == 0)
        .map(|&v| v * v)
        .fold(0u64, |acc, v| acc + v);

    // Count of elements above 5: 5 (6,7,8,9,10)
    let count_above_5: u64 = data.iter().filter(|&&v| v > 5).count() as u64;

    sum_of_even_squares + count_above_5 // 220 + 5 = 225
}

// ---------------------------------------------------------------------------
// LedgerEntry with nested struct and byte array
// ---------------------------------------------------------------------------

/// Creates a LedgerEntry with nested AccountInfo and a memo byte array.
fn create_ledger_entry(id: u32, balance: u64) -> LedgerEntry {
    let account = create_account(balance, id);
    let memo: [u8; 8] = [
        (id & 0xFF) as u8,
        ((id >> 8) & 0xFF) as u8,
        0, 0, 0, 0, 0, 0,
    ];
    LedgerEntry { id, account, memo }
}

/// Processes a ledger entry, combining account processing with memo data.
fn process_ledger_entry(entry: &LedgerEntry) -> u64 {
    let account_value = process_account(&entry.account);
    let mut memo_sum: u64 = 0;
    let mut k = 0;
    while k < 8 {
        memo_sum += entry.memo[k] as u64;
        k += 1;
    }
    account_value + memo_sum + entry.id as u64
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// Primary entry point exercising all language constructs.
/// Each section is separated by comments to provide clear breakpoint targets
/// where multiple variables are in scope for debugger inspection.
fn process_instruction() {
    // === Section 1: Basic arithmetic ===
    let a: u64 = 10;
    let b: u64 = 32;
    let sum_val: u64 = a + b;           // 42
    let doubled: u64 = sum_val * 2;     // 84
    let final_result: u64 = doubled + a; // 94
    // BREAKPOINT TARGET: line above - a, b, sum_val, doubled, final_result in scope

    // === Section 2: Struct creation and field access ===
    let account = create_account(1000, 42);
    let account_total = process_account(&account);
    // account_total = 1000 + (42 * 10) + 100 = 1520
    let struct_check: u64 = account_total;
    // BREAKPOINT TARGET: line above - account, account_total, struct_check in scope

    // === Section 3: Enum pattern matching ===
    let transfer_instr = Instruction::Transfer { amount: 500, fee: 10 };
    let transfer_result = execute_instruction(&transfer_instr, 1000);
    let transfer_value = unwrap_result(&transfer_result, 0);
    // transfer_value = 1000 - 500 - 10 = 490
    let enum_check: u64 = transfer_value;
    // BREAKPOINT TARGET: line above - transfer_value, enum_check in scope

    // Test insufficient funds path
    let big_transfer = Instruction::Transfer { amount: 2000, fee: 50 };
    let fail_result = execute_instruction(&big_transfer, 100);
    let fail_value = unwrap_result(&fail_result, 999);
    // fail_value = 999 (default, since InsufficientFunds)

    // Test Initialize and Close variants
    let init_instr = Instruction::Initialize(7);
    let init_result = execute_instruction(&init_instr, 500);
    let init_value = unwrap_result(&init_result, 0);
    // init_value = 500

    let close_instr = Instruction::Close;
    let close_result = execute_instruction(&close_instr, 500);
    let close_value = unwrap_result(&close_result, 0);
    // close_value = 0

    // === Section 4: Loops ===
    let loop_data: [u64; 6] = [10, 20, 30, 40, 50, 60];
    let while_sum = sum_while(&loop_data);       // 210
    let for_sum = sum_for(&loop_data);           // 210
    let first_above = find_first_above(&loop_data, 35); // 40
    let loop_check: u64 = while_sum + for_sum + first_above;
    // loop_check = 210 + 210 + 40 = 460
    // BREAKPOINT TARGET: line above - while_sum, for_sum, first_above, loop_check in scope

    // === Section 5: Nested function calls ===
    let nested_val = nested_computation(10, 3, 5);
    // step1 = 10 + 5 = 15
    // step2 = 15 * 3 = 45
    // step3 = 45 - 10 = 35
    // step4 = 35 + 35 = 70
    let deep_val = deeply_nested(5, 10);
    // left = nested_computation(5, 2, 10):
    //   step1=15, step2=30, step3=25, step4=50
    // right = nested_computation(10, 3, 5):
    //   step1=15, step2=45, step3=35, step4=70
    // deep_val = 50 + 70 = 120
    let nested_check: u64 = nested_val + deep_val;
    // nested_check = 70 + 120 = 190
    // BREAKPOINT TARGET: line above - nested_val, deep_val, nested_check in scope

    // === Section 6: Array and tuple operations ===
    let (arr_min, arr_max, arr_sum) = array_operations();
    // arr_min = 2, arr_max = 80, arr_sum = 117
    let tuple_result = tuple_operations();
    // tuple_result = 1800
    let array_check: u64 = arr_min + arr_max + arr_sum + tuple_result;
    // array_check = 2 + 80 + 117 + 1800 = 1999
    // BREAKPOINT TARGET: line above - arr_min, arr_max, arr_sum, tuple_result, array_check

    // === Section 7: Fibonacci ===
    let fib_recursive = fibonacci(10);           // 55
    let fib_iterative = fibonacci_iterative(10); // 55
    let fib_check: u64 = fib_recursive + fib_iterative;
    // fib_check = 110
    // BREAKPOINT TARGET: line above - fib_recursive, fib_iterative, fib_check in scope

    // === Section 8: Bitwise operations ===
    let (bw_and, bw_or, bw_xor, bw_shl, bw_shr) = bitwise_ops(0xFF00, 0x0FF0);
    // bw_and = 0x0F00 = 3840
    // bw_or  = 0xFFF0 = 65520
    // bw_xor = 0xF0F0 = 61680
    // bw_shl = 0x3FC00 = 261120
    // bw_shr = 0x07F8 = 2040
    let bitwise_check: u64 = bw_and + bw_or + bw_xor + bw_shl + bw_shr;
    // bitwise_check = 3840 + 65520 + 61680 + 261120 + 2040 = 394200

    // === Section 9: Multi-width integers ===
    let width_result = multi_width_arithmetic();
    // width_result = 19000065790

    // === Section 10: Option handling ===
    let search_data: [u64; 5] = [100, 200, 300, 400, 500];
    let found = option_handling(&search_data, 300);
    // found = 2 * 100 + 300 = 500
    let not_found = option_handling(&search_data, 999);
    // not_found = 0
    let option_check: u64 = found + not_found;
    // option_check = 500

    // === Section 11: Byte manipulation ===
    let byte_result = byte_manipulation();
    // decoded = 0x0102030405060708 = 72623859790382856
    // memo_sum = sum of ASCII values of "CodeTrcr"
    //   C=67, o=111, d=100, e=101, T=84, r=114, c=99, r=114 = 790
    // byte_result = 72623859790382856 + 790 = 72623859790383646

    // === Section 12: Borrowing patterns ===
    let borrow_result = borrowing_demo();
    // borrow_result = 250

    // === Section 13: Iterator patterns ===
    let iter_result = iterator_demo();
    // iter_result = 225

    // === Section 14: Ledger entry (nested structs + bytes) ===
    let entry = create_ledger_entry(42, 5000);
    let entry_value = process_ledger_entry(&entry);
    // account_value = 5000 + (42 * 10) + 100 = 5520
    // memo_sum = 42 + 0 + 0 + 0 + 0 + 0 + 0 + 0 = 42
    // entry_value = 5520 + 42 + 42 = 5604

    // === Final checksum ===
    // Combine all section results into a single checksum value that proves
    // every code path executed correctly.
    let checksum: u64 = final_result
        + struct_check
        + enum_check
        + fail_value
        + init_value
        + close_value
        + loop_check
        + nested_check
        + array_check
        + fib_check
        + option_check
        + borrow_result
        + iter_result
        + entry_value;
    // checksum = 94 + 1520 + 490 + 999 + 500 + 0 + 460 + 190 + 1999 + 110
    //          + 500 + 250 + 225 + 5604 = 12941
    let _final_checksum = checksum;
    // BREAKPOINT TARGET: line above - checksum and all section results in scope
}
