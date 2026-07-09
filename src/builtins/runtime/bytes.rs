use base64::{Engine as _, engine::general_purpose::STANDARD};

use super::typed::{VmArrayRef, VmBytesRef};
use super::{VmArray, VmBytes};
use crate::vm::{Value, VmError, VmResult};
use pd_host_function::pd_host_function;

/// Encodes a string as UTF-8 bytes.
#[pd_host_function(name = "bytes::from_utf8")]
pub(super) fn builtin_bytes_from_utf8_impl(text: &str) -> VmBytes {
    text.as_bytes().to_vec()
}

/// Decodes UTF-8 bytes into a string.
#[pd_host_function(name = "bytes::to_utf8")]
pub(super) fn builtin_bytes_to_utf8_impl(payload: VmBytesRef<'_>) -> VmResult<String> {
    std::str::from_utf8(payload)
        .map(str::to_string)
        .map_err(|err| VmError::HostError(format!("bytes::to_utf8 requires valid utf-8: {err}")))
}

/// Decodes bytes into a string using UTF-8 replacement semantics.
#[pd_host_function(name = "bytes::to_utf8_lossy")]
pub(super) fn builtin_bytes_to_utf8_lossy_impl(payload: VmBytesRef<'_>) -> String {
    String::from_utf8_lossy(payload).into_owned()
}

/// Decodes a hexadecimal string into bytes.
#[pd_host_function(name = "bytes::from_hex")]
pub(super) fn builtin_bytes_from_hex_impl(text: &str) -> VmResult<VmBytes> {
    if !text.len().is_multiple_of(2) {
        return Err(VmError::HostError(
            "bytes::from_hex requires an even number of hex digits".to_string(),
        ));
    }

    let mut out = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        let high = decode_hex_nibble(bytes[index]).ok_or_else(|| {
            VmError::HostError(format!(
                "bytes::from_hex encountered invalid hex digit '{}' at offset {}",
                bytes[index] as char, index
            ))
        })?;
        let low = decode_hex_nibble(bytes[index + 1]).ok_or_else(|| {
            VmError::HostError(format!(
                "bytes::from_hex encountered invalid hex digit '{}' at offset {}",
                bytes[index + 1] as char,
                index + 1
            ))
        })?;
        out.push((high << 4) | low);
        index += 2;
    }
    Ok(out)
}

/// Encodes bytes as lowercase hexadecimal.
#[pd_host_function(name = "bytes::to_hex")]
pub(super) fn builtin_bytes_to_hex_impl(payload: VmBytesRef<'_>) -> String {
    let mut out = String::with_capacity(payload.len() * 2);
    for byte in payload {
        out.push(nibble_to_hex(byte >> 4));
        out.push(nibble_to_hex(byte & 0x0F));
    }
    out
}

/// Decodes a base64 string into bytes.
#[pd_host_function(name = "bytes::from_base64")]
pub(super) fn builtin_bytes_from_base64_impl(text: &str) -> VmResult<VmBytes> {
    STANDARD.decode(text).map_err(|err| {
        VmError::HostError(format!("bytes::from_base64 requires valid base64: {err}"))
    })
}

/// Encodes bytes as standard base64.
#[pd_host_function(name = "bytes::to_base64")]
pub(super) fn builtin_bytes_to_base64_impl(payload: VmBytesRef<'_>) -> String {
    STANDARD.encode(payload)
}

/// Converts an array of ints in 0..=255 into bytes.
#[pd_host_function(name = "bytes::from_array_u8")]
pub(super) fn builtin_bytes_from_array_u8_impl(values: VmArrayRef<'_>) -> VmResult<VmBytes> {
    let mut out = Vec::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        let Value::Int(value) = value else {
            return Err(VmError::HostError(format!(
                "bytes::from_array_u8 entry {index} must be an int in 0..=255"
            )));
        };
        let value = u8::try_from(*value).map_err(|_| {
            VmError::HostError(format!(
                "bytes::from_array_u8 entry {index} must be an int in 0..=255"
            ))
        })?;
        out.push(value);
    }
    Ok(out)
}

/// Converts bytes into an array of ints in 0..=255.
#[pd_host_function(name = "bytes::to_array_u8")]
pub(super) fn builtin_bytes_to_array_u8_impl(payload: VmBytesRef<'_>) -> VmArray {
    payload
        .iter()
        .copied()
        .map(|byte| Value::Int(i64::from(byte)))
        .collect()
}

fn decode_hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn nibble_to_hex(value: u8) -> char {
    match value {
        0..=9 => char::from(b'0' + value),
        10..=15 => char::from(b'a' + (value - 10)),
        _ => unreachable!("hex nibble out of range"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_hex_rejects_odd_length() {
        let err = builtin_bytes_from_hex_impl("abc").expect_err("odd-length hex should fail");
        assert!(err.to_string().contains("even number of hex digits"));
    }

    #[test]
    fn from_base64_rejects_invalid_input() {
        let err = builtin_bytes_from_base64_impl("%%%").expect_err("invalid base64 should fail");
        assert!(err.to_string().contains("valid base64"));
    }

    #[test]
    fn to_utf8_rejects_invalid_sequences() {
        let err = builtin_bytes_to_utf8_impl(&[0xFF]).expect_err("invalid utf-8 should fail");
        assert!(err.to_string().contains("valid utf-8"));
    }

    #[test]
    fn to_array_roundtrips() {
        let values = [Value::Int(1), Value::Int(255)];
        let bytes = builtin_bytes_from_array_u8_impl(&values).expect("array<u8> should decode");
        assert_eq!(bytes, vec![1, 255]);
        assert_eq!(
            builtin_bytes_to_array_u8_impl(&bytes),
            vec![Value::Int(1), Value::Int(255)]
        );
    }
}
