use crate::utils::sha256_hex;

pub fn exact_hash(error_type: &str, message: &str, stacktrace: &str) -> String {
    sha256_hex(&[
        error_type.as_bytes(),
        b"\x1f",
        message.as_bytes(),
        b"\x1f",
        stacktrace.as_bytes(),
    ])
}
