//! Locale-independent numeric conversion backend for the vendored libplacebo.

use std::{
    ffi::c_int,
    ptr, slice,
    str::{self, FromStr},
};

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlStr {
    buf: *mut u8,
    len: usize,
}

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

const fn make_hex_pairs() -> [u8; 512] {
    let mut pairs = [0; 512];
    let mut value = 0;
    while value < 256 {
        pairs[value * 2] = HEX_DIGITS[value >> 4];
        pairs[value * 2 + 1] = HEX_DIGITS[value & 0xf];
        value += 1;
    }
    pairs
}

static HEX_PAIRS: [u8; 512] = make_hex_pairs();

unsafe fn write_output(buf: *mut u8, len: usize, bytes: &[u8]) -> c_int {
    if buf.is_null() || bytes.is_empty() || bytes.len() > len || bytes.len() > c_int::MAX as usize {
        return 0;
    }

    // SAFETY: The C ABI requires `buf` to address `len` writable bytes. The
    // bounds check above proves the complete result fits in that allocation.
    unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), buf, bytes.len()) };
    bytes.len() as c_int
}

unsafe fn print_integer<I: itoap::Integer>(buf: *mut u8, len: usize, value: I) -> c_int {
    if buf.is_null() {
        return 0;
    }

    if len >= I::MAX_LEN {
        // SAFETY: `I::MAX_LEN` bytes are sufficient for every value of `I`,
        // and the C ABI promises that `buf` addresses `len` writable bytes.
        return unsafe { itoap::write_to_ptr(buf, value) } as c_int;
    }

    // Preserve `std::to_chars` semantics for short buffers without making the
    // normal libplacebo path pay for an intermediate copy.
    let mut storage = [0u8; 40];
    // SAFETY: The stack allocation is large enough for every `itoap::Integer`.
    let formatted_len = unsafe { itoap::write_to_ptr(storage.as_mut_ptr(), value) };
    // SAFETY: Forwarding the validated C destination to the shared writer.
    unsafe { write_output(buf, len, &storage[..formatted_len]) }
}

unsafe fn print_float<F>(buf: *mut u8, len: usize, value: F) -> c_int
where
    F: zmij::Float,
{
    let mut storage = zmij::Buffer::new();
    let formatted = storage.format(value);
    let mut bytes = formatted.as_bytes();

    // Keep zmij's trailing `.0` even though `std::to_chars` omitted it. Zmij
    // uses fixed notation for a wider range of values, so stripping it can
    // turn a large float such as `3000000000.0` into an overflowing integer
    // token before Naga applies the surrounding GLSL float conversion.
    if bytes == b"NaN" {
        bytes = b"nan";
    }

    // SAFETY: Forwarding the validated C destination to the shared writer.
    unsafe { write_output(buf, len, bytes) }
}

fn valid_number_start(value: &[u8]) -> bool {
    value.first() != Some(&b'+') && !value.first().is_some_and(u8::is_ascii_whitespace)
}

fn significand_is_nonzero(value: &[u8]) -> bool {
    value
        .iter()
        .copied()
        .take_while(|byte| !matches!(byte, b'e' | b'E'))
        .any(|byte| matches!(byte, b'1'..=b'9'))
}

fn special_f32(value: &[u8]) -> Option<f32> {
    let (negative, token) = value
        .strip_prefix(b"-")
        .map_or((false, value), |token| (true, token));
    if token.eq_ignore_ascii_case(b"inf") || token.eq_ignore_ascii_case(b"infinity") {
        return Some(if negative {
            f32::NEG_INFINITY
        } else {
            f32::INFINITY
        });
    }
    token
        .eq_ignore_ascii_case(b"nan")
        .then(|| f32::NAN.copysign(if negative { -1.0 } else { 1.0 }))
}

