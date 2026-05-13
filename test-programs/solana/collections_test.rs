//! Solana program exercising the collection types the recorder must
//! eventually round-trip as `ValueRecord::Sequence` / `Tuple` /
//! `Struct` (today the SBF recorder still only emits `Int` for the
//! synthetic register stream — the test pins the present-day shape and
//! a parallel `#[ignore]`d test asserts the spec-compliant variants).
//!
//! Programs typically build:
//! * a `Vec<u64>` (sum_of_vec)
//! * a `(u64, u64)` tuple (sum_pair)
//! * a `Point { x: i64, y: i64 }` struct (point_distance_sq)
//! * a `Vec<Point>` (sum_distances)
//!
//! Canonical execution:
//! * `sum_of_vec(&[1,2,3,4])` returns `10`.
//! * `sum_pair((10, 20))` returns `30`.
//! * `point_distance_sq(Point{x:3,y:4})` returns `25`.
//! * `compute()` returns `10 + 30 + 25 = 65`.

#![allow(dead_code)]

#[derive(Debug)]
struct Point {
    x: i64,
    y: i64,
}

fn sum_of_vec(xs: &[i64]) -> i64 {
    let mut total: i64 = 0;
    let mut i = 0;
    while i < xs.len() {
        total += xs[i];
        i += 1;
    }
    total
}

fn sum_pair(pair: (i64, i64)) -> i64 {
    let (a, b) = pair;
    a + b
}

fn point_distance_sq(p: &Point) -> i64 {
    p.x * p.x + p.y * p.y
}

pub fn compute() -> i64 {
    let xs: [i64; 4] = [1, 2, 3, 4];
    let xs_total = sum_of_vec(&xs);
    let pair = (10, 20);
    let p = Point { x: 3, y: 4 };
    let pair_total = sum_pair(pair);
    let dist_sq = point_distance_sq(&p);
    let combined = xs_total + pair_total + dist_sq;
    combined
}
