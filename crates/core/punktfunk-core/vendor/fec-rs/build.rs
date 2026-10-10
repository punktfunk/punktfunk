#![allow(clippy::needless_range_loop)]

use std::env;
use std::fs::File;
use std::io::Write;
use std::path::Path;

const FIELD_SIZE: usize = 256;
const GENERATING_POLYNOMIAL: usize = 29;

fn gen_log_table(polynomial: usize) -> [u8; FIELD_SIZE] {
    let mut result = [0u8; FIELD_SIZE];
    let mut b: usize = 1;

    for log in 0..FIELD_SIZE - 1 {
        result[b] = log as u8;
        b <<= 1;
        if FIELD_SIZE <= b {
            b = (b - FIELD_SIZE) ^ polynomial;
        }
    }

    result
}

const EXP_TABLE_SIZE: usize = FIELD_SIZE * 2 - 2;

fn gen_exp_table(log_table: &[u8; FIELD_SIZE]) -> [u8; EXP_TABLE_SIZE] {
    let mut result = [0u8; EXP_TABLE_SIZE];

    for i in 1..FIELD_SIZE {
        let log = log_table[i] as usize;
        result[log] = i as u8;
        result[log + FIELD_SIZE - 1] = i as u8;
    }

    result
}

fn multiply(log_table: &[u8; FIELD_SIZE], exp_table: &[u8; EXP_TABLE_SIZE], a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        0
    } else {
        let log_a = log_table[a as usize];
        let log_b = log_table[b as usize];
        let log_result = log_a as usize + log_b as usize;
        exp_table[log_result]
    }
}

fn gen_mul_table(
    log_table: &[u8; FIELD_SIZE],
    exp_table: &[u8; EXP_TABLE_SIZE],
) -> [[u8; FIELD_SIZE]; FIELD_SIZE] {
    let mut result = [[0u8; FIELD_SIZE]; FIELD_SIZE];

    for a in 0..FIELD_SIZE {
        for b in 0..FIELD_SIZE {
            result[a][b] = multiply(log_table, exp_table, a as u8, b as u8);
        }
    }

    result
}

fn gen_mul_table_half(
    log_table: &[u8; FIELD_SIZE],
    exp_table: &[u8; EXP_TABLE_SIZE],
) -> ([[u8; 16]; FIELD_SIZE], [[u8; 16]; FIELD_SIZE]) {
    let mut low = [[0u8; 16]; FIELD_SIZE];
    let mut high = [[0u8; 16]; FIELD_SIZE];

    for a in 0..FIELD_SIZE {
        for b in 0..FIELD_SIZE {
            let mut result = 0;
            if a != 0 && b != 0 {
                let log_a = log_table[a];
                let log_b = log_table[b];
                result = exp_table[log_a as usize + log_b as usize];
            }
            if (b & 0x0F) == b {
                low[a][b] = result;
            }
            if (b & 0xF0) == b {
                high[a][b >> 4] = result;
            }
        }
    }
    (low, high)
}

/// Generate the GFNI affine matrix table.
///
/// For each constant `c` in GF(2^8), compute a u64-packed 8x8 binary matrix
/// such that `vgf2p8affineqb(x, matrix, 0)` produces `c * x` in our GF(2^8).
///
/// vgf2p8affineqb semantics:
///   result_bit[i] = popcount(x AND qword_byte[7-i]) mod 2
/// where i goes from 0 (LSB) to 7 (MSB).
///
/// Matrix packing: qword byte[7] = row for output bit 7 (MSB),
///                 qword byte[0] = row for output bit 0 (LSB).
fn gen_gfni_table(
    log_table: &[u8; FIELD_SIZE],
    exp_table: &[u8; EXP_TABLE_SIZE],
) -> [u64; FIELD_SIZE] {
    let mut result = [0u64; FIELD_SIZE];

    for c in 0..FIELD_SIZE {
        // Build row bytes for each output bit.
        // row_for_bit_i = mask where bit j is set iff input bit j contributes to output bit i.
        // M[i][j] = bit_i(c * (1 << j))
        let mut rows = [0u8; 8];
        for j in 0..8u8 {
            let basis = 1u8 << j; // input with only bit j set
            let product = multiply(log_table, exp_table, c as u8, basis);
            // product's bit i tells us M[i][j]
            for i in 0..8u8 {
                if (product >> i) & 1 == 1 {
                    rows[i as usize] |= 1 << j;
                }
            }
        }

        // Pack into u64: byte[7-i] = rows[i]
        // vgf2p8affineqb: result_bit[i] = popcount(x AND byte[7-i]) mod 2
        // We want result_bit[i] = bit i of (c*x), so byte[7-i] = rows[i].
        let mut matrix: u64 = 0;
        for i in 0..8u32 {
            matrix |= (rows[i as usize] as u64) << ((7 - i) * 8);
        }
        result[c] = matrix;
    }

    result
}

fn write_1d_table(f: &mut File, table: &[u8], name: &str) {
    let len = table.len();
    write!(f, "pub static {name}: [u8; {len}] = [").unwrap();
    for v in table {
        write!(f, "{v}, ").unwrap();
    }
    writeln!(f, "];").unwrap();
}

fn write_2d_table(f: &mut File, table: &[[u8; 16]; FIELD_SIZE], name: &str) {
    let rows = table.len();
    let cols = table[0].len();
    write!(f, "pub static {name}: [[u8; {cols}]; {rows}] = [").unwrap();
    for row in table {
        write!(f, "[").unwrap();
        for v in row {
            write!(f, "{v}, ").unwrap();
        }
        writeln!(f, "],").unwrap();
    }
    writeln!(f, "];").unwrap();
}

fn write_mul_table(f: &mut File, table: &[[u8; FIELD_SIZE]; FIELD_SIZE]) {
    let rows = table.len();
    let cols = table[0].len();
    write!(f, "pub static MUL_TABLE: [[u8; {cols}]; {rows}] = [").unwrap();
    for row in table {
        write!(f, "[").unwrap();
        for v in row {
            write!(f, "{v}, ").unwrap();
        }
        writeln!(f, "],").unwrap();
    }
    writeln!(f, "];").unwrap();
}

fn write_gfni_table(f: &mut File, table: &[u64; FIELD_SIZE]) {
    write!(f, "pub static GFNI_TABLE: [u64; {}] = [", FIELD_SIZE).unwrap();
    for v in table {
        write!(f, "0x{v:016X}, ").unwrap();
    }
    writeln!(f, "];").unwrap();
}

fn main() {
    let log_table = gen_log_table(GENERATING_POLYNOMIAL);
    let exp_table = gen_exp_table(&log_table);
    let mul_table = gen_mul_table(&log_table, &exp_table);
    let (mul_table_low, mul_table_high) = gen_mul_table_half(&log_table, &exp_table);
    let gfni_table = gen_gfni_table(&log_table, &exp_table);

    let out_dir = env::var("OUT_DIR").unwrap();
    let dest_path = Path::new(&out_dir).join("tables.rs");
    let mut f = File::create(&dest_path).unwrap();

    write_1d_table(&mut f, &log_table, "LOG_TABLE");
    write_1d_table(&mut f, &exp_table, "EXP_TABLE");
    write_mul_table(&mut f, &mul_table);
    write_2d_table(&mut f, &mul_table_low, "MUL_TABLE_LOW");
    write_2d_table(&mut f, &mul_table_high, "MUL_TABLE_HIGH");
    write_gfni_table(&mut f, &gfni_table);
}