fn special_f64(value: &[u8]) -> Option<f64> {
    let (negative, token) = value
        .strip_prefix(b"-")
        .map_or((false, value), |token| (true, token));
    if token.eq_ignore_ascii_case(b"inf") || token.eq_ignore_ascii_case(b"infinity") {
        return Some(if negative {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        });
    }
    token
        .eq_ignore_ascii_case(b"nan")
        .then(|| f64::NAN.copysign(if negative { -1.0 } else { 1.0 }))
}

fn parse_f32(value: &[u8]) -> Option<f32> {
    if !valid_number_start(value) {
        return None;
    }
    if let Some(special) = special_f32(value) {
        return Some(special);
    }

    let parsed = fast_float::parse::<f32, _>(value).ok()?;
    if parsed.is_infinite() || (parsed == 0.0 && significand_is_nonzero(value)) {
        return None;
    }
    Some(parsed)
}

fn parse_f64(value: &[u8]) -> Option<f64> {
    if !valid_number_start(value) {
        return None;
    }
    if let Some(special) = special_f64(value) {
        return Some(special);
    }

    let parsed = fast_float::parse::<f64, _>(value).ok()?;
    if parsed.is_infinite() || (parsed == 0.0 && significand_is_nonzero(value)) {
        return None;
    }
    Some(parsed)
}

unsafe fn parse_value<T>(
    value: PlStr,
    out: *mut T,
    parse: impl FnOnce(&[u8]) -> Option<T>,
) -> bool {
    if out.is_null() {
        return false;
    }
    if value.buf.is_null() || value.len == 0 {
        return false;
    }
    // SAFETY: A non-empty `pl_str` promises that `buf` addresses `len`
    // initialized bytes for the duration of this call.
    let value = unsafe { slice::from_raw_parts(value.buf.cast_const(), value.len) };
    let Some(parsed) = parse(value) else {
        return false;
    };

    // SAFETY: The C ABI requires a non-null `out` to address writable storage
    // for T. It is written only after the entire input parses successfully.
    unsafe { out.write(parsed) };
    true
}

fn parse_integer<T>(value: &[u8]) -> Option<T>
where
    T: FromStr,
{
    if !valid_number_start(value) {
        return None;
    }
    str::from_utf8(value).ok()?.parse().ok()
}

unsafe fn print_hex_impl(buf: *mut u8, len: usize, value: u16) -> c_int {
    if buf.is_null() {
        return 0;
    }

    let (digits, first_pair, second_pair) = match value {
        0x0000..=0x000f => {
            if len < 1 {
                return 0;
            }
            // SAFETY: The branch above proves the one-byte result fits.
            unsafe { buf.write(*HEX_DIGITS.get_unchecked(usize::from(value))) };
            return 1;
        }
        0x0010..=0x00ff => (2, value as u8, None),
        0x0100..=0x0fff => {
            if len < 3 {
                return 0;
            }
            // SAFETY: The branch above proves the first output byte fits.
            unsafe {
                buf.write(*HEX_DIGITS.get_unchecked(usize::from(value >> 8)));
            }
            (3, value as u8, None)
        }
        _ => (4, (value >> 8) as u8, Some(value as u8)),
    };
    if len < digits {
        return 0;
    }

    let pair_offset = usize::from(first_pair) * 2;
    // SAFETY: The table contains two bytes for every `u8`; the length checks
    // above prove the complete result fits in the C buffer.
    unsafe {
        let first_output = if digits == 3 { buf.add(1) } else { buf };
        ptr::copy_nonoverlapping(HEX_PAIRS.as_ptr().add(pair_offset), first_output, 2);
        if let Some(second_pair) = second_pair {
            ptr::copy_nonoverlapping(
                HEX_PAIRS.as_ptr().add(usize::from(second_pair) * 2),
                buf.add(2),
                2,
            );
        }
    }
    digits as c_int
}

unsafe fn print_int_impl(buf: *mut u8, len: usize, value: i32) -> c_int {
    // SAFETY: The caller owns the destination described by the C ABI.
    unsafe { print_integer(buf, len, value) }
}

