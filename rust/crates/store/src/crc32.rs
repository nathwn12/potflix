
/// The reflected IEEE 802.3 polynomial (`crc32.IEEETable`).
const POLY: u32 = 0xEDB8_8320;

const fn table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ POLY
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

static TABLE: [u32; 256] = table();

/// CRC32/IEEE of `data` -- `crc32.Checksum(data, crc32.IEEETable)`.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc = TABLE[((crc ^ byte as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::crc32;

    /// The standard published vectors for CRC32/IEEE: empty, the classic
    /// check string, single bytes, and the all-ones case. A reflected-
    /// polynomial mistake or a missing final XOR changes every one of these.
    #[test]
    fn known_vectors() {
        for (input, want) in [
            (&b""[..], 0x0000_0000u32),
            (b"a", 0xE8B7_BE43),
            (b"abc", 0x3524_41C2),
            (b"123456789", 0xCBF4_3926),
            (b"The quick brown fox jumps over the lazy dog", 0x414F_A339),
            (&[0x00], 0xD202_EF8D),
            (&[0x00, 0x00, 0x00, 0x00], 0x2144_DF1C),
            (&[0xff, 0xff, 0xff, 0xff], 0xFFFF_FFFF),
        ] {
            assert_eq!(crc32(input), want, "crc32({input:?})");
        }
    }

    /// The one vector the store format itself depends on: the header CRC of
    /// `PFLXST01` + version 1 + flags 0, as it appears in the Go-written
    /// fixtures (`tests/fixtures/go-*.pflx`, bytes 16..20 = `13 de b7 ea`).
    #[test]
    fn header_crc_matches_the_go_fixtures() {
        let mut head = Vec::new();
        head.extend_from_slice(b"PFLXST01");
        head.extend_from_slice(&1u32.to_le_bytes());
        head.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(crc32(&head), 0xEAB7_DE13);
        assert_eq!(crc32(&head).to_le_bytes(), [0x13, 0xde, 0xb7, 0xea]);
    }
}
