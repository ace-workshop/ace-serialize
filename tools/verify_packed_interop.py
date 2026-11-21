#!/usr/bin/env python3
"""Check the reference codec against specification examples and Rust vectors.

The optional Rust decoder executable reads packed bytes on stdin and writes raw
hex on stdout. It is required for differing encodings and the discretion test.
"""
import argparse
import json
import subprocess
import sys

from packed_ref import pack, unpack


def padded(raw):
    return raw + bytes(-len(raw) % 8)


def compare(name, raw, rust_packed, python_packed, decoder):
    expected = padded(raw)
    if unpack(rust_packed) != expected:
        raise ValueError(f"{name}: Python decoding of Rust bytes differs from padded raw")
    if unpack(python_packed) != expected:
        raise ValueError(f"{name}: Python encoding does not round-trip")
    if python_packed == rust_packed:
        print(f"{name}: byte_identical")
        return
    if not decoder:
        raise ValueError(f"{name}: differing encodings need --rust-decoder")
    result = subprocess.run([decoder], input=python_packed, capture_output=True, check=True)
    if bytes.fromhex(result.stdout.decode('ascii')) != expected:
        raise ValueError(f"{name}: Rust decoding of Python bytes differs from padded raw")
    first = next((i for i, (a, b) in enumerate(zip(rust_packed, python_packed)) if a != b),
                 min(len(rust_packed), len(python_packed)))
    print(f"{name}: encoding_discretion rust_len={len(rust_packed)} "
          f"python_len={len(python_packed)} length_delta={len(python_packed) - len(rust_packed):+d} "
          f"first_diff={first}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('json_path', help='Rust vector JSON')
    parser.add_argument('--rust-decoder', help='public-API Rust decoder probe')
    args = parser.parse_args()
    examples = [
        ('official_sparse', bytes.fromhex('08 00 00 00 03 00 02 00 19 00 00 00 aa 01 00 00'),
         bytes.fromhex('51 08 03 02 31 19 aa 01')),
        ('official_zero', bytes(32), bytes.fromhex('00 03')),
        ('official_nonzero', bytes([0x8a]) * 32, b'\xff' + bytes([0x8a]) * 8 + b'\x03' + bytes([0x8a]) * 24),
    ]
    for name, raw, encoded in examples:
        if pack(raw) != encoded or unpack(encoded) != raw:
            raise ValueError(f'{name}: official example failed')
        print(f'{name}: pack/unpack PASS')
    with open(args.json_path, encoding='utf-8') as file:
        vectors = json.load(file)
    if len(vectors) != 9 or len({v['name'] for v in vectors}) != 9:
        raise ValueError('expected nine distinct Rust vectors')
    for vector in vectors:
        raw = bytes.fromhex(vector['raw_hex'])
        compare(vector['name'], raw, bytes.fromhex(vector['packed_hex']), pack(raw), args.rust_decoder)
    if args.rust_decoder:
        vector = next(v for v in vectors if v['name'] == 'zero_257_words')
        raw = bytes.fromhex(vector['raw_hex'])
        conservative = pack(raw, max_extra_words=1)
        rust_packed = bytes.fromhex(vector['packed_hex'])
        if conservative == rust_packed:
            raise ValueError('discretion probe did not produce a different encoding')
        compare('synthetic_conservative', raw, rust_packed, conservative, args.rust_decoder)
    else:
        print('synthetic_conservative: SKIPPED (no --rust-decoder)')
    print('PASS')


if __name__ == '__main__':
    try:
        main()
    except (ValueError, KeyError, OSError, subprocess.SubprocessError, StopIteration) as error:
        print(f'FAIL: {error}', file=sys.stderr)
        sys.exit(1)