unsafe fn print_uint_impl(buf: *mut u8, len: usize, value: u32) -> c_int {
    // SAFETY: The caller owns the destination described by the C ABI.
    unsafe { print_integer(buf, len, value) }
}

unsafe fn print_int64_impl(buf: *mut u8, len: usize, value: i64) -> c_int {
    // SAFETY: The caller owns the destination described by the C ABI.
    unsafe { print_integer(buf, len, value) }
}

unsafe fn print_uint64_impl(buf: *mut u8, len: usize, value: u64) -> c_int {
    // SAFETY: The caller owns the destination described by the C ABI.
    unsafe { print_integer(buf, len, value) }
}

unsafe fn print_float_impl(buf: *mut u8, len: usize, value: f32) -> c_int {
    // SAFETY: The caller owns the destination described by the C ABI.
    unsafe { print_float(buf, len, value) }
}

unsafe fn print_double_impl(buf: *mut u8, len: usize, value: f64) -> c_int {
    // SAFETY: The caller owns the destination described by the C ABI.
    unsafe { print_float(buf, len, value) }
}

unsafe fn parse_hex_impl(value: PlStr, out: *mut u16) -> bool {
    // SAFETY: `parse_value` validates both C arguments before accessing them.
    unsafe {
        parse_value(value, out, |value| {
            if !valid_number_start(value) {
                return None;
            }
            u16::from_str_radix(str::from_utf8(value).ok()?, 16).ok()
        })
    }
}

unsafe fn parse_int_impl(value: PlStr, out: *mut i32) -> bool {
    // SAFETY: `parse_value` validates both C arguments before accessing them.
    unsafe { parse_value(value, out, parse_integer) }
}

unsafe fn parse_uint_impl(value: PlStr, out: *mut u32) -> bool {
    // SAFETY: `parse_value` validates both C arguments before accessing them.
    unsafe { parse_value(value, out, parse_integer) }
}

unsafe fn parse_int64_impl(value: PlStr, out: *mut i64) -> bool {
    // SAFETY: `parse_value` validates both C arguments before accessing them.
    unsafe { parse_value(value, out, parse_integer) }
}

unsafe fn parse_uint64_impl(value: PlStr, out: *mut u64) -> bool {
    // SAFETY: `parse_value` validates both C arguments before accessing them.
    unsafe { parse_value(value, out, parse_integer) }
}

unsafe fn parse_float_impl(value: PlStr, out: *mut f32) -> bool {
    // SAFETY: `parse_value` validates both C arguments before accessing them.
    unsafe { parse_value(value, out, parse_f32) }
}

unsafe fn parse_double_impl(value: PlStr, out: *mut f64) -> bool {
    // SAFETY: `parse_value` validates both C arguments before accessing them.
    unsafe { parse_value(value, out, parse_f64) }
}

#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn pl_str_print_hex(buf: *mut u8, len: usize, value: u16) -> c_int {
    // SAFETY: The caller promises the libplacebo numeric-output ABI.
    unsafe { print_hex_impl(buf, len, value) }
}

#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn pl_str_print_int(buf: *mut u8, len: usize, value: i32) -> c_int {
    // SAFETY: The caller promises the libplacebo numeric-output ABI.
    unsafe { print_int_impl(buf, len, value) }
}

#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn pl_str_print_uint(buf: *mut u8, len: usize, value: u32) -> c_int {
    // SAFETY: The caller promises the libplacebo numeric-output ABI.
    unsafe { print_uint_impl(buf, len, value) }
}

#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn pl_str_print_int64(buf: *mut u8, len: usize, value: i64) -> c_int {
    // SAFETY: The caller promises the libplacebo numeric-output ABI.
    unsafe { print_int64_impl(buf, len, value) }
}

