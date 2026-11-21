use ace_serialize::{limits::Options, wire::packed::pack};
use std::fmt::Write;

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(output, "{byte:02x}").unwrap();
    }
    output
}

fn main() {
    let mut state = 1234567u32;
    let random: Vec<u8> = (0..1000)
        .map(|_| {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            (state >> 24) as u8
        })
        .collect();
    let vectors = [
        ("zero_8", vec![0; 8]),
        ("zero_16", vec![0; 16]),
        ("zero_257_words", vec![0; 257 * 8]),
        ("nonzero_word", vec![0xaa; 8]),
        ("nonzero_24", vec![0xaa; 24]),
        ("nonzero_256_words", vec![0xaa; 256 * 8]),
        ("nonzero_257_words", vec![0xaa; 257 * 8]),
        ("partial_13", vec![1; 13]),
        ("pseudorandom_1000", random),
    ];
    println!("[");
    for (index, (name, raw)) in vectors.iter().enumerate() {
        let packed = pack(raw, &Options::default()).unwrap();
        let comma = if index + 1 == vectors.len() { "" } else { "," };
        println!(
            "  {{\"name\":\"{}\",\"raw_hex\":\"{}\",\"packed_hex\":\"{}\"}}{}",
            name,
            hex(raw),
            hex(&packed),
            comma
        );
    }
    println!("]");
}
