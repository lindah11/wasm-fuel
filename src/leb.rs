//! LEB128 decoding, the variable-length integer encoding WebAssembly uses for
//! every index, size and immediate.
//!
//! Both readers are width-aware. `u32` is not just "decode a `u64` and cast":
//! the WebAssembly specification requires that an encoding for an `N`-bit
//! integer carry no more than `ceil(N/7)` bytes and that the unused bits of the
//! final byte be a correct extension of the value. Accepting sloppier
//! encodings would let two different byte sequences mean the same module, which
//! is exactly the sort of thing a sandbox must refuse.
//!
//! ```
//! use wasm_fuel::leb::{read_u32, read_i32};
//!
//! let mut pos = 0;
//! assert_eq!(read_u32(&[0xE5, 0x8E, 0x26], &mut pos).unwrap(), 624_485);
//! assert_eq!(pos, 3);
//!
//! let mut pos = 0;
//! assert_eq!(read_i32(&[0x7F], &mut pos).unwrap(), -1);
//!
//! // 0xFF 0xFF 0xFF 0xFF 0x1F would need 33 bits.
//! let mut pos = 0;
//! assert!(read_u32(&[0xFF, 0xFF, 0xFF, 0xFF, 0x1F], &mut pos).is_err());
//! ```

use std::fmt;

/// Why a LEB128 value could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LebError {
    /// The input ended while the continuation bit was still set.
    UnexpectedEof,
    /// The encoded value does not fit the requested width, or the encoding is
    /// longer than the width allows.
    Overflow,
}

impl fmt::Display for LebError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LebError::UnexpectedEof => f.write_str("truncated LEB128 value"),
            LebError::Overflow => f.write_str("LEB128 value too large for its type"),
        }
    }
}

impl std::error::Error for LebError {}

/// Decodes an unsigned LEB128 value of at most `bits` bits.
///
/// `pos` is advanced past the value on success and left pointing at the byte
/// after the last one consumed on failure.
pub fn read_unsigned(bytes: &[u8], pos: &mut usize, bits: u32) -> Result<u64, LebError> {
    debug_assert!((1..=64).contains(&bits));
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
    loop {
        let byte = *bytes.get(*pos).ok_or(LebError::UnexpectedEof)?;
        *pos += 1;
        if shift + 7 < bits {
            result |= u64::from(byte & 0x7F) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
        } else {
            // This has to be the last byte: there is no room for another one.
            if byte & 0x80 != 0 {
                return Err(LebError::Overflow);
            }
            let used = bits - shift; // 1..=7 bits of this byte are payload
            let payload = u64::from(byte & 0x7F);
            if payload >> used != 0 {
                return Err(LebError::Overflow);
            }
            return Ok(result | (payload << shift));
        }
    }
}

/// Decodes a signed LEB128 value of at most `bits` bits, sign extended to
/// `i64`.
pub fn read_signed(bytes: &[u8], pos: &mut usize, bits: u32) -> Result<i64, LebError> {
    debug_assert!((1..=64).contains(&bits));
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
    loop {
        let byte = *bytes.get(*pos).ok_or(LebError::UnexpectedEof)?;
        *pos += 1;
        if shift + 7 < bits {
            result |= u64::from(byte & 0x7F) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                // Sign extend from the bit we just wrote.
                if byte & 0x40 != 0 && shift < 64 {
                    result |= u64::MAX << shift;
                }
                return Ok(result as i64);
            }
        } else {
            if byte & 0x80 != 0 {
                return Err(LebError::Overflow);
            }
            let used = bits - shift; // 1..=7 payload bits, the top one is the sign
            let payload = u64::from(byte & 0x7F);
            let sign = (payload >> (used - 1)) & 1;
            // Everything above the payload must repeat the sign bit, otherwise
            // the value does not fit.
            let expected_padding = if sign == 1 { (1u64 << (7 - used)) - 1 } else { 0 };
            if payload >> used != expected_padding {
                return Err(LebError::Overflow);
            }
            result |= (payload & ((1u64 << used) - 1)) << shift;
            if sign == 1 && bits < 64 {
                result |= u64::MAX << bits;
            }
            return Ok(result as i64);
        }
    }
}

/// Decodes a `u32` index or size.
pub fn read_u32(bytes: &[u8], pos: &mut usize) -> Result<u32, LebError> {
    read_unsigned(bytes, pos, 32).map(|v| v as u32)
}

/// Decodes a `u64`.
pub fn read_u64(bytes: &[u8], pos: &mut usize) -> Result<u64, LebError> {
    read_unsigned(bytes, pos, 64)
}

/// Decodes an `i32` immediate, such as the operand of `i32.const`.
pub fn read_i32(bytes: &[u8], pos: &mut usize) -> Result<i32, LebError> {
    read_signed(bytes, pos, 32).map(|v| v as i32)
}

/// Decodes an `i64` immediate.
pub fn read_i64(bytes: &[u8], pos: &mut usize) -> Result<i64, LebError> {
    read_signed(bytes, pos, 64)
}