#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn pl_str_print_uint64(buf: *mut u8, len: usize, value: u64) -> c_int {
    // SAFETY: The caller promises the libplacebo numeric-output ABI.
    unsafe { print_uint64_impl(buf, len, value) }
}

#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn pl_str_print_float(buf: *mut u8, len: usize, value: f32) -> c_int {
    // SAFETY: The caller promises the libplacebo numeric-output ABI.
    unsafe { print_float_impl(buf, len, value) }
}

#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn pl_str_print_double(buf: *mut u8, len: usize, value: f64) -> c_int {
    // SAFETY: The caller promises the libplacebo numeric-output ABI.
    unsafe { print_double_impl(buf, len, value) }
}

#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn pl_str_parse_hex(value: PlStr, out: *mut u16) -> bool {
    // SAFETY: The caller promises the libplacebo numeric-input ABI.
    unsafe { parse_hex_impl(value, out) }
}

#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn pl_str_parse_int(value: PlStr, out: *mut i32) -> bool {
    // SAFETY: The caller promises the libplacebo numeric-input ABI.
    unsafe { parse_int_impl(value, out) }
}

#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn pl_str_parse_uint(value: PlStr, out: *mut u32) -> bool {
    // SAFETY: The caller promises the libplacebo numeric-input ABI.
    unsafe { parse_uint_impl(value, out) }
}

#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn pl_str_parse_int64(value: PlStr, out: *mut i64) -> bool {
    // SAFETY: The caller promises the libplacebo numeric-input ABI.
    unsafe { parse_int64_impl(value, out) }
}

#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn pl_str_parse_uint64(value: PlStr, out: *mut u64) -> bool {
    // SAFETY: The caller promises the libplacebo numeric-input ABI.
    unsafe { parse_uint64_impl(value, out) }
}

#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn pl_str_parse_float(value: PlStr, out: *mut f32) -> bool {
    // SAFETY: The caller promises the libplacebo numeric-input ABI.
    unsafe { parse_float_impl(value, out) }
}

