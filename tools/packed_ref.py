"""Independent implementation from the official packed encoding specification.

No project Rust implementation was consulted. Python's standard library has no
Cap'n Proto packed codec; the wire format is implemented directly here.
"""


def pack(raw: bytes, *, max_extra_words: int = 255) -> bytes:
    if not 0 <= max_extra_words <= 255:
        raise ValueError("max_extra_words must be between 0 and 255")
    data = raw + bytes(-len(raw) % 8)
    result = bytearray()
    pos = 0
    while pos < len(data):
        word = data[pos:pos + 8]
        pos += 8
        tag = sum(1 << i for i, byte in enumerate(word) if byte)
        result.append(tag)
        result.extend(byte for byte in word if byte)
        if tag == 0:
            count = 0
            while count < max_extra_words and pos < len(data) and data[pos:pos + 8] == bytes(8):
                count += 1
                pos += 8
            result.append(count)
        elif tag == 255:
            start = pos
            count = 0
            while count < max_extra_words and pos < len(data) and all(data[pos:pos + 8]):
                count += 1
                pos += 8
            result.append(count)
            result.extend(data[start:pos])
    return bytes(result)


def unpack(packed: bytes) -> bytes:
    result = bytearray()
    pos = 0

    def take(count: int) -> bytes:
        nonlocal pos
        if pos + count > len(packed):
            raise ValueError("truncated packed stream")
        chunk = packed[pos:pos + count]
        pos += count
        return chunk

    while pos < len(packed):
        tag = take(1)[0]
        for bit in range(8):
            result.extend(take(1) if tag & (1 << bit) else b"\0")
        if tag == 0:
            result.extend(bytes(8 * take(1)[0]))
        elif tag == 255:
            count = take(1)[0]
            result.extend(take(8 * count))
    return bytes(result)