/// Encodes an unsigned value, for building test fixtures and for round trip
/// tests.
pub fn write_unsigned(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Encodes a signed value.
pub fn write_signed(mut value: i64, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7; // arithmetic shift keeps the sign
        let sign_bit_set = byte & 0x40 != 0;
        if (value == 0 && !sign_bit_set) || (value == -1 && sign_bit_set) {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_of(bytes: &[u8]) -> Result<u32, LebError> {
        let mut pos = 0;
        let value = read_u32(bytes, &mut pos)?;
        assert_eq!(pos, bytes.len(), "decoder must consume the whole input");
        Ok(value)
    }

    fn i32_of(bytes: &[u8]) -> Result<i32, LebError> {
        let mut pos = 0;
        let value = read_i32(bytes, &mut pos)?;
        assert_eq!(pos, bytes.len(), "decoder must consume the whole input");
        Ok(value)
    }

    #[test]
    fn decodes_unsigned_values() {
        assert_eq!(u32_of(&[0x00]), Ok(0));
        assert_eq!(u32_of(&[0x01]), Ok(1));
        assert_eq!(u32_of(&[0x7F]), Ok(127));
        assert_eq!(u32_of(&[0x80, 0x01]), Ok(128));
        assert_eq!(u32_of(&[0xE5, 0x8E, 0x26]), Ok(624_485));
        assert_eq!(u32_of(&[0xFF, 0xFF, 0xFF, 0xFF, 0x0F]), Ok(u32::MAX));
    }

    #[test]
    fn decodes_signed_values() {
        assert_eq!(i32_of(&[0x00]), Ok(0));
        assert_eq!(i32_of(&[0x7F]), Ok(-1));
        assert_eq!(i32_of(&[0x3F]), Ok(63));
        assert_eq!(i32_of(&[0x40]), Ok(-64));
        assert_eq!(i32_of(&[0xC0, 0x00]), Ok(64));
        assert_eq!(i32_of(&[0x9B, 0xF1, 0x59]), Ok(-624_485));
        assert_eq!(i32_of(&[0xFF, 0xFF, 0xFF, 0xFF, 0x07]), Ok(i32::MAX));
        assert_eq!(i32_of(&[0x80, 0x80, 0x80, 0x80, 0x78]), Ok(i32::MIN));
    }

    #[test]
    fn rejects_oversized_encodings() {
        // 33 bits of payload.
        assert_eq!(u32_of(&[0xFF, 0xFF, 0xFF, 0xFF, 0x1F]), Err(LebError::Overflow));
        // Six bytes can never encode a u32, even if the value would fit.
        assert_eq!(
            u32_of(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x00]),
            Err(LebError::Overflow)
        );
        // The padding of the last byte must match the sign.
        assert_eq!(i32_of(&[0xFF, 0xFF, 0xFF, 0xFF, 0x0F]), Err(LebError::Overflow));
        assert_eq!(i32_of(&[0x80, 0x80, 0x80, 0x80, 0x08]), Err(LebError::Overflow));
    }

    #[test]
    fn rejects_truncated_input() {
        assert_eq!(u32_of(&[]), Err(LebError::UnexpectedEof));
        assert_eq!(u32_of(&[0x80]), Err(LebError::UnexpectedEof));
        assert_eq!(u32_of(&[0x80, 0x80]), Err(LebError::UnexpectedEof));
        assert_eq!(i32_of(&[0xFF]), Err(LebError::UnexpectedEof));
    }

    #[test]
    fn handles_the_full_64_bit_range() {
        let mut pos = 0;
        let bytes = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01];
        assert_eq!(read_u64(&bytes, &mut pos), Ok(u64::MAX));
        assert_eq!(pos, 10);

        let mut pos = 0;
        let bytes = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x02];
        assert_eq!(read_u64(&bytes, &mut pos), Err(LebError::Overflow));

        let mut pos = 0;
        let bytes = [0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x7F];
        assert_eq!(read_i64(&bytes, &mut pos), Ok(i64::MIN));
    }

    #[test]
    fn round_trips_through_the_encoders() {
        let unsigned = [0u64, 1, 63, 64, 127, 128, 300, 624_485, u32::MAX as u64, u64::MAX];
        for &value in &unsigned {
            let mut buf = Vec::new();
            write_unsigned(value, &mut buf);
            let mut pos = 0;
            assert_eq!(read_u64(&buf, &mut pos), Ok(value), "value {}", value);
            assert_eq!(pos, buf.len());
        }

        let signed = [0i64, 1, -1, 63, -64, 64, -65, 624_485, -624_485, i32::MAX as i64, i32::MIN as i64, i64::MAX, i64::MIN];
        for &value in &signed {
            let mut buf = Vec::new();
            write_signed(value, &mut buf);
            let mut pos = 0;
            assert_eq!(read_i64(&buf, &mut pos), Ok(value), "value {}", value);
            assert_eq!(pos, buf.len());
        }
    }

    #[test]
    fn position_advances_across_consecutive_values() {
        let bytes = [0x08, 0xE5, 0x8E, 0x26, 0x7F];
        let mut pos = 0;
        assert_eq!(read_u32(&bytes, &mut pos), Ok(8));
        assert_eq!(pos, 1);
        assert_eq!(read_u32(&bytes, &mut pos), Ok(624_485));
        assert_eq!(pos, 4);
        assert_eq!(read_i32(&bytes, &mut pos), Ok(-1));
        assert_eq!(pos, 5);
    }
}