#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn pl_str_parse_double(value: PlStr, out: *mut f64) -> bool {
    // SAFETY: The caller promises the libplacebo numeric-input ABI.
    unsafe { parse_double_impl(value, out) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{hint::black_box, time::Instant};

    unsafe extern "C" {
        #[link_name = "pl_str_print_hex"]
        fn abi_print_hex(buf: *mut u8, len: usize, value: u16) -> c_int;
        #[link_name = "pl_str_print_int"]
        fn abi_print_int(buf: *mut u8, len: usize, value: i32) -> c_int;
        #[link_name = "pl_str_print_uint"]
        fn abi_print_uint(buf: *mut u8, len: usize, value: u32) -> c_int;
        #[link_name = "pl_str_print_int64"]
        fn abi_print_int64(buf: *mut u8, len: usize, value: i64) -> c_int;
        #[link_name = "pl_str_print_uint64"]
        fn abi_print_uint64(buf: *mut u8, len: usize, value: u64) -> c_int;
        #[link_name = "pl_str_print_float"]
        fn abi_print_float(buf: *mut u8, len: usize, value: f32) -> c_int;
        #[link_name = "pl_str_print_double"]
        fn abi_print_double(buf: *mut u8, len: usize, value: f64) -> c_int;
        #[link_name = "pl_str_parse_hex"]
        fn abi_parse_hex(value: PlStr, out: *mut u16) -> bool;
        #[link_name = "pl_str_parse_int"]
        fn abi_parse_int(value: PlStr, out: *mut i32) -> bool;
        #[link_name = "pl_str_parse_uint"]
        fn abi_parse_uint(value: PlStr, out: *mut u32) -> bool;
        #[link_name = "pl_str_parse_int64"]
        fn abi_parse_int64(value: PlStr, out: *mut i64) -> bool;
        #[link_name = "pl_str_parse_uint64"]
        fn abi_parse_uint64(value: PlStr, out: *mut u64) -> bool;
        #[link_name = "pl_str_parse_float"]
        fn abi_parse_float(value: PlStr, out: *mut f32) -> bool;
        #[link_name = "pl_str_parse_double"]
        fn abi_parse_double(value: PlStr, out: *mut f64) -> bool;
    }

    fn printed_f64(value: f64) -> String {
        let mut output = [0u8; 32];
        // SAFETY: `output` is valid for its declared length.
        let len = unsafe { print_double_impl(output.as_mut_ptr(), output.len(), value) };
        str::from_utf8(&output[..len as usize]).unwrap().to_owned()
    }

    fn printed_i64(value: i64) -> String {
        let mut output = [0u8; 32];
        // SAFETY: `output` is valid for its declared length.
        let len = unsafe { print_int64_impl(output.as_mut_ptr(), output.len(), value) };
        str::from_utf8(&output[..len as usize]).unwrap().to_owned()
    }

    fn pl_str(value: &str) -> PlStr {
        PlStr {
            buf: value.as_ptr().cast_mut(),
            len: value.len(),
        }
    }

    #[test]
    fn exported_symbols_match_the_libplacebo_c_abi() {
        assert_eq!(
            std::mem::size_of::<PlStr>(),
            2 * std::mem::size_of::<usize>()
        );

        let mut output = [0u8; 32];
        // SAFETY: Every destination and output value satisfies the declared C ABI.
        unsafe {
            assert!(abi_print_hex(output.as_mut_ptr(), output.len(), 0xff) > 0);
            assert!(abi_print_int(output.as_mut_ptr(), output.len(), -1) > 0);
            assert!(abi_print_uint(output.as_mut_ptr(), output.len(), 1) > 0);
            assert!(abi_print_int64(output.as_mut_ptr(), output.len(), -1) > 0);
            assert!(abi_print_uint64(output.as_mut_ptr(), output.len(), 1) > 0);
            assert!(abi_print_float(output.as_mut_ptr(), output.len(), 1.5) > 0);
            assert!(abi_print_double(output.as_mut_ptr(), output.len(), 1.5) > 0);

            let mut hex = 0u16;
            let mut int = 0i32;
            let mut uint = 0u32;
            let mut int64 = 0i64;
            let mut uint64 = 0u64;
            let mut float = 0.0f32;
            let mut double = 0.0f64;
            assert!(abi_parse_hex(pl_str("ff"), &mut hex));
            assert!(abi_parse_int(pl_str("-1"), &mut int));
            assert!(abi_parse_uint(pl_str("1"), &mut uint));
            assert!(abi_parse_int64(pl_str("-1"), &mut int64));
            assert!(abi_parse_uint64(pl_str("1"), &mut uint64));
            assert!(abi_parse_float(pl_str("1.5"), &mut float));
            assert!(abi_parse_double(pl_str("1.5"), &mut double));
            assert_eq!(
                (hex, int, uint, int64, uint64, float, double),
                (0xff, -1, 1, -1, 1, 1.5, 1.5)
            );
        }
    }

    #[test]
    fn formats_integer_boundaries_without_allocation() {
        assert_eq!(printed_i64(i64::MIN), i64::MIN.to_string());
        assert_eq!(printed_i64(i64::MAX), i64::MAX.to_string());
        assert_eq!(printed_i64(0), "0");

        let mut output = [0u8; 8];
        // SAFETY: `output` is valid for its declared length.
        let len = unsafe { print_hex_impl(output.as_mut_ptr(), output.len(), u16::MAX) };
        assert_eq!(&output[..len as usize], b"ffff");
    }

    #[test]
    fn formats_libplacebo_float_spellings() {
        assert_eq!(printed_f64(1.0), "1.0");
        assert_eq!(printed_f64(0.0), "0.0");
        assert_eq!(printed_f64(-0.0), "-0.0");
        assert_eq!(printed_f64(3_000_000_000.0), "3000000000.0");
        assert_eq!(printed_f64(4_294_967_295.56), "4294967295.56");
        assert_eq!(printed_f64(83_224_965_647_295.65), "83224965647295.66");
        assert_eq!(printed_f64(f64::INFINITY), "inf");
        assert_eq!(printed_f64(f64::NEG_INFINITY), "-inf");
        assert_eq!(printed_f64(f64::NAN), "nan");
    }

    #[test]
    fn rejects_undersized_or_null_output() {
        let mut output = [0xa5u8; 2];
        // SAFETY: The first destination is valid but short; null is explicitly supported.
        assert_eq!(
            unsafe { print_uint_impl(output.as_mut_ptr(), output.len(), 123) },
            0
        );
        assert_eq!(output, [0xa5; 2]);
        assert_eq!(unsafe { print_uint_impl(ptr::null_mut(), 8, 1) }, 0);

        let mut exact = [0u8; 3];
        assert_eq!(
            unsafe { print_uint_impl(exact.as_mut_ptr(), exact.len(), 123) },
            3
        );
        assert_eq!(&exact, b"123");
    }

    #[test]
    fn parses_complete_locale_independent_numbers() {
        let mut signed = 7;
        // SAFETY: Each output pointer addresses the expected type.
        assert!(unsafe { parse_int_impl(pl_str("-2147483648"), &mut signed) });
        assert_eq!(signed, i32::MIN);

        let mut unsigned = 7;
        assert!(unsafe { parse_uint64_impl(pl_str("18446744073709551615"), &mut unsigned) });
        assert_eq!(unsigned, u64::MAX);

        let mut float = 7.0;
        assert!(unsafe { parse_float_impl(pl_str("-3.14e20"), &mut float) });
        assert_eq!(float, -3.14e20);
        assert!(unsafe { parse_float_impl(pl_str("inf"), &mut float) });
        assert_eq!(float, f32::INFINITY);
        assert!(unsafe { parse_float_impl(pl_str("-INFINITY"), &mut float) });
        assert_eq!(float, f32::NEG_INFINITY);
    }

    #[test]
    fn rejects_malformed_and_out_of_range_numbers_without_writing() {
        for value in ["", "+1", " 1", "1 ", "1x", "4294967296"] {
            let mut output = 99u32;
            // SAFETY: `output` addresses the expected type.
            assert!(
                !unsafe { parse_uint_impl(pl_str(value), &mut output) },
                "{value:?}"
            );
            assert_eq!(output, 99);
        }

        for value in ["1e999", "1e-999"] {
            let mut output = 99.0f64;
            // SAFETY: `output` addresses the expected type.
            assert!(
                !unsafe { parse_double_impl(pl_str(value), &mut output) },
                "{value:?}"
            );
            assert_eq!(output, 99.0);
        }

        let invalid_utf8 = [0xff];
        let mut output = 99i32;
        assert!(!unsafe {
            parse_int_impl(
                PlStr {
                    buf: invalid_utf8.as_ptr().cast_mut(),
                    len: invalid_utf8.len(),
                },
                &mut output,
            )
        });
        assert_eq!(output, 99);

        assert!(!unsafe {
            parse_int_impl(
                PlStr {
                    buf: ptr::null_mut(),
                    len: 0,
                },
                &mut output,
            )
        });
        assert!(!unsafe { parse_int_impl(pl_str("1"), ptr::null_mut()) });
    }

    #[test]
    fn finite_float_corpus_round_trips_bit_exactly() {
        let mut state = 0x6a09_e667_f3bc_c909u64;
        let mut output = [0u8; 32];
        for _ in 0..100_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let value = f64::from_bits(state);
            if !value.is_finite() {
                continue;
            }

            // SAFETY: `output` and `parsed` satisfy their respective ABI contracts.
            let len = unsafe { print_double_impl(output.as_mut_ptr(), output.len(), value) };
            assert!(len > 0);
            let mut parsed = 0.0;
            assert!(unsafe {
                parse_double_impl(
                    PlStr {
                        buf: output.as_mut_ptr(),
                        len: len as usize,
                    },
                    &mut parsed,
                )
            });
            assert_eq!(parsed.to_bits(), value.to_bits());
        }

        let mut state = 0x243f_6a88u32;
        for _ in 0..100_000 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let value = f32::from_bits(state);
            if !value.is_finite() {
                continue;
            }

            let len = unsafe { print_float_impl(output.as_mut_ptr(), output.len(), value) };
            assert!(len > 0);
            let mut parsed = 0.0;
            assert!(unsafe {
                parse_float_impl(
                    PlStr {
                        buf: output.as_mut_ptr(),
                        len: len as usize,
                    },
                    &mut parsed,
                )
            });
            assert_eq!(parsed.to_bits(), value.to_bits());
        }
    }

    #[test]
    #[ignore = "manual same-host comparison of the C++ and Rust ABI implementations"]
    fn abi_conversion_throughput() {
        const LUT_VALUES: usize = 65 * 65 * 65 * 3;
        const HOT_INTEGER_FORMATS: u64 = 10_000_000;
        let input = pl_str("0.1234567");
        let mut parsed = 0.0f32;
        let parse_start = Instant::now();
        for _ in 0..LUT_VALUES {
            // SAFETY: Static input and stack output satisfy the libplacebo ABI.
            assert!(unsafe { abi_parse_float(black_box(input), black_box(&mut parsed)) });
        }
        let parse_elapsed = parse_start.elapsed();

        let mut output = [0u8; 32];
        let format_start = Instant::now();
        for index in 0..1_000_000u64 {
            let value = black_box(index as f64 / 997.0);
            // SAFETY: `output` satisfies the libplacebo ABI.
            assert!(unsafe { abi_print_double(output.as_mut_ptr(), output.len(), value) } > 0);
        }
        let format_elapsed = format_start.elapsed();

        let hex_start = Instant::now();
        for index in 0..HOT_INTEGER_FORMATS {
            // Shader identifiers use libplacebo's `%hx` formatter.
            let value = black_box(index as u16);
            // SAFETY: `output` satisfies the libplacebo ABI.
            assert!(unsafe { abi_print_hex(output.as_mut_ptr(), output.len(), value) } > 0);
        }
        let hex_elapsed = hex_start.elapsed();

        let small_int_start = Instant::now();
        for index in 0..HOT_INTEGER_FORMATS {
            // Shader dimensions, component indices, and offsets are small signed values.
            let value = black_box((index % 16_385) as i32 - 8_192);
            // SAFETY: `output` satisfies the libplacebo ABI.
            assert!(unsafe { abi_print_int(output.as_mut_ptr(), output.len(), value) } > 0);
        }
        let small_int_elapsed = small_int_start.elapsed();

        let uint_lut_start = Instant::now();
        for index in 0..LUT_VALUES as u64 {
            // Integer LUT serialization commonly handles full-range 16-bit samples.
            let value = black_box(index.wrapping_mul(40_503) as u16 as u32);
            // SAFETY: `output` satisfies the libplacebo ABI.
            assert!(unsafe { abi_print_uint(output.as_mut_ptr(), output.len(), value) } > 0);
        }
        let uint_lut_elapsed = uint_lut_start.elapsed();

        eprintln!(
            "libplacebo numeric ABI: {LUT_VALUES} float parses in {parse_elapsed:?}; \
             1000000 float formats in {format_elapsed:?}; \
             {HOT_INTEGER_FORMATS} shader IDs in {hex_elapsed:?}; \
             {HOT_INTEGER_FORMATS} small ints in {small_int_elapsed:?}; \
             {LUT_VALUES} uint LUT samples in {uint_lut_elapsed:?}"
        );
        black_box((parsed, output));
    }
}
