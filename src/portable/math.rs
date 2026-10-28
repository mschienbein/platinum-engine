//! Portable equivalents of the libSystem math primitives the engine calls.
//!
//! The reference is macOS arm64 libSystem under round-to-nearest-even. The
//! binary32 unary entry points are compared over all 2^32 bit patterns; the
//! binary32 binary and binary64 entry points use deterministic billion-input
//! samples, including IEEE exceptional values and engine/codec domains. Both
//! FZ=0 and FZ=1 are exercised. Exact commands, counts, seeds, mismatch
//! examples and pinned binaries are retained in
//! `local-artifacts/portable-parity/math-01/`.
//!
//! Multipass, dynamics, transient and limiter processing can call these
//! functions from `engine.rs`'s `with_flush_to_zero` scopes; setup and editor
//! paths also call them outside those scopes. Their results depend on the
//! caller's active flush mode. The separate C ABI in `tools/portable/math/shim`
//! does not change that mode. Other IEEE rounding modes are not verified.
//!
//! Linux x86_64 and aarch64 comparisons use a Mac-generated raw-bit corpus;
//! those sampled comparisons do not prove engine-wide byte identity or the
//! behavior of arbitrary operations outside these primitives.

// Each entry is the binary64 encoding of 2^(k/128), less k in its
// exponent-position. Adding the full signed index there supplies both the
// integer power of two and the fractional table value without multiplication.
const EXP2_TABLE: [u64; 128] = [
    0x3ff0000000000000, 0x3feff63da9fb3335, 0x3fefec9a3e778061, 0x3fefe315e86e7f85,
    0x3fefd9b0d3158574, 0x3fefd06b29ddf6de, 0x3fefc74518759bc8, 0x3fefbe3ecac6f383,
    0x3fefb5586cf9890f, 0x3fefac922b7247f7, 0x3fefa3ec32d3d1a2, 0x3fef9b66affed31b,
    0x3fef9301d0125b51, 0x3fef8abdc06c31cc, 0x3fef829aaea92de0, 0x3fef7a98c8a58e51,
    0x3fef72b83c7d517b, 0x3fef6af9388c8dea, 0x3fef635beb6fcb75, 0x3fef5be084045cd4,
    0x3fef54873168b9aa, 0x3fef4d5022fcd91d, 0x3fef463b88628cd6, 0x3fef3f49917ddc96,
    0x3fef387a6e756238, 0x3fef31ce4fb2a63f, 0x3fef2b4565e27cdd, 0x3fef24dfe1f56381,
    0x3fef1e9df51fdee1, 0x3fef187fd0dad990, 0x3fef1285a6e4030b, 0x3fef0cafa93e2f56,
    0x3fef06fe0a31b715, 0x3fef0170fc4cd831, 0x3feefc08b26416ff, 0x3feef6c55f929ff1,
    0x3feef1a7373aa9cb, 0x3feeecae6d05d866, 0x3feee7db34e59ff7, 0x3feee32dc313a8e5,
    0x3feedea64c123422, 0x3feeda4504ac801c, 0x3feed60a21f72e2a, 0x3feed1f5d950a897,
    0x3feece086061892d, 0x3feeca41ed1d0057, 0x3feec6a2b5c13cd0, 0x3feec32af0d7d3de,
    0x3feebfdad5362a27, 0x3feebcb299fddd0d, 0x3feeb9b2769d2ca7, 0x3feeb6daa2cf6642,
    0x3feeb42b569d4f82, 0x3feeb1a4ca5d920f, 0x3feeaf4736b527da, 0x3feead12d497c7fd,
    0x3feeab07dd485429, 0x3feea9268a5946b7, 0x3feea76f15ad2148, 0x3feea5e1b976dc09,
    0x3feea47eb03a5585, 0x3feea34634ccc320, 0x3feea23882552225, 0x3feea155d44ca973,
    0x3feea09e667f3bcd, 0x3feea012750bdabf, 0x3fee9fb23c651a2f, 0x3fee9f7df9519484,
    0x3fee9f75e8ec5f74, 0x3fee9f9a48a58174, 0x3fee9feb564267c9, 0x3feea0694fde5d3f,
    0x3feea11473eb0187, 0x3feea1ed0130c132, 0x3feea2f336cf4e62, 0x3feea427543e1a12,
    0x3feea589994cce13, 0x3feea71a4623c7ad, 0x3feea8d99b4492ed, 0x3feeaac7d98a6699,
    0x3feeace5422aa0db, 0x3feeaf3216b5448c, 0x3feeb1ae99157736, 0x3feeb45b0b91ffc6,
    0x3feeb737b0cdc5e5, 0x3feeba44cbc8520f, 0x3feebd829fde4e50, 0x3feec0f170ca07ba,
    0x3feec49182a3f090, 0x3feec86319e32323, 0x3feecc667b5de565, 0x3feed09bec4a2d33,
    0x3feed503b23e255d, 0x3feed99e1330b358, 0x3feede6b5579fdbf, 0x3feee36bbfd3f37a,
    0x3feee89f995ad3ad, 0x3feeee07298db666, 0x3feef3a2b84f15fb, 0x3feef9728de5593a,
    0x3feeff76f2fb5e47, 0x3fef05b030a1064a, 0x3fef0c1e904bc1d2, 0x3fef12c25bd71e09,
    0x3fef199bdd85529c, 0x3fef20ab5fffd07a, 0x3fef27f12e57d14b, 0x3fef2f6d9406e7b5,
    0x3fef3720dcef9069, 0x3fef3f0b555dc3fa, 0x3fef472d4a07897c, 0x3fef4f87080d89f2,
    0x3fef5818dcfba487, 0x3fef60e316c98398, 0x3fef69e603db3285, 0x3fef7321f301b460,
    0x3fef7c97337b9b5f, 0x3fef864614f5a129, 0x3fef902ee78b3ff6, 0x3fef9a51fbc74c83,
    0x3fefa4afa2a490da, 0x3fefaf482d8e67f1, 0x3fefba1bee615a27, 0x3fefc52b376bba97,
    0x3fefd0765b6e4540, 0x3fefdbfdad9cbe14, 0x3fefe7c1819e90d8, 0x3feff3c22b8f71f1,
];

// ARM64 flushes a narrowing result that is tiny before rounding. SSE can
// retain MIN_POSITIVE when that same tiny value rounds up, so correct that
// boundary only on x86 while FTZ is active.
#[inline]
fn narrow_f32(value: f64) -> f32 {
    let result = value as f32;
    #[cfg(target_arch = "x86_64")]
    {
        let bits = result.to_bits();
        if bits & 0x7fff_ffff == 0x0080_0000
            && value.to_bits() & 0x7fff_ffff_ffff_ffff < 0x3810_0000_0000_0000
            && (std::hint::black_box(f32::MIN_POSITIVE) * std::hint::black_box(0.5_f32))
                .to_bits() & 0x7fff_ffff == 0
        {
            return f32::from_bits(bits & 0x8000_0000);
        }
    }
    result
}

// ARM propagates a signaling NaN ahead of either quiet NaN, then chooses
// the first quiet NaN. That differs from the x86 SSE operand precedence.
#[inline]
fn propagate_nan32(first: u32, second: u32) -> f32 {
    let first_abs = first & 0x7fff_ffff;
    let second_abs = second & 0x7fff_ffff;
    let selected = if first_abs > 0x7f80_0000 && first_abs & 0x0040_0000 == 0 {
        first
    } else if second_abs > 0x7f80_0000 && second_abs & 0x0040_0000 == 0 {
        second
    } else if first_abs > 0x7f80_0000 {
        first
    } else {
        second
    };
    f32::from_bits(selected | 0x0040_0000)
}

#[inline]
fn trig_nan32(bits: u32) -> f32 {
    if bits & 0x7fff_ffff == 0x7f80_0000 {
        f32::from_bits(0x7fc0_0000)
    } else {
        f32::from_bits(bits | 0x0040_0000)
    }
}

#[inline]
fn trig_nan64(bits: u64) -> f64 {
    if bits & 0x7fff_ffff_ffff_ffff == 0x7ff0_0000_0000_0000 {
        f64::from_bits(0x7ff8_0000_0000_0000)
    } else {
        f64::from_bits(bits | 0x0008_0000_0000_0000)
    }
}

#[inline]
fn exp_reduced(index_bits: u64, remainder: f64, quadratic: f64, linear: f64) -> f32 {
    let table = EXP2_TABLE[(index_bits & 127) as usize];
    let scale = f64::from_bits(table.wrapping_add(index_bits << 45));
    let polynomial = quadratic.mul_add(remainder, linear) * remainder;
    narrow_f32(polynomial.mul_add(scale, scale))
}

#[inline]
fn exp_outside_range(value: f32) -> f32 {
    if value.is_sign_negative() && !value.is_nan() {
        0.0
    } else {
        value * f32::INFINITY
    }
}

/// Binary32 natural exponential, using the reference's range reduction and
/// fused polynomial. Binary32 subnormal outputs follow the active flush mode.
pub fn expf(value: f32) -> f32 {
    if value.to_bits() & 0x7fff_ffff >= 0x4300_0000 {
        return exp_outside_range(value);
    }
    let x = value as f64;
    let scale = f64::from_bits(0x4067_1547_652b_82fe);
    let bias = f64::from_bits(0x4338_0000_0000_0000);
    let biased = scale.mul_add(x, bias);
    let rounded = biased - bias;
    let remainder = scale.mul_add(x, -rounded);
    exp_reduced(
        biased.to_bits(),
        remainder,
        f64::from_bits(0x3eee_bfbd_ff30_d656),
        f64::from_bits(0x3f76_2e44_53e1_0dae),
    )
}

/// Binary32 base-two exponential, using the reference's range reduction and
/// fused polynomial. Binary32 subnormal outputs follow the active flush mode.
pub fn exp2f(value: f32) -> f32 {
    if value.to_bits() & 0x7fff_ffff >= 0x4380_0000 {
        return exp_outside_range(value);
    }
    let x = value as f64;
    let bias = f64::from_bits(0x42c8_0000_0000_0000);
    let biased = x + bias;
    let remainder = x - (biased - bias);
    exp_reduced(
        biased.to_bits(),
        remainder,
        f64::from_bits(0x3fce_bfbd_ff30_d656),
        f64::from_bits(0x3fe6_2e44_53e1_0dae),
    )
}

/// Binary32 base-ten exponential, using the reference's range reduction and
/// fused polynomial. Binary32 subnormal outputs follow the active flush mode.
pub fn exp10f(value: f32) -> f32 {
    if value.to_bits() & 0x7fff_ffff >= 0x4280_0000 {
        return exp_outside_range(value);
    }
    let scaled = (value as f64) * f64::from_bits(0x407a_934f_0979_a371);
    let truncated = scaled as i64;
    let fraction = scaled - truncated as f64;
    let rounded = if fraction >= 0.5 {
        truncated + 1
    } else if fraction <= -0.5 {
        truncated - 1
    } else {
        truncated
    };
    exp_reduced(
        rounded as u64,
        scaled - rounded as f64,
        f64::from_bits(0x3eee_bfbd_ff30_d656),
        f64::from_bits(0x3f76_2e44_53e1_0dae),
    )
}
const EXP64_LO: [u64; 128] = [
    0x0000000000000000, 0x3ca3a4bc88ab6f0b, 0x3cb5448c4f3a3faa, 0x3cb0cea8d139df85,
    0x3c94ca556a1b2fa8, 0x3cb173b2871f2217, 0x3c687a10f84f53b7, 0x3c9801b99f4f5b18,
    0x3ca106d9ba253401, 0x3c96269675ff4e35, 0x3c462d2d983e6844, 0x3cb4935522e05558,
    0x3cabdebf5fb67959, 0x3cb5245852cc8960, 0x3cadf748a567f8da, 0x3c8846f8d141b1d0,
    0x3cb24349c9dfc205, 0x3caee14f25e66832, 0x3c93dd2c819e0908, 0x3ca92b6b9f4c306a,
    0x3ca36c3949dfc34e, 0x3cadb25aa6565c95, 0x3c93118ae698d206, 0x3c87c551db68456f,
    0x3ca045c6a6d3eecb, 0x3c90db2bd14acf32, 0x3c877bf4b6a48214, 0x3ca77f3dc394bd27,
    0x3c8b5d8e0885ab1b, 0x3cb390730108bc38, 0x3c93a1fee8c9225c, 0x3c75b2c9eff60911,
    0x3c8bd9046b69ea24, 0x3c900e3e54fb7d75, 0x3c96fc3180aac683, 0x3cae07c0017dfc4a,
    0x3ca8cb357ddac301, 0x3ca3ba556658c94d, 0x3cb12ee4dbdbae86, 0x3ca345c8212d2278,
    0x3c8f31b9fb32e2e4, 0x3ca7378cb7144a8a, 0x3cb1d4e28014ce07, 0x3caf7ec78b0a541a,
    0x3c4bf96ee8afb9fb, 0x3ca0283b91b051a3, 0x3c7637bd8150db7d, 0x3c9cd9f521d33a36,
    0x3c804715f70da237, 0x3c9b942d586961a7, 0x3ca7d36cccde36a5, 0x3caa6a7e10f26745,
    0x3cae59e8ae6a99de, 0x3caab0010b956f3f, 0x3c9bb5c29f573dd8, 0x3c83e81d7478cc39,
    0x3c97a560f05eeddb, 0x3c3dfa723ae1bf17, 0x3c9d2407ce0f6433, 0x3ca71eb321c101b5,
    0x3ca72ffea320da87, 0x3ca9d919a952ca95, 0x3ca2b620c0998aac, 0x3c60a350f225390e,
    0x3ca26e8d1ea7de88, 0x3cae1e012931967d, 0x3cae8b028d2e51bb, 0x3caa098f71bcc9b0,
    0x3cab984227e443e3, 0x3ca7807bd5e512c0, 0x3ca7a16a40378bdb, 0x3c97f77c42eaf147,
    0x3caa5b034b27b244, 0x3c9e308095c8fab2, 0x3c6fa03bd08bcda3, 0x3ca5df63c009a845,
    0x3ca0964d359847c9, 0x3ca8857d98c4418d, 0x3c9e78c3e7b184a0, 0x3c980f72957b0484,
    0x3c956f6cd720fba3, 0x3cabd0cf46f2f2b2, 0x3c842bc69235780f, 0x3c9f715c3d56cd59,
    0x3cabf4d1acf902db, 0x3caa991ae1b50822, 0x3c9f9bcff58b04aa, 0x3ca4f656fe0ffb05,
    0x3c7984806660caa6, 0x3c7582eb35cac3f3, 0x3ca3c8ba7b0b1398, 0x3c938e88295250ef,
    0x3c9e84e556f025f4, 0x3c983bde4707301a, 0x3c8d72d3f1e40a93, 0x3ca4cacf4c4b05e2,
    0x3c9445b6038d7018, 0x3c9ed34583749cd0, 0x3caa2c7497c16670, 0x3c9e0244cd9df3b7,
    0x3ca89f65d4b96070, 0x3c9b9006b1da3a39, 0x3c7e4bb29d39c3a3, 0x3c9b4338a23cfc57,
    0x3c7c08f260a0042a, 0x3c9647d6355e9b74, 0x3c8e1e92a1c04f0d, 0x3c6c920234dbf34d,
    0x3c70e49a8304ef1f, 0x3ca39e87e388d84e, 0x3c9c0ae7070a6c51, 0x3c9b7531d7fbf731,
    0x3c7dc682d68358e2, 0x3ca256cea656ebcd, 0x3c95e51917091778, 0x3c8d2ec7bfc90fd9,
    0x3ca1d72cedf8d91c, 0x3c9ed88162f63e73, 0x3c7de1319abb2ae2, 0x3c8c86cfb6cd46e4,
    0x3c9926f89159b0a1, 0x3c9a89ed8a4d4206, 0x3c963124664aaf80, 0x3c8d19790d732ab4,
    0x3c9309e68da0499f, 0x3c991ff9e4e92cf9, 0x3c80fa4931906b80, 0x3c5b71923d729650,
];

const fn high_table() -> [u64; 128] {
    let mut values = [0; 128];
    let mut index = 0;
    let rounded_up: u128 = 0x2332e0be94fa797f783c0ef8282b782c;
    while index < 128 {
        values[index] = EXP2_TABLE[index] + ((index as u64) << 45)
            - ((rounded_up >> index) & 1) as u64;
        index += 1;
    }
    values
}
const EXP64_HI: [u64; 128] = high_table();

#[inline]
fn outside(value: f64) -> f64 {
    if value.is_sign_negative() && !value.is_nan() { 0.0 } else { value * f64::MAX }
}

#[inline]
fn reduced(index: i64, remainder: f64, correction: f64, base2: bool) -> f64 {
    let j = (index & 127) as usize;
    let scale = f64::from_bits(EXP64_HI[j]);
    let low = f64::from_bits(EXP64_LO[j]);
    let (a, b, c, d, e, low) = if base2 {
        (0x401ef1584af17ecd, 0xbfe13b4d4879e55e, 0x4034ffde7cc02cf4,
         0x4038b31b87eb8904, 0x3f55e52272e0eaec, low)
    } else {
        (0x408ef1584af17ecd, 0xc0513b4d4879e55e, 0x4114ffde7cc02cf4,
         0x4118b31b87eb8904, 0x3d25e52272e0eaec, low * 128.0)
    };
    let first = remainder + f64::from_bits(a);
    let second = remainder + f64::from_bits(b);
    let low = low + correction;
    let r = remainder + low;
    let p = r.mul_add(first, f64::from_bits(c));
    let q = r.mul_add(second, f64::from_bits(d));
    let polynomial = ((r * f64::from_bits(e)) * p) * q;
    let exponent = index >> 7;
    if exponent >= -1022 {
        let rounded = polynomial.mul_add(scale, scale);
        f64::from_bits(rounded.to_bits().wrapping_add((exponent as u64) << 52))
    } else {
        let scaled = f64::from_bits(scale.to_bits().wrapping_add(((exponent + 1022) as u64) << 52));
        let sum = scaled + 1.0;
        let error = scaled - (sum - 1.0);
        let correction = scaled.mul_add(polynomial, error);
        ((sum + correction) - 1.0) * f64::MIN_POSITIVE
    }
}

#[inline]
fn exponential(value: f64, multiplier: f64, multiplier_tail: f64, positive_limit: u64, negative_limit: u64) -> f64 {
    let word = value.to_bits();
    if word >= positive_limit && (word as i64) >= (negative_limit as i64) {
        return outside(value);
    }
    let product = value * multiplier;
    let truncated = product as i64;
    let index = if product < truncated as f64 { truncated - 1 } else { truncated };
    let remainder = product - index as f64;
    let product_error = (-value).mul_add(multiplier, product);
    let correction = value.mul_add(multiplier_tail, -product_error);
    reduced(index, remainder, correction, false)
}

/// Binary64 natural exponential with the system's split range reduction.
/// Subnormal outputs and inputs follow the caller's floating-point flush mode.
pub fn exp(value: f64) -> f64 {
    exponential(value, f64::from_bits(0x40671547652b82fe), f64::from_bits(0x3ce777d0ffda0d24),
                0x40862e42fefa39f0, 0xc0874a0000000000)
}
/// Binary64 base-ten exponential with the system's split range reduction.
/// Subnormal outputs and inputs follow the caller's floating-point flush mode.
pub fn exp10(value: f64) -> f64 {
    exponential(value, f64::from_bits(0x407a934f0979a372), f64::from_bits(0xbd2406db502402ca),
                0x40734413509f79ff, 0xc07439b746e36b53)
}
/// Binary64 base-two exponential with the system's split range reduction.
/// Subnormal outputs and inputs follow the caller's floating-point flush mode.
pub fn exp2(value: f64) -> f64 {
    let word = value.to_bits();
    let magnitude = word & 0x7fff_ffff_ffff_ffff;
    let limit = if word >> 63 == 0 { 0x4090000000000000 } else { 0x4091000000000000 };
    if magnitude >= limit { return outside(value); }
    let index = (value * 128.0) as i64 - (word >> 63) as i64;
    let remainder = value - (index as f64 * 0.0078125);
    reduced(index, remainder, 0.0, true)
}
const LOG_CENTERS: [u64; 128] = [
    0xc05042bd4b9a7c99, 0xc050014332be0033, 0xc04f804ae8d0cd02, 0xc04efec61b011f85,
    0xc04e7df5fe538ab3, 0xc04dfdd89d586e2b, 0xc04d7e6c0abc3579, 0xc04cffae611ad12b,
    0xc04c819dc2d45fe4, 0xc04c043859e2fdb3, 0xc04b877c57b1b070, 0xc04b0b67f4f46810,
    0xc04a8ff971810a5e, 0xc04a152f142981b4, 0xc0499b072a96c6b2, 0xc04921800924dd3b,
    0xc048a8980abfbd32, 0xc048304d90c11fd3, 0xc047b89f02cf2aad, 0xc047418acebbf18f,
    0xc046cb0f6865c8ea, 0xc046552b49986277, 0xc045dfdcf1eeae0e, 0xc0456b22e6b578e5,
    0xc044f6fbb2cec598, 0xc0448365e695d797, 0xc044106017c3eca3, 0xc0439de8e1559f6f,
    0xc0432bfee370ee68, 0xc042baa0c34be1ec, 0xc04249cd2b13cd6c, 0xc041d982c9d52708,
    0xc04169c05363f158, 0xc040fa848044b351, 0xc0408bce0d95fa38, 0xc0401d9bbcfa61d4,
    0xc03f5fd8a9063e35, 0xc03e857d3d361368, 0xc03dac22d3e441d3, 0xc03cd3c712d31109,
    0xc03bfc67a7fff4cc, 0xc03b2602497d5346, 0xc03a5094b54d2828, 0xc0397c1cb13c7ec1,
    0xc038a8980abfbd32, 0xc037d60496cfbb4c, 0xc037046031c79f85, 0xc03633a8bf437ce1,
    0xc03563dc29ffacb2, 0xc03494f863b8df35, 0xc033c6fb650cde51, 0xc032f9e32d5bfdd1,
    0xc0322dadc2ab3497, 0xc03162593186da70, 0xc03097e38ce60649, 0xc02f9c95dc1d1165,
    0xc02e0b1ae8f2fd56, 0xc02c7b528b70f1c5, 0xc02aed391ab6674e, 0xc02960caf9abb7ca,
    0xc027d60496cfbb4c, 0xc0264ce26c067157, 0xc024c560fe68af88, 0xc0233f7cde14cf5a,
    0xc021bb32a600549d, 0xc020387efbca869e, 0xc01d6ebd1f1febfe, 0xc01a6f9c377dd31b,
    0xc0177394c9d958d5, 0xc0147aa07357704f, 0xc01184b8e4c56af8, 0xc00d23afc49139f9,
    0xc00743ee861f3556, 0xc0016a21e20a0a45, 0xbff72c7ba20f7327, 0xbfe720d9c06a835f,
    0x0000000000000000, 0x3ff6fe50b6ef0851, 0x4006e79685c2d22a, 0x40111cd1d5133413,
    0x4016bad3758efd87, 0x401c4dfab90aab5f, 0x4020eb389fa29f9b, 0x4023aa2fdd27f1c3,
    0x402663f6fac91316, 0x402918a16e46335b, 0x402bc84240adabba, 0x402e72ec117fa5b2,
    0x40308c588cda79e4, 0x4031dcd197552b7b, 0x40332ae9e278ae1a, 0x403476a9f983f74d,
    0x4035c01a39fbd688, 0x40370742d4ef027f, 0x40384c2bd02f03b3, 0x40398edd077e70df,
    0x403acf5e2db4ec94, 0x403c0db6cdd94dee, 0x403d49ee4c325970, 0x403e840be74e6a4d,
    0x403fbc16b902680a, 0x4040790adbb03009, 0x40411307dad30b76, 0x4041ac05b291f070,
    0x40424407ab0e073a, 0x4042db10fc4d9aaf, 0x40437124cea4cded, 0x404406463b1b0449,
    0x40449a784bcd1b8b, 0x40452dbdfc4c96b3, 0x4045c01a39fbd688, 0x4046518fe4677ba7,
    0x4046e221cd9d0cde, 0x404771d2ba7efb3c, 0x404800a563161c54, 0x40488e9c72e0b226,
    0x40491bba891f1709, 0x4049a802391e232f, 0x404a33760a7f6051, 0x404abe18797f1f49,
    0x404b47ebf73882a1, 0x404bd0f2e9e79031, 0x404c592fad295b56, 0x404ce0a4923a587d,
    0x404d6753e032ea0f, 0x404ded3fd442364c, 0x404e726aa1e754d2, 0x404ef6d67328e220,
];
const fn reciprocals() -> [f64; 128] {
    let mut result = [0.0; 128];
    let mut i = 0;
    while i < 128 {
        result[i] = 1.0 / (f32::from_bits(0x3f34_0000 + ((i as u32) << 16)) as f64);
        i += 1;
    }
    result
}
const RECIPROCALS: [f64; 128] = reciprocals();

// Returns None for a noninteger exponent, Some(true) for an odd integer.
fn integer_parity(magnitude: u32) -> Option<bool> {
    if magnitude < 0x3f80_0000 { return None; }
    if magnitude >= 0x4b80_0000 { return Some(false); }
    let exponent = ((magnitude >> 23) as i32 - 127) as u32;
    let shift = 23 - exponent;
    let significand = (magnitude & 0x007f_ffff) | 0x0080_0000;
    let mask = (1_u32 << shift) - 1;
    if significand & mask != 0 { None } else { Some((significand >> shift) & 1 != 0) }
}

fn multiply_one(x: f32) -> f32 { x * std::hint::black_box(1.0_f32) }

/// Binary32 power, with system-compatible classification and range reduction.
/// The logarithm and exponentiation run under the caller's current flush mode.
pub fn powf(x: f32, y: f32) -> f32 {
    if x == 1.0 { return 1.0; }
    if y == 1.0 { return multiply_one(x); }
    let xb = x.to_bits();
    let yb = y.to_bits();
    let ax = xb & 0x7fff_ffff;
    let ay = yb & 0x7fff_ffff;
    if ay == 0 { return 1.0; }
    if ay > 0x7f80_0000 { return multiply_one(y); }
    if ay == 0x7f80_0000 {
        if ax > 0x7f80_0000 { return multiply_one(x); }
        if ax == 0x3f80_0000 { return 1.0; }
        let grows = (ax > 0x3f80_0000) ^ (yb >> 31 != 0);
        return if grows { f32::INFINITY } else { 0.0 };
    }
    if ax == 0 || ax >= 0x7f80_0000 {
        if ax > 0x7f80_0000 { return multiply_one(x); }
        let odd = integer_parity(ay) == Some(true);
        let signed = f32::from_bits(if odd { xb } else { ax });
        return if yb >> 31 == 0 { signed } else { 1.0 / signed };
    }
    let sign = if xb >> 31 != 0 {
        match integer_parity(ay) {
            None => return f32::NAN,
            Some(true) => 0x8000_0000,
            Some(false) => 0,
        }
    } else { 0 };
    let word = if ax < 0x0080_0000 {
        let highest = 31 - ax.leading_zeros();
        let fraction = (ax << (23 - highest)) & 0x007f_ffff;
        let exponent = highest as i32 - 149;
        (0x3f80_0000 | fraction).wrapping_add((exponent as u32) << 23)
    } else { ax };
    let difference = word.wrapping_sub(0x3f33_8000);
    let index = ((difference >> 16) & 127) as usize;
    let shift = difference & 0xff80_0000;
    let normalized = f32::from_bits(word.wrapping_sub(shift)) as f64;
    let exponent = (shift as i32 >> 16) as f64;
    let remainder = normalized.mul_add(RECIPROCALS[index], -1.0);
    let center = exponent + f64::from_bits(LOG_CENTERS[index]);
    let square = remainder * remainder;
    let upper = remainder.mul_add(f64::from_bits(0xc047_1559_dca8_27eb), f64::from_bits(0x404e_c71c_53b3_b1e8));
    let lower = remainder.mul_add(f64::from_bits(0xc057_1547_6529_654c), f64::from_bits(0x4067_1547_652a_ef42));
    let polynomial = upper.mul_add(square, lower);
    let logarithm = polynomial.mul_add(remainder, center);
    let scaled = ((y as f64) * logarithm).clamp(-32768.0, 32768.0);
    let truncated = scaled as i64;
    let fraction = scaled - truncated as f64;
    let rounded = if fraction >= 0.5 { truncated + 1 } else if fraction <= -0.5 { truncated - 1 } else { truncated };
    let result = exp_reduced(rounded as u64, scaled - rounded as f64,
                                   f64::from_bits(0x3eee_bfbd_ff30_d656),
                                   f64::from_bits(0x3f76_2e44_53e1_0dae));
    f32::from_bits(result.to_bits() ^ sign)
}
// Packed reciprocal/logarithm centers for binary64 range reduction.
const POW64_LOG_TABLE: [(u64, u64); 129] = [
    (0x000000003ff00000, 0x0000000000000000), (0xbc0ebef93fefc068, 0x3f8706adaf949f49),
    (0x3bab0b5d3fef81a8, 0x3f96f6435f8f3269), (0x3c2ec7643fef445d, 0x3fa11d9d6edefdb8),
    (0x3bbbc33d3fef0871, 0x3fa6aa8c69f82995), (0x3b8261bf3feecc32, 0x3fac4a04287d6529),
    (0xbb6669b23fee920f, 0x3fb0e0c68e424ace), (0xbc0ca7313fee56ee, 0x3fb3add69167a803),
    (0xbc20d8513fee1e52, 0x3fb6617ac26d3c85), (0xbbc630d63fede50f, 0x3fb92246d1c79540),
    (0x3b74d1953fedad92, 0x3fbbd24b77dbb164), (0x3c043bb83fed7712, 0x3fbe7af7c8933e03),
    (0x3c6e54c13fed411b, 0x3fc090e9025eddd4), (0x3c1ef8a03fed0cfe, 0x3fc1db051c140737),
    (0xbc117e843fecd75d, 0x3fc33127811d49f6), (0xbc15ffe33feca3c2, 0x3fc47cbbf2792a04),
    (0x3bf265a13fec72bb, 0x3fc5b9eaf8e6028b), (0x3c423d8f3fec404b, 0x3fc70275f1837cd6),
    (0x3c438f233fec0df8, 0x3fc84c8ea3631e5b), (0x3bca3acf3febdc79, 0x3fc9937c528da973),
    (0xbb9531f63febac9a, 0x3fcad1d89ecd0361), (0x3c6563203feb7d57, 0x3fcc0e457f1a4606),
    (0xbc0c163d3feb4e23, 0x3fcd4c6ec7f0fa9b), (0xbbba40e13feb2043, 0x3fce83b51de1e130),
    (0xbbc7e2273feaf2f4, 0x3fcfb929fcb47417), (0xbc1c14b63feac4f8, 0x3fd07aa91b2ca230),
    (0x3bd5ff933fea985d, 0x3fd1150405368efc), (0x3c1cf38b3fea6c72, 0x3fd1adfba4172d24),
    (0x3c054efc3fea41fc, 0x3fd242d294235c3a), (0xbba4d3ba3fea1613, 0x3fd2ddbbfb7f3c85),
    (0xbc5100723fe9ec65, 0x3fd371b8e9222827), (0xbc7cd2013fe9c1f1, 0x3fd4096a3fa76078),
    (0x3bdf75713fe999e1, 0x3fd49976c7cfb747), (0xbc041a1c3fe971c7, 0x3fd52a89b6126155),
    (0xbbd3ca043fe9495a, 0x3fd5bdb10e1494ce), (0x3c039bdd3fe92122, 0x3fd65101a7359962),
    (0x3b93fcad3fe8fa0a, 0x3fd6e11602431b0e), (0x3c3c1b243fe8d2f8, 0x3fd771f64145b29f),
    (0x3c2f706a3fe8ada3, 0x3fd7fd3a0cd1729b), (0x3bd918b83fe887a8, 0x3fd88bc1f2b8fde1),
    (0xbc2608613fe861f5, 0x3fd91a168bad4093), (0xbbfe1d823fe83bac, 0x3fd9ab83601c2e99),
    (0xbc2aee253fe81812, 0x3fda338d65098fbf), (0x3c1a69003fe7f4e7, 0x3fdabab52f19a9cb),
    (0xbc0f037e3fe7d08a, 0x3fdb47463d06a5f0), (0x3c2340b73fe7adab, 0x3fdbcedccd4da020),
    (0xbbed72fc3fe78ac8, 0x3fdc574b4f99ab6d), (0xbc2e1aae3fe7674e, 0x3fdce2d8491411da),
    (0xbc5131033fe74516, 0x3fdd6a3ba17a98be), (0xbc118bbe3fe7237e, 0x3fddefe824a6fe07),
    (0x3c2e68a13fe701f8, 0x3fde760f0bc2be8b), (0x3c3ea09e3fe6e2c1, 0x3fdef3a91e17bbc9),
    (0xbc6f56bb3fe6c240, 0x3fdf77299fcad5de), (0x3c4a86963fe6a196, 0x3fdffc0dbd620500),
    (0xbc25c2883fe6810c, 0x3fe04097743d5c6f), (0x3c7fac703fe66156, 0x3fe081d335d14e25),
    (0x3c33a9f83fe6421b, 0x3fe0c26c86b500df), (0x3c6397df3fe6239f, 0x3fe101d26a8d3ec4),
    (0x3c4eed683fe6057d, 0x3fe140d32757ef1b), (0xbbdc85393fe5e6c8, 0x3fe181602d908fc1),
    (0x3c257ab03fe5c88f, 0x3fe1c1412597d32d), (0x3bc639043fe5ac59, 0x3fe1fd31c7e401e5),
    (0x3c5c090a3fe58e2f, 0x3fe23d9f51b99eb2), (0x3c167a3e3fe572b0, 0x3fe278a8c472c40f),
    (0xbbf2e2123fe5552c, 0xbfda8f468bea6bab), (0x3c50b09f3fe538c4, 0xbfda1401a35e2207),
    (0x3c3d724e3fe51da3, 0xbfd99dad548d5854), (0x3bdb2df33fe5014f, 0xbfd9217b478247c6),
    (0x3c7dc05c3fe4e5f6, 0xbfd8a8f65b8148dc), (0xbc6280263fe4cb44, 0xbfd832b8e69895b4),
    (0xbc42ec4d3fe4af97, 0xbfd7b78304d09278), (0x3be07f8f3fe494df, 0xbfd73ff3131de57a),
    (0x3c5aa8043fe47a6e, 0xbfd6c907a7936f76), (0xbc23a9d13fe46037, 0xbfd6528920cd0775),
    (0x3c157e153fe44758, 0xbfd5e190433aef0e), (0xbc1c5e8c3fe42d10, 0xbfd56998ac049478),
    (0xbc26d1ef3fe414a2, 0xbfd4f9884d6cfb2f), (0x3c57b5cf3fe3faa2, 0xbfd481adba3bc479),
    (0x3c4c44373fe3e1a4, 0xbfd40de517e6879c), (0xbc702b7b3fe3c990, 0xbfd39dce8d0c6f05),
    (0x3c7803ce3fe3b1bb, 0xbfd32e56a8c00a1e), (0xbc76ff603fe398b0, 0xbfd2b8a31c011109),
    (0x3bd74b4a3fe380a5, 0xbfd24714e481adf7), (0x3bd6010c3fe36a2c, 0xbfd1dc7279db548f),
    (0x3c48b8973fe351dc, 0xbfd16889c7153834), (0xbc2a84b23fe339e9, 0xbfd0f5cd7fa7752c),
    (0xbbd88c373fe323ac, 0xbfd08ac35435c943), (0xbbc6fbf93fe30c53, 0xbfd019dc059d0e54),
    (0x3bf111ca3fe2f6db, 0xbfcf6320a3e9eb20), (0xbbfb7cc43fe2e05b, 0xbfce87862d120f6b),
    (0xbbc4f1023fe2ca3a, 0xbfcdae8b05ec1659), (0x3c056e0a3fe2b461, 0xbfccd75698d83d8e),
    (0x3bf96f3e3fe29f22, 0xbfcc051d7f25d8c7), (0xbbec0f453fe28808, 0xbfcb1f77901c26d4),
    (0xbbf3bc703fe27389, 0xbfca52c7fa334e41), (0xbbbf1f003fe25d32, 0xbfc972ab200ae538),
    (0xbc6a2c6d3fe249c6, 0xbfc8aef62d76aa72), (0x3c41beab3fe233c6, 0xbfc7d04afe225801),
    (0xbc54f6153fe21f57, 0xbfc70088de1f5b2d), (0x3c290e853fe20b41, 0xbfc6336ad8a92825),
    (0xbba6ad5e3fe1f655, 0xbfc55cd01eaa9487), (0x3bc5753a3fe1e203, 0xbfc48b713902f7fe),
    (0xbbdbbf393fe1ce6e, 0xbfc3c0cc1d1b1692), (0xbb8ae1d33fe1bb6e, 0xbfc2fb577a193876),
    (0xbbd4d76f3fe1a8ac, 0xbfc2379731e9df8f), (0x3c0bb1b03fe194f0, 0xbfc168c50866af19),
    (0x3c3a48a53fe18154, 0xbfc09a5bcb833e9e), (0x3ba5a0983fe16ed7, 0xbfbfadd6b7f005b1),
    (0x3c0b41903fe15b12, 0xbfbe0a139ddec9d0), (0xbc4080e43fe14803, 0xbfbc73a3428818bc),
    (0x3bc9b8cb3fe1358a, 0xbfbae8051978c11b), (0xbc2d8b833fe12303, 0xbfb9598fa7feff2b),
    (0xbba7a1293fe11141, 0xbfb7da11dd576bbe), (0xbbe388f73fe0fe07, 0xbfb63919388d8212),
    (0x3bedc80c3fe0ed3a, 0xbfb4cb396209a15a), (0x3c597adf3fe0da57, 0xbfb32e39f0a97dd1),
    (0x3bfa2e1c3fe0ca09, 0xbfb1c83bbb91b133), (0x3b44b8943fe0b80b, 0xbfb03b9bf822af8e),
    (0x3bc4ae6c3fe0a76c, 0xbfad977378a15b0c), (0xbc473d493fe095dd, 0xbfaa8b0f9d7eed2f),
    (0x3bfc4c603fe084a8, 0xbfa78b28a5f18eb0), (0xbbc08d1e3fe07294, 0xbfa461023c6c2b3f),
    (0x3b9e5abc3fe062fa, 0xbfa1a30ab127d27f), (0xbbf9605e3fe05187, 0xbf9d1dafa56778af),
    (0x3bcf176c3fe04027, 0xbf96f5700660f22d), (0x3c1200bd3fe02fe4, 0xbf912c35b1e2912e),
    (0xbc1b6a1c3fe01f94, 0xbf86b1027d8347ea), (0x3b806a723fe00f11, 0xbf75b2440dc383d5),
    (0x000000003fe00000, 0x0000000000000000),
];
// Correction from the rounded power-of-two center to its exact logarithm.
const POW64_EXP_CORRECTION: [u64; 128] = [
    0x0000000000000000, 0xbd13a4bc88ab6f0b, 0x3ce91137d36a5dac, 0x3d079d9482b6e550,
    0xbd04ca556a1b2fa8, 0x3d040ee9a808de1d, 0xbcd87a10f84f53b7, 0xbd0801b99f4f5b18,
    0xbd1106d9ba253401, 0xbd06269675ff4e35, 0xbcb62d2d983e6844, 0x3ce2c399e6b423b5,
    0x3d0ec8769e7386f7, 0x3cc7cf4b389b60d3, 0x3d09a8cab70c1007, 0xbcf846f8d141b1d0,
    0x3cf73c5aec1a0547, 0x3d0673a724b46d55, 0xbd03dd2c819e0908, 0x3d107b9a3e9b6a86,
    0xbd136c3949dfc34e, 0x3d07039d1ea652dc, 0xbd03118ae698d206, 0xbcf7c551db68456f,
    0xbd1045c6a6d3eecb, 0xbd00db2bd14acf32, 0xbcf77bf4b6a48214, 0x3d1063b457a17edb,
    0xbcfb5d8e0885ab1b, 0x3cb5038687737bee, 0xbd03a1fee8c9225c, 0xbce5b2c9eff60911,
    0xbcfbd9046b69ea24, 0xbd000e3e54fb7d75, 0xbd06fc3180aac683, 0x3d005491f94cc017,
    0x3d0a6409cdf25d3a, 0x3d120e5f6745f2e3, 0x3ce9b570deeb4717, 0x3d121abaf1248342,
    0xbcff31b9fb32e2e4, 0x3d0b83c7e85e695f, 0x3cd1c8d3f3908470, 0x3cf452cbd4449600,
    0xbcbbf96ee8afb9fb, 0xbd10283b91b051a3, 0xbce637bd8150db7d, 0xbd0cd9f521d33a36,
    0xbcf04715f70da237, 0xbd0b942d586961a7, 0x3d06c785171c0daa, 0x3d013802b6c9135a,
    0x3cf1f0a88738ea6a, 0x3cffd7a06db1d675, 0xbd0bb5c29f573dd8, 0xbcf3e81d7478cc39,
    0xbd07a560f05eeddb, 0xbcadfa723ae1bf17, 0xbd0d2407ce0f6433, 0x3d04d74bbcd33652,
    0x3d0457f6a88ab15f, 0x3cfd5305865039df, 0x3d0c93b642e08a78, 0xbcd0a350f225390e,
    0x3d0c6cdcb8cfcc24, 0x3ce2cec326ee0b73, 0x3cdbff379bac560e, 0x3cf85304766e7062,
    0x3cf1669aa6aea2d3, 0x3d008a8544a7c319, 0x3cffe19bb8ecd7c4, 0xbd07f77c42eaf147,
    0x3cf39ea4b4e0a91d, 0xbd0e308095c8fab2, 0xbcdfa03bd08bcda3, 0x3d01c4a6d87e2162,
    0x3d0c01c85abda967, 0x3cf79da32bfd8963, 0x3d0e0ce7d31c17b4, 0xbd080f72957b0484,
    0xbd056f6cd720fba3, 0x3cdf530b22c146b2, 0xbcf42bc69235780f, 0x3d0b766e6aa0a1df,
    0x3cd565b633aaadee, 0x3ce44c91b60b1f8d, 0x3d0a58fa485235a1, 0x3cff6ff7608a3078,
    0xbce984806660caa6, 0xbce582eb35cac3f3, 0x3d0125694bb46c7e, 0xbd038e88295250ef,
    0x3d099593455a26b5, 0xbd083bde4707301a, 0xbcfd72d3f1e40a93, 0x3cfb3ad1802c5436,
    0xbd0445b6038d7018, 0x3d07c789b4248453, 0x3ccf667a98d32ac1, 0x3d0801f60c88f9bd,
    0x3ce1eb0404780ad1, 0x3d09df3f08eda153, 0xbcee4bb29d39c3a3, 0x3d0998b2be294a3f,
    0xbcec08f260a0042a, 0xbd0647d6355e9b74, 0xbcfe1e92a1c04f0d, 0xbcdc920234dbf34d,
    0xbce0e49a8304ef1f, 0x3cf7dc824b04c033, 0x3d06d9ab77ff22ef, 0x3d072904080a5c48,
    0xbcedc682d68358e2, 0x3cfaca05497de9c9, 0xbd05e51917091778, 0xbcfd2ec7bfc90fd9,
    0x3cfb2b6b3f445f87, 0x3d02277152860520, 0xbcede1319abb2ae2, 0xbcfc86cfb6cd46e4,
    0x3d070ed6ccceec4b, 0x3d05693ab0188765, 0xbd063124664aaf80, 0xbcfd19790d732ab4,
    0xbd0309e68da0499f, 0x3d05cc2651e079fb, 0xbcf0fa4931906b80, 0xbccb71923d729650,
];

const fn pow64_exp_high() -> [u64; 128] {
    let mut result = [0; 128];
    let mut i = 0;
    while i < 128 {
        result[i] = EXP2_TABLE[i] + ((i as u64) << 45);
        i += 1;
    }
    result
}
const POW64_EXP_HIGH: [u64; 128] = pow64_exp_high();

// None denotes a noninteger; Some(true) is an odd integer.
fn exponent_parity(magnitude: u64) -> Option<bool> {
    if magnitude < 0x3ff0_0000_0000_0000 { return None; }
    if magnitude >= 0x4340_0000_0000_0000 { return Some(false); }
    let exponent = ((magnitude >> 52) as i32 - 1023) as u32;
    let shift = 52 - exponent;
    let significand = (magnitude & 0x000f_ffff_ffff_ffff) | 0x0010_0000_0000_0000;
    let mask = (1_u64 << shift) - 1;
    if significand & mask != 0 { None } else { Some((significand >> shift) & 1 != 0) }
}

#[inline]
fn exp_power(product: f64, error: f64, sign: u64) -> f64 {
    if product > f64::from_bits(0x4100_0400_0000_0000) {
        return f64::from_bits(sign | 0x7ff0_0000_0000_0000);
    }
    if product < f64::from_bits(0xc100_cc00_0000_0000) {
        return f64::from_bits(sign);
    }
    let trunc = product as i64;
    let index = if product < trunc as f64 { trunc - 1 } else { trunc };
    let remainder = product - index as f64;
    let j = (index & 127) as usize;
    let corrected = (error - f64::from_bits(POW64_EXP_CORRECTION[j])) + remainder;
    let p = remainder.mul_add(f64::from_bits(0x3d25_e522_7217_1bcd), f64::from_bits(0x3dc3_b2a8_abda_c87b));
    let p = p.mul_add(remainder, f64::from_bits(0x3e5c_6b08_d783_107a));
    let p = p.mul_add(corrected, f64::from_bits(0x3eee_bfbd_ff82_bda7));
    let p = p.mul_add(corrected, f64::from_bits(0x3f76_2e42_fefa_39ef));
    let p = p * corrected;
    let mask = 0xfff0_0000_0000_0000_u64;
    let first_shift = ((index as u64) << 44) & mask;
    let second_shift = (((index as u64) << 45) & mask).wrapping_sub(first_shift);
    let first = f64::from_bits(POW64_EXP_HIGH[j].wrapping_add(first_shift ^ sign));
    let first = first.mul_add(p, first);
    let second = f64::from_bits(second_shift.wrapping_add(0x3ff0_0000_0000_0000));
    first * second
}

/// Binary64 power with reference-specific logarithm and exponential reduction.
/// Sampled across arbitrary raw pairs, exceptional pairs, negative integer
/// powers, and codec bases in [0.1,24] with exponents -0.8, 4, 4/3 and 0.75.
/// Inputs and underflow intermediates follow the caller's current flush mode.
pub fn pow(x: f64, y: f64) -> f64 {
    let xb = x.to_bits();
    let yb = y.to_bits();
    let ax = xb & 0x7fff_ffff_ffff_ffff;
    let ay = yb & 0x7fff_ffff_ffff_ffff;
    let inf = 0x7ff0_0000_0000_0000;
    let one = 0x3ff0_0000_0000_0000;
    if xb == one { return x; }
    if yb == one {
        return if ax > inf { f64::from_bits(xb | 0x0008_0000_0000_0000) } else { x };
    }
    if ay == 0 { return 1.0; }
    if ay > inf { return f64::from_bits(yb | 0x0008_0000_0000_0000); }
    if ay == inf {
        if ax > inf { return f64::from_bits(xb | 0x0008_0000_0000_0000); }
        if ax == one { return 1.0; }
        let grows = (ax > one) ^ (yb >> 63 != 0);
        return if grows { f64::INFINITY } else { 0.0 };
    }
    if ax == 0 || ax >= inf {
        if ax > inf { return f64::from_bits(xb | 0x0008_0000_0000_0000); }
        let odd = exponent_parity(ay) == Some(true);
        let signed = f64::from_bits(if odd { xb } else { ax });
        return if yb >> 63 == 0 { signed } else { 1.0 / signed };
    }
    let sign = if xb >> 63 != 0 {
        match exponent_parity(ay) {
            None => return f64::from_bits(0x7ff8_0000_0000_0000),
            Some(true) => 0x8000_0000_0000_0000,
            Some(false) => 0,
        }
    } else { 0 };
    if ay > 0x43f0_0000_0000_0000 {
        if ax == one { return f64::from_bits(one | sign); }
        let grows = (ax > one) ^ (yb >> 63 != 0);
        return f64::from_bits(sign | if grows { inf } else { 0 });
    }
    if ay < 0x3be0_0000_0000_0000 { return f64::from_bits(one | sign); }
    let word = if ax < 0x0010_0000_0000_0000 {
        let highest = 63 - ax.leading_zeros();
        let fraction = (ax << (52 - highest)) & 0x000f_ffff_ffff_ffff;
        let exponent = highest as i32 - 1074;
        (one | fraction).wrapping_add((exponent as u64) << 52)
    } else { ax };
    let fraction = word & 0x000f_ffff_ffff_ffff;
    if fraction == 0 {
        let exponent = (word.wrapping_sub(one) as i64) >> 52;
        let logarithm = (exponent * 128) as f64;
        let product = logarithm * y;
        return exp_power(product, logarithm.mul_add(y, -product), sign);
    }
    let index = ((fraction + (1_u64 << 44)) >> 45) as usize;
    let exponent = (word.wrapping_add(0xc018_1000_0000_0000) as i64) >> 52;
    let (packed, center) = POW64_LOG_TABLE[index];
    let inverse = f64::from_bits((packed as u32 as u64) << 32);
    let center = f64::from_bits(center);
    let center_low = f64::from_bits(packed & 0xffff_ffff_0000_0000);
    let normalized_bits = one | fraction;
    let normalized = f64::from_bits(normalized_bits);
    let normalized_high = f64::from_bits(normalized_bits & 0xffff_ffff_ffe0_0000);
    let normalized_low = normalized - normalized_high;
    let product = inverse * normalized;
    let error = inverse.mul_add(normalized_high, -product);
    let low = inverse.mul_add(normalized_low, error);
    let r = product - 1.0;
    let p = r.mul_add(f64::from_bits(0xbfc7_14b3_12ff_c98e), f64::from_bits(0x3fca_619f_9289_80a6));
    let p = p.mul_add(r, f64::from_bits(0xbfce_c709_dd39_1af8));
    let p = p.mul_add(r, f64::from_bits(0x3fd2_776c_50e2_8d61));
    let p = p.mul_add(r, f64::from_bits(0xbfd7_1547_652b_82c7));
    let r2 = r*r;
    let p = (r2*r2)*p;
    let a = f64::from_bits(0x3fde_c709_dc3a_0400);
    let b = f64::from_bits(0x3c57_46f3_07c0_0c8e);
    let c = f64::from_bits(0xbfe7_1547_652b_82fe);
    let d = f64::from_bits(0xbc67_7aa5_cbc3_6d0f);
    let f = f64::from_bits(0x3ff7_1547_652b_82fe);
    let g = f64::from_bits(0x3c77_77cb_ae0e_d1aa);
    let h = a.mul_add(r,c);
    let lost = a.mul_add(r,-(h-c));
    let l = a.mul_add(low,b.mul_add(r,d)) + lost;
    let h2 = h.mul_add(r,f);
    let l2 = h.mul_add(low,l.mul_add(r,g)) + h.mul_add(r,-(h2-f));
    let h3 = h2*r;
    let l3 = h2.mul_add(r,-h3) + h2.mul_add(low,l2*r);
    let ex = exponent as f64;
    let approximate = ((ex + center) + h3) + p;
    let high = f64::from_bits(approximate.to_bits() & 0xffff_ffff_ffff_fffe);
    let log_low = (center_low + (((ex - high) + center) + h3 + p)) + l3;
    let scaled_y = y * 128.0;
    let product = scaled_y * high;
    let error = scaled_y.mul_add(log_low, scaled_y.mul_add(high,-product));
    exp_power(product,error,sign)
}

// Rounded log((128+k)/128), or log((128+k)/256) for k >= 64.
const LOG_TABLE: [u64; 129] = [
    0x0000000000000000, 0x3f7fe02a6b106789, 0x3f8fc0a8b0fc03e4, 0x3f97b91b07d5b11b,
    0x3f9f829b0e783300, 0x3fa39e87b9febd60, 0x3fa77458f632dcfc, 0x3fab42dd711971bf,
    0x3faf0a30c01162a6, 0x3fb16536eea37ae1, 0x3fb341d7961bd1d1, 0x3fb51b073f06183f,
    0x3fb6f0d28ae56b4c, 0x3fb8c345d6319b21, 0x3fba926d3a4ad563, 0x3fbc5e548f5bc743,
    0x3fbe27076e2af2e6, 0x3fbfec9131dbeabb, 0x3fc0d77e7cd08e59, 0x3fc1b72ad52f67a0,
    0x3fc29552f81ff523, 0x3fc371fc201e8f74, 0x3fc44d2b6ccb7d1e, 0x3fc526e5e3a1b438,
    0x3fc5ff3070a793d4, 0x3fc6d60fe719d21d, 0x3fc7ab890210d909, 0x3fc87fa06520c911,
    0x3fc9525a9cf456b4, 0x3fca23bc1fe2b563, 0x3fcaf3c94e80bff3, 0x3fcbc286742d8cd6,
    0x3fcc8ff7c79a9a22, 0x3fcd5c216b4fbb91, 0x3fce27076e2af2e6, 0x3fcef0adcbdc5936,
    0x3fcfb9186d5e3e2b, 0x3fd0402594b4d041, 0x3fd0a324e27390e3, 0x3fd1058bf9ae4ad5,
    0x3fd1675cababa60e, 0x3fd1c898c16999fb, 0x3fd22941fbcf7966, 0x3fd2895a13de86a3,
    0x3fd2e8e2bae11d31, 0x3fd347dd9a987d55, 0x3fd3a64c556945ea, 0x3fd404308686a7e4,
    0x3fd4618bc21c5ec2, 0x3fd4be5f957778a1, 0x3fd51aad872df82d, 0x3fd5767717455a6c,
    0x3fd5d1bdbf5809ca, 0x3fd62c82f2b9c795, 0x3fd686c81e9b14af, 0x3fd6e08eaa2ba1e4,
    0x3fd739d7f6bbd007, 0x3fd792a55fdd47a2, 0x3fd7eaf83b82afc3, 0x3fd842d1da1e8b17,
    0x3fd89a3386c1425b, 0x3fd8f11e873662c7, 0x3fd947941c2116fb, 0x3fd99d958117e08b,
    0xbfd269621134db92, 0xbfd214456d0eb8d4, 0xbfd1bf99635a6b95, 0xbfd16b5ccbacfb73,
    0xbfd1178e8227e47c, 0xbfd0c42d676162e3, 0xbfd07138604d5862, 0xbfd01eae5626c691,
    0xbfcf991c6cb3b379, 0xbfcef5ade4dcffe6, 0xbfce530effe71012, 0xbfcdb13db0d48940,
    0xbfcd1037f2655e7b, 0xbfcc6ffbc6f00f71, 0xbfcbd087383bd8ad, 0xbfcb31d8575bce3d,
    0xbfca93ed3c8ad9e3, 0xbfc9f6c407089664, 0xbfc95a5adcf7017f, 0xbfc8beafeb38fe8c,
    0xbfc823c16551a3c2, 0xbfc7898d85444c73, 0xbfc6f0128b756abc, 0xbfc6574ebe8c133a,
    0xbfc5bf406b543db2, 0xbfc527e5e4a1b58d, 0xbfc4913d8333b561, 0xbfc3fb45a59928cc,
    0xbfc365fcb0159016, 0xbfc2d1610c86813a, 0xbfc23d712a49c202, 0xbfc1aa2b7e23f72a,
    0xbfc1178e8227e47c, 0xbfc08598b59e3a07, 0xbfbfe89139dbd566, 0xbfbec739830a1120,
    0xbfbda727638446a2, 0xbfbc885801bc4b23, 0xbfbb6ac88dad5b1c, 0xbfba4e7640b1bc38,
    0xbfb9335e5d594989, 0xbfb8197e2f40e3f0, 0xbfb700d30aeac0e1, 0xbfb5e95a4d9791cb,
    0xbfb4d3115d207eac, 0xbfb3bdf5a7d1ee64, 0xbfb2aa04a44717a5, 0xbfb1973bd1465567,
    0xbfb08598b59e3a07, 0xbfaeea31c006b87c, 0xbfaccb73cdddb2cc, 0xbfaaaef2d0fb10fc,
    0xbfa894aa149fb343, 0xbfa67c94f2d4bb58, 0xbfa466aed42de3ea, 0xbfa252f32f8d183f,
    0xbfa0415d89e74444, 0xbf9c63d2ec14aaf2, 0xbf98492528c8cabf, 0xbf9432a925980cc1,
    0xbf90205658935847, 0xbf882448a388a2aa, 0xbf8010157588de71, 0xbf70080559588b35,
    0x0000000000000000,
];

const fn logarithm_reciprocals() -> [f64; 129] {
    let mut table = [0.0; 129];
    let mut index = 0;
    while index < table.len() {
        table[index] = 128.0 / (128 + index) as f64;
        index += 1;
    }
    table
}
const LOG_RECIPROCALS: [f64; 129] = logarithm_reciprocals();

#[inline]
fn normalized_log_input(value: f32) -> Result<(u32, i32), f32> {
    let bits = value.to_bits();
    let magnitude = bits & 0x7fff_ffff;
    if magnitude > 0x7f80_0000 {
        return Err(value);
    }
    if value == 0.0 {
        return Err(f32::NEG_INFINITY);
    }
    if bits >> 31 != 0 {
        return Err(f32::NAN);
    }
    if magnitude == 0x7f80_0000 {
        return Err(value);
    }
    let field = (bits >> 23) as i32;
    if field == 0 {
        let highest = 31 - bits.leading_zeros();
        let fraction = (bits << (23 - highest)) & 0x007f_ffff;
        Ok((fraction, highest as i32 - 149))
    } else {
        Ok((bits & 0x007f_ffff, field - 127))
    }
}

/// Binary32 natural logarithm with the reference's rounded lookup values and
/// fused polynomial. The reference treats subnormal inputs as zero when
/// flush-to-zero is enabled, and returns a NaN argument without changing bits.
pub fn logf(value: f32) -> f32 {
    let (fraction, exponent) = match normalized_log_input(value) {
        Ok(parts) => parts,
        Err(result) => return result,
    };
    let index = ((fraction + 0x8000) >> 16) as usize;
    let exponent = exponent + i32::from(index >= 64);
    let normalized = f64::from_bits(0x3ff0_0000_0000_0000 | ((fraction as u64) << 29));
    let remainder = normalized.mul_add(LOG_RECIPROCALS[index], -1.0);
    let square = remainder * remainder;
    let polynomial = remainder.mul_add(
        f64::from_bits(0x3fd5_5561_555c_cbad),
        f64::from_bits(0xbfe0_0006_0003_49d2),
    );
    let base = (exponent as f64).mul_add(
        f64::from_bits(0x3fe6_2e42_fefa_39ef),
        f64::from_bits(LOG_TABLE[index]),
    );
    let base = remainder.mul_add(f64::from_bits(0x3fef_ffff_ffff_6666), base);
    narrow_f32(square.mul_add(polynomial, base))
}

// Each pair contains a tuned reciprocal and the corresponding logarithm
// center used by the binary32 base-two and base-ten approximations.
const LOG2F_TABLE: [(u64, u64); 65] = [
    (0x3ff0000000000000, 0x0000000000000000),
    (0x3fef81f81f8271f1, 0x3f96e79685ac8265),
    (0x3fef07c1f07d1862, 0x3fa6bad37577cdd3),
    (0x3fee9131abf02f18, 0x3fb0eb389fa90e92),
    (0x3fee1e1e1e1ea60d, 0x3fb663f6fac29027),
    (0x3fedae6076b9258d, 0x3fbbc84240b2284b),
    (0x3fed41d41d412575, 0x3fc08c588cdec84a),
    (0x3fecd8568902a484, 0x3fc32ae9e27ed85b),
    (0x3fec71c71c70f96e, 0x3fc5c01a3a010dd7),
    (0x3fec0e070382101d, 0x3fc84c2bd02cfa24),
    (0x3febacf914c254af, 0x3fcacf5e2db0e9de),
    (0x3feb4e81b4e7482e, 0x3fcd49ee4c37ed36),
    (0x3feaf286bca1e09f, 0x3fcfbc16b9011514),
    (0x3fea98ef606a9dbf, 0x3fd11307dad24218),
    (0x3fea41a41a41297d, 0x3fd24407ab0fb668),
    (0x3fe9ec8e95105eca, 0x3fd37124cea434fc),
    (0x3fe999999998b16f, 0x3fd49a784bd060e8),
    (0x3fe948b0fcd65c4a, 0x3fd5c01a39fddb95),
    (0x3fe8f9c18f9c4c0f, 0x3fd6e221cd9c5005),
    (0x3fe8acb90f6c0b26, 0x3fd800a56315c472),
    (0x3fe861861860d371, 0x3fd91bba8921bb9c),
    (0x3fe81818181771e6, 0x3fda33760a81dd34),
    (0x3fe7d05f417dd809, 0x3fdb47ebf7355417),
    (0x3fe78a4c81793167, 0x3fdc592fad2733c7),
    (0x3fe745d1745df36f, 0x3fdd6753e02f8097),
    (0x3fe702e05c0a9328, 0x3fde726aa1eb10ed),
    (0x3fe6c16c16c19bf9, 0x3fdf7a8568ca4484),
    (0x3fe68168168170b4, 0x3fe03fda8b9787d3),
    (0x3fe642c8590c06b4, 0x3fe0c10500d45f16),
    (0x3fe6058160586486, 0x3fe140c9faa140b1),
    (0x3fe5c9882b93331f, 0x3fe1bf311e95865b),
    (0x3fe58ed23081846a, 0x3fe23c41d426caa7),
    (0x3fe55555555565b2, 0xbfda8ff97181512f),
    (0x3fe51d07eae25f1e, 0xbfd99b072a9429c1),
    (0x3fe4e5e0a72eb123, 0xbfd8a8980abe49ad),
    (0x3fe4afd6a052aff3, 0xbfd7b89f02cee5ec),
    (0x3fe47ae147ae2fd2, 0xbfd6cb0f6866442d),
    (0x3fe446f865628c84, 0xbfd5dfdcf1ed4d52),
    (0x3fe414141413acbe, 0xbfd4f6fbb2ccea65),
    (0x3fe3e22cbce4861e, 0xbfd4106017c34a9b),
    (0x3fe3b13b13b1ac72, 0xbfd32bfee37301f7),
    (0x3fe381381380a51e, 0xbfd249cd2b1115be),
    (0x3fe3521cfb2c1f3a, 0xbfd169c053670ce7),
    (0x3fe323e34a2bfc50, 0xbfd08bce0d9a6a96),
    (0x3fe2f684bda0ad76, 0xbfcf5fd8a9014cc3),
    (0x3fe2c9fb4d803821, 0xbfcdac22d3daded7),
    (0x3fe29e4129e3a3ef, 0xbfcbfc67a7fbaaf8),
    (0x3fe27350b88209ac, 0xbfca5094b55602c6),
    (0x3fe249249249dbc3, 0xbfc8a8980ac6f736),
    (0x3fe21fb781227c5a, 0xbfc7046031ccc0b5),
    (0x3fe1f7047dc01f7d, 0xbfc563dc29f565bb),
    (0x3fe1cf06ada36398, 0xbfc3c6fb65160ac7),
    (0x3fe1a7b9611b19bc, 0xbfc22dadc2b1aabf),
    (0x3fe181181180a9e7, 0xbfc097e38ce17c11),
    (0x3fe15b1e5f74bdc8, 0xbfbe0b1ae8ea3d3a),
    (0x3fe135c811359e80, 0xbfbaed391ab2eb45),
    (0x3fe111111110e212, 0xbfb7d60496cbc247),
    (0x3fe0ecf56be648da, 0xbfb4c560fe618ce8),
    (0x3fe0c9714fbbf1a4, 0xbfb1bb32a5ec5767),
    (0x3fe0a6810a681e3a, 0xbfad6ebd1f224645),
    (0x3fe0842108426a0d, 0xbfa77394c9e8fd24),
    (0x3fe0624dd2f26f2d, 0xbfa184b8e4e82535),
    (0x3fe0410410417bc2, 0xbf9743ee8649b454),
    (0x3fe0204081026480, 0xbf872c7ba2547e55),
    (0x3fe0000000000000, 0x0000000000000000),
];
const LOG10F_TABLE: [(u64, u64); 65] = [
    (0x3ff0000000000000, 0x0000000000000000),
    (0x3fef81f81f81a3e5, 0x3f7b9476a50f6475),
    (0x3fef07c1f07c3108, 0x3f8b5e908eaf3398),
    (0x3fee9131abf09b55, 0x3f945f4f5acd2447),
    (0x3fee1e1e1e1e0ae1, 0x3f9af5f92b020224),
    (0x3fedae6076b8f76b, 0x3fa0ba01a81b0d1d),
    (0x3fed41d41d410288, 0x3fa3ed1199ac1d02),
    (0x3fecd8568902fb3f, 0x3fa714834281fa6c),
    (0x3fec71c71c70f481, 0x3faa30a9d6105e4b),
    (0x3fec0e0703826caa, 0x3fad41d5164a5b23),
    (0x3febacf914c177b9, 0x3fb02428c1f18d98),
    (0x3feb4e81b4e7b01f, 0x3fb1a2344551cc7e),
    (0x3feaf286bca2397c, 0x3fb31b3055c23663),
    (0x3fea98ef60696bd7, 0x3fb48f3ed1e3553a),
    (0x3fea41a41a40b6e9, 0x3fb5fe80488ee159),
    (0x3fe9ec8e95113011, 0x3fb769140a20ed4f),
    (0x3fe9999999992278, 0x3fb8cf1838884d6e),
    (0x3fe948b0fcd69907, 0x3fba30a9d60b536d),
    (0x3fe8f9c18f9b4e11, 0x3fbb8de4d3aec4d9),
    (0x3fe8acb90f6c68a1, 0x3fbce6e41e442e9e),
    (0x3fe861861862484b, 0x3fbe3bc1ab0aa46f),
    (0x3fe8181818173ae4, 0x3fbf8c96834a7e48),
    (0x3fe7d05f417c0774, 0x3fc06cbd67a915cd),
    (0x3fe78a4c81784397, 0x3fc11142f081f8db),
    (0x3fe745d1745de7ba, 0x3fc1b3e71ec75d90),
    (0x3fe702e05c0c18e3, 0x3fc254b4d35d0f5f),
    (0x3fe6c16c16c06c9b, 0x3fc2f3b691c81020),
    (0x3fe681681681a1f6, 0x3fc390f684497c90),
    (0x3fe642c8590aa5ef, 0x3fc42c7e7fe5304e),
    (0x3fe605816057fe92, 0x3fc4c65807e96e6b),
    (0x3fe5c9882b922e81, 0x3fc55e8c518d5130),
    (0x3fe58ed230825723, 0x3fc5f5244722cd0f),
    (0x3fe555555555be48, 0xbfbffbfc2bbe9af4),
    (0x3fe51d07eae25fe5, 0xbfbed50a4a23c997),
    (0x3fe4e5e0a72ea8ca, 0xbfbdb11ed764c030),
    (0x3fe4afd6a051e80f, 0xbfbc902a19e1cbfd),
    (0x3fe47ae147ad26e5, 0xbfbb721cd16c4e1c),
    (0x3fe446f8656250ca, 0xbfba56e8325c6c50),
    (0x3fe4141414137fce, 0xbfb93e7de0f90978),
    (0x3fe3e22cbce5a6b7, 0xbfb828cfed2f2caf),
    (0x3fe3b13b13b04947, 0xbfb715d0ce3125d3),
    (0x3fe3813813803a55, 0xbfb605735ee3df92),
    (0x3fe3521cfb2b1096, 0xbfb4f7aad9b97441),
    (0x3fe323e34a2baef3, 0xbfb3ec6ad5440f5a),
    (0x3fe2f684bda0a126, 0xbfb2e3a740b43dff),
    (0x3fe2c9fb4d8060d5, 0xbfb1dd5460c3fb8b),
    (0x3fe29e4129e3187d, 0xbfb0d966cc5f2b50),
    (0x3fe27350b881d871, 0xbfafafa6d3a047d3),
    (0x3fe249249249e9cf, 0xbfadb11ed7700a68),
    (0x3fe21fb781227951, 0xbfabb7209d242ce5),
    (0x3fe1f7047dc1c36b, 0xbfa9c197abf7fb7a),
    (0x3fe1cf06ada1ee29, 0xbfa7d070145824fe),
    (0x3fe1a7b961199740, 0xbfa5e3966b7356cb),
    (0x3fe181181181a457, 0xbfa3faf7c669fbf0),
    (0x3fe15b1e5f74ca8f, 0xbfa21681b5c42118),
    (0x3fe135c81135efed, 0xbfa0362241e83be8),
    (0x3fe1111111104099, 0xbf9cb38fccc387a4),
    (0x3fe0ecf56be75b6e, 0xbf9902c31d763f47),
    (0x3fe0c9714fbdd2a2, 0xbf9559bd2420786b),
    (0x3fe0a6810a68f2a2, 0xbf91b85d605c7d78),
    (0x3fe084210841501d, 0xbf8c3d08374fd2e0),
    (0x3fe0624dd2f253e0, 0xbf851824c77c8587),
    (0x3fe04104104183ee, 0xbf7c03a80b1c8a49),
    (0x3fe0204081017ac2, 0xbf6be76bd7050239),
    (0x3fe0000000000000, 0x0000000000000000),
];

#[inline]
fn log_base_input(value: f32, table: &[(u64, u64); 65]) -> Result<(f64, f64, f64), f32> {
    let (fraction, exponent) = normalized_log_input(value)?;
    let index = ((fraction + 0x10000) >> 17) as usize;
    let exponent = (exponent + i32::from(index >= 32)) as f64;
    let normalized = f64::from_bits(0x3ff0_0000_0000_0000 | ((fraction as u64) << 29));
    let (reciprocal, center) = table[index];
    let remainder = normalized.mul_add(f64::from_bits(reciprocal), -1.0);
    Ok((remainder, exponent, f64::from_bits(center)))
}

/// Binary32 base-two logarithm. NaN and flush-to-zero behavior is as for
/// [`logf`], with a separate reference-specific approximation.
pub fn log2f(value: f32) -> f32 {
    let (remainder, exponent, center) = match log_base_input(value, &LOG2F_TABLE) {
        Ok(parts) => parts,
        Err(result) => return result,
    };
    let polynomial = remainder.mul_add(
        f64::from_bits(0xbfd7_15a3_c1d1_bbdd),
        f64::from_bits(0x3fde_c753_bab7_b7ce),
    );
    let polynomial = remainder.mul_add(polynomial, f64::from_bits(0xbfe7_1547_64ed_ed82));
    let polynomial = remainder.mul_add(polynomial, f64::from_bits(0x3ff7_1547_6522_4725));
    narrow_f32(exponent + remainder.mul_add(polynomial, center))
}

/// Binary32 base-ten logarithm. NaN and flush-to-zero behavior is as for
/// [`logf`], with a separate reference-specific approximation.
pub fn log10f(value: f32) -> f32 {
    let (remainder, exponent, center) = match log_base_input(value, &LOG10F_TABLE) {
        Ok(parts) => parts,
        Err(result) => return result,
    };
    let polynomial = remainder.mul_add(
        f64::from_bits(0xbfbb_cbea_4c25_8daa),
        f64::from_bits(0x3fc2_87d3_dcaf_476d),
    );
    let polynomial = remainder.mul_add(polynomial, f64::from_bits(0xbfcb_cb7b_14dc_bd85));
    let polynomial = remainder.mul_add(polynomial, f64::from_bits(0x3fdb_cb7b_151b_c6b3));
    let base = exponent.mul_add(f64::from_bits(0x3fd3_4413_509f_79ff), center);
    narrow_f32(remainder.mul_add(polynomial, base))
}
const LOG10_ODD: [(u64,u64);64] = [
    (0x3fefc07f01fb38d4, 0x3f6bafd4727c9364),
    (0x3fef44659e49d2b1, 0x3f849b085150a16d),
    (0x3feecc07b3017601, 0x3f910a83a84b1f39),
    (0x3fee573ac902a11f, 0x3f97adc3df306138),
    (0x3fede5d6e3f7cbaf, 0x3f9e3806acc7e0f9),
    (0x3fed77b654b7b0b1, 0x3fa25502c0ffd549),
    (0x3fed0cb58f6e27bc, 0x3fa58238eeb7e4d6),
    (0x3feca4b3055ed9ba, 0x3fa8a3fadeb884d0),
    (0x3fec3f8f01c3ce79, 0x3fabba9a058f4bc9),
    (0x3febdd2b89930e8e, 0x3faec6647ebd4665),
    (0x3feb7d6c3dd95825, 0x3fb0e3d29d848db3),
    (0x3feb2036406badd7, 0x3fb25f5215eeba23),
    (0x3feac5701ac4d7c0, 0x3fb3d5d335c7aa31),
    (0x3fea6d01a6cfc64f, 0x3fb547773198a06b),
    (0x3fea16d3f97b18da, 0x3fb6b45df6f07593),
    (0x3fe9c2d14ee59e3b, 0x3fb81ca63d015f6c),
    (0x3fe970e4f80c4ba4, 0x3fb9806d94167d88),
    (0x3fe920fb49d1de51, 0x3fbadfd074126264),
    (0x3fe8d3018d2f1d8b, 0x3fbc3aea4a60d466),
    (0x3fe886e5f0abeab2, 0x3fbd91d58669a0d9),
    (0x3fe83c977ab327e6, 0x3fbee4aba60f1033),
    (0x3fe7f405fd00d922, 0x3fc019c2a066360c),
    (0x3fe7ad2208e0dbe0, 0x3fc0bf3d0937ebc3),
    (0x3fe767dce433bafd, 0x3fc162d082aed3fe),
    (0x3fe724287f468d6d, 0x3fc204881def4ac9),
    (0x3fe6e1f76b42428f, 0x3fc2a46e8caa0de1),
    (0x3fe6a13cd1528b5e, 0x3fc3428e2542415b),
    (0x3fe661ec6a5049d6, 0x3fc3def0e6e1face),
    (0x3fe623fa7701094f, 0x3fc479a07d3c4360),
    (0x3fe5e75bb8cfdcfb, 0x3fc512a64429fcb4),
    (0x3fe5ac056b01183e, 0x3fc5aa0b4b0a3393),
    (0x3fe571ed3c514831, 0x3fc63fd857fa0cf1),
    (0x3fe5390948f33af7, 0xbfbf68216c986b90),
    (0x3fe501501501bd58, 0xbfbe42b4c16eed44),
    (0x3fe4cab88726a83c, 0xbfbd204698d07526),
    (0x3fe49539e3b27047, 0xbfbc00c776701e92),
    (0x3fe460cbc7f6ce28, 0xbfbae428550f01d9),
    (0x3fe42d6625d45a4e, 0xbfb9ca5aa16e608d),
    (0x3fe3fb013fb1102f, 0xbfb8b3503651ddb3),
    (0x3fe3c995a47afe53, 0xbfb79efb57ad28ba),
    (0x3fe3991c2c17b662, 0xbfb68d4eaf2263a3),
    (0x3fe3698df3de9d20, 0xbfb57e3d47c709af),
    (0x3fe33ae45b573065, 0xbfb471ba8a7ab790),
    (0x3fe30d190131cd5b, 0xbfb367ba3aafd5ee),
    (0x3fe2e025c04c5430, 0xbfb2603072a73dd8),
    (0x3fe2b404ad01d56c, 0xbfb15b11a098953a),
    (0x3fe288b012898ed6, 0xbfb05852837b79c2),
    (0x3fe25e22707fbbf8, 0xbfaeafd0502b92c7),
    (0x3fe23456789bb267, 0xbfacb38fcce476e8),
    (0x3fe20b470c676cac, 0xbfaabbcebd80ef57),
    (0x3fe1e2ef3b3fd2c9, 0xbfa8c878eeb197cb),
    (0x3fe1bb4a4045ee1f, 0xbfa6d97ab3addfcf),
    (0x3fe19453808caf9f, 0xbfa4eec0e24633c5),
    (0x3fe16e0689430a4c, 0xbfa30838cdca801e),
    (0x3fe1485f0e0bc882, 0xbfa125d0433b42f6),
    (0x3fe12358e75dd419, 0xbf9e8eeb0a0393c4),
    (0x3fe0fef010fe3855, 0xbf9ada2e8e2b8ccc),
    (0x3fe0db20a89031a0, 0xbf972d4956e25992),
    (0x3fe0b7e6ec259ec9, 0xbf93881a7b81aa5e),
    (0x3fe0953f3900460b, 0xbf8fd503c36764ef),
    (0x3fe073260a48f47f, 0xbf88a8c06be7337e),
    (0x3fe05197f7d73005, 0xbf818b2dc8d1e1bf),
    (0x3fe03091b51e60a3, 0xbf74f82051c84636),
    (0x3fe01010100fc088, 0xbf5bd96a1cf3fdd8),
];
const fn centers() -> [(u64,u64);129] {
    let mut table = [(0,0);129];
    let mut i=0;
    while i<129 {
        table[i] = if i & 1 == 0 { LOG10F_TABLE[i/2] } else { LOG10_ODD[i/2] };
        i+=1;
    }
    table
}
const LOG10_TABLE: [(u64,u64);129] = centers();

/// Binary64 base-ten logarithm using reference-specific split reduction.
pub fn log10(value: f64) -> f64 {
    let bits=value.to_bits();
    let magnitude=bits & 0x7fff_ffff_ffff_ffff;
    if magnitude>0x7ff0_0000_0000_0000 { return value; }
    if value==0.0 { return f64::NEG_INFINITY; }
    if bits>>63!=0 { return f64::NAN; }
    if magnitude==0x7ff0_0000_0000_0000 { return value; }
    let field=(bits>>52) as i32;
    let (fraction,exponent)=if field==0 {
        let highest=63-bits.leading_zeros();
        ((bits<<(52-highest)) & 0x000f_ffff_ffff_ffff,highest as i32-1074)
    } else { (bits & 0x000f_ffff_ffff_ffff,field-1023) };
    let index=((fraction+(1_u64<<44))>>45) as usize;
    let exponent=exponent+i32::from(index>=64);
    let (inv,center)=LOG10_TABLE[index];
    let inv=f64::from_bits(inv);
    let center=f64::from_bits(center);
    let normalized=f64::from_bits(0x3ff0_0000_0000_0000 | fraction);
    let r=normalized.mul_add(inv,-1.0);
    let a=r.mul_add(f64::from_bits(0x3faf_c431_f82b_9b9d),f64::from_bits(0xbfb4_9704_e9bc_168e));
    let b=r.mul_add(r+f64::from_bits(0x3ff6_09d1_4c7e_e9be),f64::from_bits(0x3ff9_de61_616c_df2b));
    let c=r.mul_add(r+f64::from_bits(0xbff3_f6ab_6ec1_0818),f64::from_bits(0x3ffa_b7d4_f054_7db7));
    let p=((r*r)*a*b)*c;
    if exponent!=0 {
        let center_high=f64::from_bits(center.to_bits() & 0xffff_ffff_f800_0000);
        let center_low=center-center_high;
        let high=(exponent as f64).mul_add(f64::from_bits(0x3fd3_4413_5000_0000),center_high);
        let low=(exponent as f64)*f64::from_bits(0x3e03_ef3f_de62_3e25);
        ((low + r*f64::from_bits(0x3fdb_cb7b_1526_e50e)) + (center_low+p))+high
    } else {
        let product=inv*normalized;
        let diff=product-1.0;
        let error=inv.mul_add(normalized,-product);
        let r_high=f64::from_bits(r.to_bits() & 0xffff_ffff_f800_0000);
        let r_low=error+(diff-r_high);
        let part=r_high*f64::from_bits(0x3fdb_cb7b_1800_0000);
        let sum=center+part;
        let lost=part-(sum-center);
        let low=r_low.mul_add(f64::from_bits(0x3fdb_cb7b_1800_0000),r*f64::from_bits(0xbe26_c8d7_8e6a_caa4));
        ((low+lost)+p)+sum
    }
}

// Each center stores a shortened reciprocal, a high logarithm part, and
// the low residual of -log(reciprocal), adjusted by log(2) above index 63.
const LOG64_TABLE: [(u32, u32, u64); 129] = [
    (0x3ff00000, 0x00000000, 0x0000000000000000),
    (0x3fefc07f, 0x3f7fe02b, 0x3e1ac419e43f0dda),
    (0x3fef81f8, 0x3f8fc0b1, 0xbe23c0fe070c179f),
    (0x3fef4466, 0x3f97b90f, 0xbe3e0a8ed755c02f),
    (0x3fef07c2, 0x3f9f8299, 0x3e0cf0670099f1a3),
    (0x3feecc08, 0x3fa39e83, 0xbe41804f1815c244),
    (0x3fee9132, 0x3fa77453, 0x3e4d8cb9231189c5),
    (0x3fee573b, 0x3fab42da, 0xbe3773457c1ebb72),
    (0x3fee1e1e, 0x3faf0a33, 0xbe3ff74e2ccf4146),
    (0x3fede5d7, 0x3fb16536, 0xbdf5c84ae979dea4),
    (0x3fedae60, 0x3fb341dc, 0xbe5a79098bdb544a),
    (0x3fed77b6, 0x3fb51b0a, 0x3e3f061c61692f7a),
    (0x3fed41d4, 0x3fb6f0d4, 0xbe5d46a50d1906c4),
    (0x3fed0cb6, 0x3fb8c342, 0xbe239cbaba14cd62),
    (0x3fecd857, 0x3fba9269, 0x3e3a4adde564f46d),
    (0x3feca4b3, 0x3fbc5e55, 0xbe50290e2e1a8a7a),
    (0x3fec71c7, 0x3fbe2708, 0x3e5b8abcd97a7ab7),
    (0x3fec3f8f, 0x3fbfec91, 0x3e5076faaeca8b94),
    (0x3fec0e07, 0x3fc0d77f, 0xbe6ccbdc69665a24),
    (0x3febdd2c, 0x3fc1b729, 0xbe62b424f6f5c1b2),
    (0x3febacf9, 0x3fc29553, 0x3e6607fd51d3017c),
    (0x3feb7d6c, 0x3fc371fd, 0x3e6007a42e0ef3df),
    (0x3feb4e82, 0x3fc44d2a, 0x3e3996fe04cfa0c4),
    (0x3feb2036, 0x3fc526e7, 0x3e43a1b5a0a2e63d),
    (0x3feaf287, 0x3fc5ff2f, 0x3e5853cab1e438a4),
    (0x3feac570, 0x3fc6d610, 0x3e69c67497235528),
    (0x3fea98ef, 0x3fc7ab8b, 0xbe56f791d6f20a52),
    (0x3fea6d02, 0x3fc87f9f, 0xbe62b7cd059bf998),
    (0x3fea41a4, 0x3fc9525b, 0x3e4cf456f476415b),
    (0x3fea16d4, 0x3fca23bc, 0xbdcd4a98e6c8eefa),
    (0x3fe9ec8f, 0x3fcaf3c7, 0x3e5f406219ec6157),
    (0x3fe9c2d1, 0x3fcbc288, 0xbe1e93868cb00b40),
    (0x3fe9999a, 0x3fcc8ff6, 0xbe5c32b0ef29f269),
    (0x3fe970e5, 0x3fcd5c21, 0x3e60d3eee5e6e443),
    (0x3fe948b1, 0x3fce2707, 0x3e678abcb9ba7aa2),
    (0x3fe920fb, 0x3fcef0af, 0x3e60f716d7a48746),
    (0x3fe8f9c2, 0x3fcfb916, 0x3e56af219d46a30b),
    (0x3fe8d302, 0x3fd04024, 0x3e7b2d34bb56b68f),
    (0x3fe8acb9, 0x3fd0a325, 0x3e44e721dfbee7fa),
    (0x3fe886e6, 0x3fd1058c, 0xbe6728da8f33b032),
    (0x3fe86186, 0x3fd1675d, 0xbe545459d1fc6323),
    (0x3fe83c97, 0x3fd1c89a, 0x3e35a674bc6f457a),
    (0x3fe81818, 0x3fd22942, 0x3e6de7bcc2d1214d),
    (0x3fe7f406, 0x3fd2895a, 0x3e47bd0d47bd6926),
    (0x3fe7d05f, 0x3fd2e8e3, 0x3e7ab84788a70ba1),
    (0x3fe7ad22, 0x3fd347de, 0xbe7359e0a9aa6ea6),
    (0x3fe78a4d, 0x3fd3a64b, 0xbe24b5b3cdc6edd8),
    (0x3fe767dd, 0x3fd40430, 0x3e6d4354086e85f7),
    (0x3fe745d1, 0x3fd4618d, 0x3e20e30f13e8af2f),
    (0x3fe72428, 0x3fd4be61, 0xbe4511072e494b13),
    (0x3fe702e0, 0x3fd51aaf, 0xbe7e348174bd8c5b),
    (0x3fe6e1f7, 0x3fd57678, 0x3e70d1574add28d3),
    (0x3fe6c16c, 0x3fd5d1be, 0xbe04fec2b5ee4b93),
    (0x3fe6a13d, 0x3fd62c82, 0x3e7bae720753dba9),
    (0x3fe68168, 0x3fd686c8, 0x3e77a6c533b110b5),
    (0x3fe661ec, 0x3fd6e090, 0xbe62ea2da539f1bf),
    (0x3fe642c8, 0x3fd739d9, 0xbe42885bf2c630bd),
    (0x3fe623fa, 0x3fd792a7, 0xbe7208ad3040f74d),
    (0x3fe60581, 0x3fd7eaf9, 0x3e74e0ac89f92ea8),
    (0x3fe5e75c, 0x3fd842d1, 0x3e443d18d2927070),
    (0x3fe5c988, 0x3fd89a34, 0x3e3b050b6ad69f0d),
    (0x3fe5ac05, 0x3fd8f120, 0xbe6e64cd1630ef47),
    (0x3fe58ed2, 0x3fd94795, 0xbe74f7ba18d16c8c),
    (0x3fe571ed, 0x3fd99d96, 0x3e6a8bf0c3f5d585),
    (0x3fe55555, 0xbfd26961, 0xbe5134d992783696),
    (0x3fe53909, 0xbfd21445, 0x3e7bbc5229784364),
    (0x3fe51d08, 0xbfd1bf9a, 0x3e77296522c89570),
    (0x3fe50150, 0xbfd16b5d, 0x3e7d14c12b299048),
    (0x3fe4e5e1, 0xbfd11790, 0x3e7b760771887184),
    (0x3fe4cab9, 0xbfd0c42f, 0x3e624f50ab07466e),
    (0x3fe4afd7, 0xbfd0713a, 0x3e7decaa928322d9),
    (0x3fe4953a, 0xbfd01eaf, 0x3e74764e6ae23044),
    (0x3fe47ae1, 0xbfcf991b, 0x3e64d313e5a09b79),
    (0x3fe460cc, 0xbfcef5af, 0xbe61373f8077bb84),
    (0x3fe446f8, 0xbfce530c, 0xbe6ff9c2748484a3),
    (0x3fe42d66, 0xbfcdb13d, 0x3e5f95bbd0655f6f),
    (0x3fe41414, 0xbfcd1037, 0xbe6c99578ed6061e),
    (0x3fe3fb01, 0xbfcc6ffa, 0xbe577806732e3237),
    (0x3fe3e22d, 0xbfcbd089, 0x3e47c42a2bf10fed),
    (0x3fe3c996, 0xbfcb31db, 0x3e66290dc716311b),
    (0x3fe3b13b, 0xbfca93ed, 0x3e60dd4997243510),
    (0x3fe3991c, 0xbfc9f6c3, 0x3e48f76adfeca7cd),
    (0x3fe38138, 0xbfc95a5a, 0xbe673dc04fc8a158),
    (0x3fe3698e, 0xbfc8beb0, 0xbe5d9c7f39955aae),
    (0x3fe3521d, 0xbfc823c2, 0x3e6eab971091232d),
    (0x3fe33ae4, 0xbfc7898b, 0xbe52a223679ef009),
    (0x3fe323e3, 0xbfc6f011, 0x3e6922a64158e0c6),
    (0x3fe30d19, 0xbfc6574f, 0x3e625cfb3193a69e),
    (0x3fe2f685, 0xbfc5bf42, 0xbe55aa1d50fdc4dc),
    (0x3fe2e026, 0xbfc527e8, 0x3e6ad7935301700f),
    (0x3fe2c9fb, 0xbfc4913b, 0xbe6cccec47f79263),
    (0x3fe2b405, 0xbfc3fb48, 0x3e51336e105b0885),
    (0x3fe29e41, 0xbfc365fc, 0x3e6bfa9c4b741671),
    (0x3fe288b0, 0xbfc2d161, 0x3e6cde5fc18a4cdd),
    (0x3fe27351, 0xbfc23d73, 0xbe4a49be40a47bad),
    (0x3fe25e22, 0xbfc1aa28, 0xbe6b88fb7238dab1),
    (0x3fe24925, 0xbfc11792, 0x3e6f760921086a1d),
    (0x3fe23456, 0xbfc08595, 0xbe69678bc3621ce2),
    (0x3fe21fb8, 0xbfbfe898, 0xbe4cedde72caa542),
    (0x3fe20b47, 0xbfbec739, 0x3e467af78e5a2c07),
    (0x3fe1f704, 0xbfbda720, 0xbe58e10b8893e565),
    (0x3fe1e2ef, 0xbfbc8855, 0x3e5390ee9645ca3c),
    (0x3fe1cf07, 0xbfbb6acd, 0xbe46d6a834f00afe),
    (0x3fe1bb4a, 0xbfba4e73, 0x3e57d3929695b9c2),
    (0x3fe1a7b9, 0xbfb93359, 0x3e415362cba90d0b),
    (0x3fe19454, 0xbfb81985, 0xbe5bd0326a07078d),
    (0x3fe18118, 0xbfb700d2, 0xbe25d580c1e8d9ef),
    (0x3fe16e07, 0xbfb5e961, 0xbe3d977a96fd4b35),
    (0x3fe15b1e, 0xbfb4d30c, 0x3e416fc839d147d2),
    (0x3fe1485f, 0xbfb3bdf5, 0x3e441708f82856a0),
    (0x3fe135c8, 0xbfb2aa04, 0x3e56ee3a36dd15e9),
    (0x3fe12359, 0xbfb1973d, 0xbe5051951794557e),
    (0x3fe11111, 0xbfb08598, 0x3e5298719e5dd716),
    (0x3fe0fef0, 0xbfaeea30, 0x3e3ffca44227ca4e),
    (0x3fe0ecf5, 0xbfaccb67, 0xbe1bbb144f0c5eb6),
    (0x3fe0db21, 0xbfaaaefd, 0xbe387d7b098d4926),
    (0x3fe0c971, 0xbfa894a1, 0x3e4ad818d332cbe8),
    (0x3fe0b7e7, 0xbfa67c97, 0xbe44b52e7bd04186),
    (0x3fe0a681, 0xbfa466ae, 0x3e4af4871e9cdd7a),
    (0x3fe0953f, 0xbfa252ec, 0xbe43e3431b66b143),
    (0x3fe08421, 0xbfa0415d, 0x3e4d862efee3fa36),
    (0x3fe07326, 0xbf9c63d0, 0xbe3b052a8a631fbb),
    (0x3fe0624e, 0xbf984930, 0xbe24645dcf45db83),
    (0x3fe05198, 0xbf9432ab, 0xbe22cc06204e64b7),
    (0x3fe04104, 0xbf902052, 0xbe3624d591d27c39),
    (0x3fe03092, 0xbf88246e, 0x3e271decb9779881),
    (0x3fe02041, 0xbf801054, 0xbe2d61f9984b78c6),
    (0x3fe01010, 0xbf7007f5, 0xbe165620cd5f950e),
    (0x3fe00000, 0x00000000, 0x0000000000000000),
];

/// Binary64 natural logarithm with reference-specific split reduction.
/// Positive finite inputs use the reference's table and fused polynomial;
/// NaNs are returned without changing their bits and FZ treats subnormals as
/// zero. This entry point is also suitable for a separate C-ABI codec shim.
pub fn log(value: f64) -> f64 {
    let bits = value.to_bits();
    let magnitude = bits & 0x7fff_ffff_ffff_ffff;
    if magnitude > 0x7ff0_0000_0000_0000 {
        return value;
    }
    if value == 0.0 {
        return f64::NEG_INFINITY;
    }
    if bits >> 63 != 0 {
        return f64::NAN;
    }
    if magnitude == 0x7ff0_0000_0000_0000 {
        return value;
    }
    let field = (bits >> 52) as i32;
    let (fraction, exponent) = if field == 0 {
        let highest = 63 - bits.leading_zeros();
        ((bits << (52 - highest)) & 0x000f_ffff_ffff_ffff, highest as i32 - 1074)
    } else {
        (bits & 0x000f_ffff_ffff_ffff, field - 1023)
    };
    let index = ((fraction + (1_u64 << 44)) >> 45) as usize;
    let exponent = exponent + i32::from(index >= 64);
    let (reciprocal, high, low) = LOG64_TABLE[index];
    let reciprocal = f64::from_bits((reciprocal as u64) << 32);
    let center_high = f64::from_bits((high as u64) << 32);
    let center_low = f64::from_bits(low);
    let normalized = f64::from_bits(0x3ff0_0000_0000_0000 | fraction);
    let remainder = normalized.mul_add(reciprocal, -1.0);
    let a = remainder.mul_add(f64::from_bits(0x3fc2_4944_9239_443d),
                              f64::from_bits(0xbfc7_b47c_3003_7039));
    let b = remainder.mul_add(remainder + f64::from_bits(0x3ff6_09d1_4c7e_e9be),
                              f64::from_bits(0x3ff9_de61_616c_df2b));
    let c = remainder.mul_add(remainder + f64::from_bits(0xbff3_f6ab_6ec1_0818),
                              f64::from_bits(0x3ffa_b7d4_f054_7db7));
    let polynomial = ((remainder * remainder) * a * b) * c;
    if exponent != 0 {
        let low = (exponent as f64).mul_add(f64::from_bits(0x3e5d_f473_de6a_f279), center_low);
        let high = (exponent as f64).mul_add(f64::from_bits(0x3fe6_2e42_f000_0000), center_high);
        high + (remainder + (low + polynomial))
    } else {
        let product = reciprocal * normalized;
        let difference = product - 1.0;
        let error = reciprocal.mul_add(normalized, -product);
        let high = f64::from_bits(difference.to_bits() & 0xffff_ffff_f800_0000);
        let low = error + (difference - high);
        (high + center_high) + ((low + center_low) + polynomial)
    }
}

/// Alias matching the existing internal `math::ln` call sites.
pub fn ln(value: f64) -> f64 {
    log(value)
}

// Split reductions of 1/pi, indexed by pairs of binary32 exponents. Keeping
// the small residual separate preserves the high bits of large arguments.
pub(crate) const TRIG_REDUCTION: [(u64, u64); 64] = [
    (0x3fd45f306e000000, 0xbdeb1bbead603d8b),
    (0x3fd45f306e000000, 0xbdeb1bbead603d8b),
    (0x3fd45f306e000000, 0xbdeb1bbead603d8b),
    (0x3fd45f306e000000, 0xbdeb1bbead603d8b),
    (0x3fd45f306e000000, 0xbdeb1bbead603d8b),
    (0x3fd45f306e000000, 0xbdeb1bbead603d8b),
    (0x3fd45f306e000000, 0xbdeb1bbead603d8b),
    (0x3fd45f306e000000, 0xbdeb1bbead603d8b),
    (0x3fd45f306e000000, 0xbdeb1bbead603d8b),
    (0x3fd45f306e000000, 0xbdeb1bbead603d8b),
    (0x3fd45f306e000000, 0xbdeb1bbead603d8b),
    (0x3fd45f306e000000, 0xbdeb1bbead603d8b),
    (0xbfc7419f24000000, 0xbdeb1bbead603d8b),
    (0xbfad067c92000000, 0x3dc391054a7f09d6),
    (0x3f77cc1b72000000, 0x3d9c882a53f84eb0),
    (0xbf6067c91b000000, 0xbd6bbead603d8a83),
    (0xbf09f246c7000000, 0x3d0054a7f09d5f48),
    (0xbf09f246c7000000, 0x3d0054a7f09d5f48),
    (0xbf09f246c7000000, 0x3d0054a7f09d5f48),
    (0x3ee836e4e4000000, 0x3d0054a7f09d5f48),
    (0xbecf246c6f000000, 0x3ca529fc2757d1f5),
    (0x3e7b727221000000, 0xbc95ac07b1505c16),
    (0x3e7b727221000000, 0xbc95ac07b1505c16),
    (0xbe5236377d000000, 0xbc76b01ec5417056),
    (0x3e4b939105000000, 0x3c629fc2757d1f53),
    (0xbe21b1bbeb000000, 0x3c34fe13abe8fa9a),
    (0xbdeb1bbead000000, 0xbc080f62a0b82b2d),
    (0xbdeb1bbead000000, 0xbc080f62a0b82b2d),
    (0x3dc391054a000000, 0x3befc2757d1f534e),
    (0x3d9c882a54000000, 0xbb7ec54170565912),
    (0xbd6bbead60000000, 0xbb7ec54170565912),
    (0xbd6bbead60000000, 0xbb7ec54170565912),
    (0x3d41054a7f000000, 0x3b33abe8fa9a6ee0),
    (0x3d0054a7f1000000, 0xbb28a82e0acb223f),
    (0x3d0054a7f1000000, 0xbb28a82e0acb223f),
    (0x3ca529fc27000000, 0x3ac5f47d4d377037),
    (0x3ca529fc27000000, 0x3ac5f47d4d377037),
    (0x3ca529fc27000000, 0x3ac5f47d4d377037),
    (0x3c84a7f09d000000, 0x3aa7d1f534ddc0db),
    (0x3c629fc275000000, 0x3a8f47d4d377036e),
    (0x3c34fe13ac000000, 0xba370565911f924f),
    (0xbc2603d8a8000000, 0xba370565911f924f),
    (0xbc080f62a1000000, 0x3a21f534ddc0db63),
    (0x3befc2757d000000, 0x39ef534ddc0db629),
    (0xbb7ec54170000000, 0xb99596447e493ad5),
    (0xbb7ec54170000000, 0xb99596447e493ad5),
    (0xbb7ec54170000000, 0xb99596447e493ad5),
    (0x3b33abe8fb000000, 0xb9596447e493ad4d),
    (0x3b33abe8fb000000, 0xb9596447e493ad4d),
    (0xbb28a82e0b000000, 0x393a6ee06db14acd),
    (0x3b0d5f47d5000000, 0xb916447e493ad4ce),
    (0xbad505c159000000, 0xb8f911f924eb5336),
    (0x3ac5f47d4d000000, 0x38dbb81b6c52b328),
    (0x3aa7d1f535000000, 0xb8b11f924eb53362),
    (0x3a8f47d4d3000000, 0x38adc0db6295993c),
    (0xba37056591000000, 0xb83f924eb53361de),
    (0xba37056591000000, 0xb83f924eb53361de),
    (0x3a21f534de000000, 0xb83f924eb53361de),
    (0x39ef534ddc000000, 0x37db6c52b3278872),
    (0x39ef534ddc000000, 0x37db6c52b3278872),
    (0xb99596447e000000, 0xb7b24eb53361de38),
    (0xb99596447e000000, 0xb7b24eb53361de38),
    (0x3984d37703000000, 0x37ab6295993c4390),
    (0x39634ddc0e000000, 0xb78275a99b0ef1bf),
];

#[inline]
fn nearest_even_integer(value: f64) -> i64 {
    // The trigonometric reduction keeps this finite and well inside i64.
    let n = value as i64;
    let fraction = value - n as f64;
    if fraction > 0.5 || (fraction == 0.5 && n & 1 != 0) {
        n + 1
    } else if fraction < -0.5 || (fraction == -0.5 && n & 1 != 0) {
        n - 1
    } else {
        n
    }
}

#[inline]
fn sine_polynomial(value: f64) -> f64 {
    let square = value * value;
    let cube = value * square;
    let polynomial = square.mul_add(
        f64::from_bits(0xbf29_943e_0fc4_818f),
        f64::from_bits(0x3f81_1073_afd1_45eb),
    );
    let fifth = square * cube;
    let base = cube.mul_add(f64::from_bits(0xbfc5_5554_5268_a02a), value);
    fifth.mul_add(polynomial, base)
}

#[inline]
fn cosine_polynomial(value: f64) -> f64 {
    let square = value * value;
    let upper = square.mul_add(
        f64::from_bits(0x3ef9_9169_fc93_8f4a),
        f64::from_bits(0xbf56_c07f_1694_5df4),
    );
    let lower = square.mul_add(
        f64::from_bits(0x3fa5_5553_c789_9699),
        f64::from_bits(0xbfdfffff_fcb8_2e97),
    );
    let polynomial = (square * square).mul_add(upper, lower);
    square.mul_add(polynomial, 1.0)
}

#[inline]
fn reduced_trig(value: f64, quadrant: i64) -> f32 {
    let output = if quadrant & 1 == 0 {
        sine_polynomial(value)
    } else {
        cosine_polynomial(value)
    };
    narrow_f32(f64::from_bits(output.to_bits() ^ (((quadrant as u64) & 2) << 62)))
}

#[inline]
fn large_trig(value: f64, magnitude: u32, cosine: bool) -> f32 {
    let (hi, lo) = TRIG_REDUCTION[((magnitude >> 24) - 64) as usize];
    let high_product = value * f64::from_bits(hi);
    let low_product = value * f64::from_bits(lo);
    let shifted = if cosine { high_product + 0.5 } else { high_product };
    let quadrant = nearest_even_integer(shifted);
    let remainder = (shifted - quadrant as f64) + low_product;
    let signed = f64::from_bits(remainder.to_bits() ^ (((quadrant as u64) & 1) << 63));
    let square = remainder * remainder;
    let upper = square.mul_add(square + f64::from_bits(0xc010_a8b9_fc97_b22c),
                                f64::from_bits(0x402e_f88f_0b12_50c1));
    let lower = square.mul_add(square + f64::from_bits(0xc00d_31f4_a0a5_5230),
                                f64::from_bits(0x4005_37b6_744f_a779));
    narrow_f32((signed * upper) * (f64::from_bits(0x3fb3_9502_de83_d49c) * lower))
}

/// Binary32 sine using the macOS reduction and polynomials, including the
/// separate large-argument path and the active flush-to-zero behavior.
pub fn sinf(value: f32) -> f32 {
    let magnitude = value.to_bits() & 0x7fff_ffff;
    if magnitude >= 0x7f80_0000 {
        return trig_nan32(value.to_bits());
    }
    if magnitude >= 0x42f0_0000 {
        return large_trig(value as f64, magnitude, false);
    }
    if magnitude < 0x3980_0000 {
        return value.mul_add(f32::from_bits(0x4c80_0000), value) * f32::from_bits(0x3280_0000);
    }
    if magnitude < 0x3f49_0fdb {
        return narrow_f32(sine_polynomial(value as f64));
    }
    let x = value as f64;
    let quadrant = nearest_even_integer(x * f64::from_bits(0x3fe4_5f30_6dc9_c883));
    let remainder = (-(quadrant as f64)).mul_add(f64::from_bits(0x3ff9_21fb_5444_2d18), x);
    reduced_trig(remainder, quadrant)
}

/// Binary32 cosine using the macOS reduction and polynomials, including the
/// separate large-argument path and the active flush-to-zero behavior.
pub fn cosf(value: f32) -> f32 {
    let magnitude = value.to_bits() & 0x7fff_ffff;
    let x = f32::from_bits(magnitude);
    if magnitude >= 0x7f80_0000 {
        return trig_nan32(magnitude);
    }
    if magnitude < 0x3980_0000 {
        return (f32::from_bits(0x4c80_0000) - x) * f32::from_bits(0x3280_0000);
    }
    if magnitude < 0x3f49_0fdb {
        return narrow_f32(cosine_polynomial(x as f64));
    }
    if magnitude >= 0x4c80_0000 {
        return large_trig(x as f64, magnitude, true);
    }
    let x = x as f64;
    let quadrant = nearest_even_integer(x * f64::from_bits(0x3fe4_5f30_6dc9_c883));
    let remainder = (-(quadrant as f64)).mul_add(f64::from_bits(0x3ff9_21fb_5444_2d18), x);
    let remainder = if magnitude >= 0x42f0_0000 {
        (-(quadrant as f64)).mul_add(f64::from_bits(0x3c91_a626_3314_5c07), remainder)
    } else {
        remainder
    };
    reduced_trig(remainder, quadrant + 1)
}

/// Pair returned by the system's binary32 combined sine/cosine entry point.
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C)]
pub struct SineCosineF32 {
    pub sine: f32,
    pub cosine: f32,
}

/// Combined binary32 sine and cosine. The system's paired entry point uses
/// different approximations from independent [`sinf`] and [`cosf`] calls.
pub fn sincosf(value: f32) -> SineCosineF32 {
    let magnitude = value.to_bits() & 0x7fff_ffff;
    if magnitude >= 0x7f80_0000 {
        let nan = trig_nan32(value.to_bits());
        return SineCosineF32 { sine: nan, cosine: nan };
    }
    if magnitude < 0x3980_0000 {
        let large = f32::from_bits(0x4c80_0000);
        let small = f32::from_bits(0x3280_0000);
        return SineCosineF32 {
            sine: value.mul_add(large, value) * small,
            cosine: (large - f32::from_bits(magnitude)) * small,
        };
    }

    let (quadrant, remainder) = if magnitude < 0x42f0_0000 {
        let scaled = (value as f64) * f64::from_bits(0x3fe4_5f30_6dc9_c883);
        let quadrant = nearest_even_integer(scaled);
        (quadrant, scaled - quadrant as f64)
    } else {
        let doubled = (value as f64) * 2.0;
        let (hi, lo) = TRIG_REDUCTION[((magnitude >> 24) - 64) as usize];
        let high_product = doubled * f64::from_bits(hi);
        let low_product = doubled * f64::from_bits(lo);
        let quadrant = nearest_even_integer(high_product);
        (quadrant, (high_product - quadrant as f64) + low_product)
    };
    let square = remainder * remainder;
    let sine_first = square.mul_add(square + f64::from_bits(0xc02c_3d3e_e4f8_713f),
                                     f64::from_bits(0x4044_4921_a30f_0d80));
    let cosine_first = square.mul_add(square + f64::from_bits(0xc020_bc03_9654_0c13),
                                       f64::from_bits(0x401d_7821_2789_bc11));
    let sine_second = square.mul_add(square + f64::from_bits(0xc02e_f09a_b49e_3a5b),
                                      f64::from_bits(0x406e_9726_4e36_bd93));
    let cosine_second = square.mul_add(square + f64::from_bits(0xc02d_6f5a_de6c_8c37),
                                        f64::from_bits(0x4062_c6b9_c7f8_8fa0));
    let sine = ((remainder * f64::from_bits(0x3f24_bc89_9499_342c)) * sine_first) * sine_second;
    let cosine = (f64::from_bits(0x3f4d_9c36_4e97_21d5) * cosine_first) * cosine_second;
    let flip = (((quadrant as u32) & 2) << 30) as u32;
    let sine = f32::from_bits(narrow_f32(sine).to_bits() ^ flip);
    let cosine = f32::from_bits(narrow_f32(cosine).to_bits() ^ flip);
    if quadrant & 1 == 0 {
        SineCosineF32 { sine, cosine }
    } else {
        SineCosineF32 { sine: cosine, cosine: f32::from_bits(sine.to_bits() ^ 0x8000_0000) }
    }
}
const TWO_OVER_PI: [u64; 19] = [
    0x35fdafd88fc6ae84, 0x9e839cfbc5294975, 0xba93dd63f5f2f8bd, 0xa7a31fb34f2ff516,
    0xb69b3f6793e584db, 0xf79788c5ad05368f, 0x8ffc4bffef02cc07, 0x4e422fc5defc941d,
    0x9cc8eb1cc1a99cfa, 0x74ce38135a2fbf20, 0x74411afa975da242, 0x7f0ef58e5894d39f,
    0x0324977504e8c90e, 0xdb92371d2126e970, 0xff28b1d5ef5de2b0, 0x6db14acc9e21c820,
    0xfe13abe8fa9a6ee0, 0x517cc1b727220a94, 0x0000000000000000,
];
const SIGN: u64 = 1_u64 << 63;
const HALF_PI_FIXED: u64 = 0xc90fdaa22168c235;
const SIN_COEFF: [u64; 6] = [
    0x3de5d8fd1fd19ccd, 0xbe5ae5e5a9291f5d, 0x3ec71de3567d48a1,
    0xbf2a01a019bfdf03, 0x3f8111111110f7d0, 0xbfc5555555555548,
];
const COS_COEFF: [u64; 7] = [
    0xbda8fa49a0861a9b, 0x3e21ee9d7b4e3f05, 0xbe927e4f7eac4bc6,
    0x3efa01a019c844f5, 0xbf56c16c16c14f91, 0x3fa555555555554b,
    0xbfe0000000000000,
];

#[inline]
fn polynomial(square: f64, coeff: &[u64]) -> f64 {
    let mut result = f64::from_bits(coeff[0]);
    for &value in &coeff[1..] { result = square.mul_add(result, f64::from_bits(value)); }
    result
}

#[inline]
fn moderate_reduction(x: f64) -> (f64, f64, i64) {
    let quadrant = nearest_even_integer(x * f64::from_bits(0x3fe4_5f30_6dc9_c883));
    let q = quadrant as f64;
    let first = q * f64::from_bits(0x3ff9_21fb_5444_0000);
    let second = q * f64::from_bits(0x3d86_8c23_4c4c_0000);
    let third = q * f64::from_bits(0x3b29_8a2e_0370_7345);
    let partial = x - first;
    let high = partial - second;
    let low = ((partial - high) - second) - third;
    (high, low, quadrant)
}

#[inline]
fn sin_from_parts(high: f64, low: f64, sign: u64) -> f64 {
    let value = high + low;
    let square = value * value;
    let correction = square * polynomial(square, &SIN_COEFF);
    let output = value.mul_add(correction, low) + high;
    f64::from_bits(output.to_bits() ^ sign)
}
#[inline]
fn cos_from_parts(high: f64, low: f64, sign: u64) -> f64 {
    let cross = high * low;
    let square = high.mul_add(high, cross + cross);
    let output = square.mul_add(polynomial(square, &COS_COEFF), 1.0);
    f64::from_bits(output.to_bits() ^ sign)
}

#[inline]
fn mul_high(x: u64, y: u64) -> u64 { ((x as u128 * y as u128) >> 64) as u64 }

#[inline]
fn large_reduction(bits: u64) -> (u64, u32, u64) {
    let exponent = (((bits >> 52) & 0x7ff) as usize) - 1023;
    let pos = 15 - (exponent >> 6);
    let shift = (exponent & 63) as u32;
    let a = TWO_OVER_PI[pos];
    let b = TWO_OVER_PI[pos + 1];
    let c = TWO_OVER_PI[pos + 2];
    let d = TWO_OVER_PI[pos + 3];
    let (part4, part5, part6) = if shift == 0 { (b, c, d) } else {
        let back = 64 - shift;
        ((b << shift) | (a >> back), (c << shift) | (b >> back), (d << shift) | (c >> back))
    };
    let mantissa = SIGN | (bits << 11);
    let product5 = (part5 as u128) * (mantissa as u128);
    let (low, carry) = mul_high(part4, mantissa).overflowing_add(product5 as u64);
    let high = ((product5 >> 64) as u64).wrapping_add(part6.wrapping_mul(mantissa)).wrapping_add(carry as u64);
    let invert = if (high << 2) & SIGN != 0 { u64::MAX } else { 0 };
    let upper = (high ^ invert) & 0x1fff_ffff_ffff_ffff;
    let lower = low ^ invert;
    let leading = upper.leading_zeros();
    let normalized = upper.wrapping_shl(leading) | lower.wrapping_shr(64 - leading);
    (normalized, leading, high)
}

#[inline]
fn exponent_shift(value: f64, leading: u32) -> f64 {
    f64::from_bits(value.to_bits().wrapping_sub((leading as u64) << 52))
}
#[inline]
fn big_sin(normalized: u64, leading: u32, sign: u64) -> f64 {
    let product = mul_high(HALF_PI_FIXED, normalized);
    let scale = f64::from_bits(0x3c20_0000_0000_0000); // 2^-61
    let value = exponent_shift((product as f64) * scale, leading);
    let high = exponent_shift(((product & 0xffff_0000_0000_0000) as f64) * scale, leading);
    let low = exponent_shift(((product & 0x0000_ffff_ffff_ffff) as f64) * scale, leading);
    let square = value * value;
    let correction = square * polynomial(square, &SIN_COEFF);
    let output = value.mul_add(correction, low) + high;
    f64::from_bits(output.to_bits() ^ sign)
}
#[inline]
fn big_cos(normalized: u64, leading: u32, sign: u64) -> f64 {
    let product = mul_high(HALF_PI_FIXED, normalized);
    let squared = mul_high(product, product);
    let square = exponent_shift((squared as f64) * f64::from_bits(0x3c50_0000_0000_0000), leading * 2); // 2^-58
    let output = square.mul_add(polynomial(square, &COS_COEFF), 1.0);
    f64::from_bits(output.to_bits() ^ sign)
}

/// Binary64 sine, including full-range reduction and signed exceptional cases.
pub fn sin(x: f64) -> f64 {
    let bits = x.to_bits();
    let magnitude = bits & !SIGN;
    if x == 0.0 { return x; }
    if magnitude >= 0x7ff0_0000_0000_0000 { return trig_nan64(bits); }
    if magnitude <= 0x3fe9_21fb_5444_2d18 { return sin_from_parts(0.0, x, 0); }
    if magnitude < 0x4120_0001_3be5_7a40 {
        let (high, low, q) = moderate_reduction(x);
        let sign = ((q as u64) & 2) << 62;
        return if q & 1 == 0 { sin_from_parts(high, low, sign) } else { cos_from_parts(high, low, sign) };
    }
    let (normalized, leading, q) = large_reduction(bits);
    let use_cos = ((q << 2) ^ (q << 1)) & SIGN != 0;
    let sign = (bits ^ q) & SIGN;
    if use_cos { big_cos(normalized, leading, sign) } else { big_sin(normalized, leading, sign) }
}

/// Binary64 cosine, including full-range reduction and signed exceptional cases.
pub fn cos(x: f64) -> f64 {
    let magnitude = x.to_bits() & !SIGN;
    let x = f64::from_bits(magnitude);
    if magnitude >= 0x7ff0_0000_0000_0000 { return trig_nan64(magnitude); }
    if magnitude <= 0x3fe9_21fb_5444_2d18 { return cos_from_parts(x, 0.0, 0); }
    if magnitude < 0x4120_0001_3be5_7a40 {
        let (high, low, q) = moderate_reduction(x);
        let sign = (((q ^ (q >> 1)) as u64) & 1) << 63;
        return if q & 1 == 0 { cos_from_parts(high, low, sign) } else { sin_from_parts(high, low, sign) };
    }
    let (normalized, leading, q) = large_reduction(magnitude);
    let use_sin = ((q << 2) ^ (q << 1)) & SIGN != 0;
    let sign = (q ^ (q << 1)) & SIGN;
    if use_sin { big_sin(normalized, leading, sign) } else { big_cos(normalized, leading, sign) }
}

/// Binary64 tangent using the reference's sine/cosine polynomial quotient.
pub fn tan(x: f64) -> f64 {
    let bits = x.to_bits();
    let magnitude = bits & !SIGN;
    if x == 0.0 { return x; }
    if magnitude >= 0x7ff0_0000_0000_0000 { return trig_nan64(bits); }
    let (value, inverse, flip) = if magnitude < 0x3fe9_21fb_5444_2d18 {
        (x, false, 0)
    } else if magnitude < 0x4120_0001_3be5_7a40 {
        let (high, low, q) = moderate_reduction(x);
        (high + low, q & 1 != 0, ((q as u64) & 1) << 63)
    } else {
        let (normalized, leading, q) = large_reduction(bits);
        let product = mul_high(HALF_PI_FIXED, normalized);
        let value = exponent_shift((product as f64) * f64::from_bits(0x3c20_0000_0000_0000), leading);
        let inverse = ((q << 2) ^ (q << 1)) & SIGN != 0;
        (value, inverse, (bits ^ (q << 1)) & SIGN)
    };
    let square = value * value;
    let sin = value.mul_add(square * polynomial(square, &SIN_COEFF), value);
    let sin = f64::from_bits(sin.to_bits() ^ flip);
    let cos = square.mul_add(polynomial(square, &COS_COEFF), 1.0);
    if inverse { cos / sin } else { sin / cos }
}

/// Pair returned by the binary64 combined sine/cosine system entry point.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SineCosineF64 { pub sine: f64, pub cosine: f64 }

/// Combined binary64 sine and cosine.
pub fn sincos(x: f64) -> SineCosineF64 {
    let magnitude = x.to_bits() & !SIGN;
    if magnitude > 0x7ff0_0000_0000_0000 {
        let nan = trig_nan64(x.to_bits());
        return SineCosineF64 { sine: nan, cosine: nan };
    }
    if x == 0.0 { return SineCosineF64 { sine: x, cosine: 1.0 }; }
    if magnitude <= 0x3fe9_21fb_5444_2d18 {
        return SineCosineF64 {
            sine: sin_from_parts(x, 0.0, 0),
            cosine: cos_from_parts(x, 0.0, 0),
        };
    }
    if magnitude == 0x7ff0_0000_0000_0000 {
        let nan = trig_nan64(x.to_bits());
        return SineCosineF64 { sine: nan, cosine: nan };
    }
    if magnitude < 0x4120_0001_3be5_7a40 {
        let (high, low, q) = moderate_reduction(x);
        let sin_sign = ((q as u64) & 2) << 62;
        let cos_sign = (((q ^ (q >> 1)) as u64) & 1) << 63;
        return if q & 1 == 0 {
            SineCosineF64 {
                sine: sin_from_parts(high, low, sin_sign),
                cosine: cos_from_parts(high, low, cos_sign),
            }
        } else {
            SineCosineF64 {
                sine: cos_from_parts(high, low, sin_sign),
                cosine: sin_from_parts(high, low, cos_sign),
            }
        };
    }
    let bits = x.to_bits();
    let (normalized, leading, q) = large_reduction(bits);
    let use_cos = ((q << 2) ^ (q << 1)) & SIGN != 0;
    let sin_sign = (bits ^ q) & SIGN;
    let cos_sign = (q ^ (q << 1)) & SIGN;
    if use_cos {
        SineCosineF64 {
            sine: big_cos(normalized, leading, sin_sign),
            cosine: big_sin(normalized, leading, cos_sign),
        }
    } else {
        SineCosineF64 {
            sine: big_sin(normalized, leading, sin_sign),
            cosine: big_cos(normalized, leading, cos_sign),
        }
    }
}
const ATAN_SMALL: [u64;12] = [
    0xbf9c0eb85f543412,0x3f84a9f5c4724056,0x3fa56cdb5d887934,0xbfaa38cf590469ec,
    0x3fadfe7b9674ae37,0xbfb10f31279ec05d,0x3fb3b113f18ac049,0xbfb745cf51795b21,
    0x3fbc71c7096c2725,0xbfc2492492179ca3,0x3fc99999999918d8,0xbfd555555555551b,
];
const ATAN_P0: [u64;13] = [
    0xbf8d88d31abc3ae5,0x3f710f3e20f6a2e2,0x3f8950e69dcdd967,0x3f7ff3b7c531aa4a,
    0xbfa09bc0ab7f914c,0x3fa094966be2b531,0x3f8a759263f377f2,0xbfb519e110f61b54,
    0x3fba1247ca5d9475,0x3f95d0b7e9e69054,0xbfd4af2b78215a1b,0x3fe702e05c0b8155,0x3fe1e00babdefed0,
];
const ATAN_P1: [u64;12] = [
    0xbf7265ef51b17db7,0x3f1ca7cde5de9bd7,0x3f8152311b180e6c,0xbf7ac1645739e676,
    0xbf791f7e2a7a338f,0x3f9c90e92ac8d86c,0xbfa63b543effa4ef,0x3f959bc93f81895a,
    0x3fb41b15e5e8dcd0,0xbfd1f6a8499714a2,0x3fe21fb781196ac3,0x3fe700a7c580ea7e,
];
const ATAN_P2: [u64;13] = [
    0x3f4a89224ff69018,0xbf3883d8959134b3,0xbf50120e602f6336,0x3f2cbd49da316282,
    0x3f62fcdacdd6e5b5,0xbf7c0b993a09ce31,0x3f873a9328786665,0xbf82b01fc60cc37a,
    0xbf8bc317394714b7,0x3fb3fd2b9f586a67,0xbfcac97826d58470,0x3fdb1b1b1b1b1b3d,0x3feb96e5a78c5c40,
];
const ATAN_P3: [u64;12] = [
    0x3f1b4e30d1ba3819,0x3f00280537f097f3,0xbf3e4eef429eb680,0x3f525bad4d85cbe1,
    0xbf5f4ac1342182d2,0x3f5dfaa5e77b7375,0x3f613a254d6e5b7c,0xbf9135a0938ec462,
    0x3fac963c83985742,0xbfc22d719c06115e,0x3fd3b13b13b13b0c,0x3fef730bd281f69d,
];
const ATAN_P4: [u64;11] = [
    0xbf23045b70e93129,0x3f100b6a460ac05d,0x3f310f884eac0e0a,0xbf32b9ad13db35a8,
    0xbf33182219e21362,0x3f68cb82a74e0699,0xbf8881ec3d15241f,0x3fa2b090aad5f9dc,
    0xbfb8ad3c44f10dc3,0x3fcd59ae78c11c49,0x3ff124a85750fb5c,
];
const ATAN_OUTER: [u64;11] = [
    0x3fa007733e06ceb3,0xbf883dafda7bd3fd,0xbfa81d33e401836d,0x3fad78252fa69c1c,
    0xbfb104146b1a1ae8,0x3fb3afd7a0e6eb75,0xbfb745b7f2d72663,0x3fbc71c5edfed480,
    0xbfc249248e1422e3,0x3fc999999989ebca,0xbfd5555555554a51,
];
#[inline]
fn atan_poly(x: f64, constants: &[u64]) -> f64 {
    let mut value=x.mul_add(f64::from_bits(constants[1]), f64::from_bits(constants[0]));
    for &c in &constants[2..] { value=value.mul_add(x,f64::from_bits(c)); }
    value
}
/// Binary64 arctangent with reference-specific piecewise polynomials.
pub fn atan(value: f64) -> f64 {
    let bits=value.to_bits();
    let magnitude=bits & 0x7fff_ffff_ffff_ffff;
    if magnitude > 0x7ff0_0000_0000_0000 { return f64::from_bits(bits | 0x0008_0000_0000_0000); }
    let x=f64::from_bits(magnitude);
    if magnitude <= 0x3e4d_12ed_0af1_a27f { return value; }
    let angle=if x<=0.5 {
        let square=x*x;
        (square*atan_poly(square,&ATAN_SMALL)).mul_add(x,x)
    } else if x<=0.75 {
        atan_poly(x+f64::from_bits(0xbfe4_0000_0000_0027),&ATAN_P0)
    } else if x<=1.0 {
        atan_poly(x+f64::from_bits(0xbfec_0000_000f_4213),&ATAN_P1)
    } else if x<=f64::from_bits(0x3ff5_5555_5555_5555) {
        atan_poly(x+f64::from_bits(0xbff2_aaaa_aaaa_aa96),&ATAN_P2)
    } else if x<=f64::from_bits(0x3ffa_aaaa_aaaa_aaab) {
        atan_poly(x+f64::from_bits(0xbff8_0000_0000_0003),&ATAN_P3)
    } else if x<=2.0 {
        atan_poly(x+f64::from_bits(0xbffd_5555_5546_1337),&ATAN_P4)
    } else if x>=f64::from_bits(0x4340_0000_0000_0000) {
        f64::from_bits(0x3ff9_21fb_5444_2d18)
    } else {
        let inverse=1.0/x;
        let square=inverse*inverse;
        let approximation=(square*atan_poly(square,&ATAN_OUTER)).mul_add(inverse,inverse);
        (f64::from_bits(0x3c91_a626_3314_5c07)-approximation)+f64::from_bits(0x3ff9_21fb_5444_2d18)
    };
    f64::from_bits(angle.to_bits() | (bits & 0x8000_0000_0000_0000))
}

#[inline]
fn atan2f_reduced(ratio: f64) -> f64 {
    let square = ratio * ratio;
    let a = square.mul_add(square + f64::from_bits(0x4002_6093_7207_6e2c),
                           f64::from_bits(0x4003_8f4c_070b_63c1));
    let b = square.mul_add(square + f64::from_bits(0xc007_92d5_bf17_c0bc),
                           f64::from_bits(0x4015_e431_68d7_c2b6));
    let c = square.mul_add(square + f64::from_bits(0x3f95_3db4_9b9f_cf6b),
                           f64::from_bits(0x400e_4fa4_7972_9042));
    let d = square.mul_add(square + f64::from_bits(0xc013_e370_1467_26b3),
                           f64::from_bits(0x401a_e0fb_08f2_dc00));
    ((a * c) * f64::from_bits(0x3f68_0bc0_fe23_203d)) * ((b * d) * ratio)
}

/// Binary32 four-quadrant arctangent, in the argument order `atan2f(y, x)`.
/// Comparisons and conversions are intentionally affected by the caller's
/// flush-to-zero mode, as in the system implementation.
pub fn atan2f(y: f32, x: f32) -> f32 {
    // Under FZ two different raw inputs can compare equal as zeros. Preserve
    // their original sign bits independently of that floating-point comparison.
    let y_bits = std::hint::black_box(y.to_bits());
    let x_bits = std::hint::black_box(x.to_bits());
    if y.is_nan() || x.is_nan() {
        return propagate_nan32(y_bits, x_bits);
    }
    let negative_x = f32::from_bits(x_bits ^ 0x8000_0000);
    let angle = if y < x {
        if negative_x >= y {
            if negative_x == y {
                f64::from_bits(0xbfe9_21fb_5444_2d18)
            } else {
                let ratio = (x as f64) / (y as f64);
                f64::from_bits(0xbff9_21fb_5444_2d18) - atan2f_reduced(ratio)
            }
        } else {
            atan2f_reduced((y as f64) / (x as f64))
        }
    } else if y == x {
        if y < 0.0 {
            f64::from_bits(0xc002_d97c_7f33_21d2)
        } else if y == 0.0 {
            if x_bits >> 31 == 0 {
                return y;
            }
            let pi = f64::from_bits(0x4009_21fb_4008_0000);
            if y_bits >> 31 == 0 { pi } else { -pi }
        } else {
            f64::from_bits(0x3fe9_21fb_5444_2d18)
        }
    } else if negative_x >= y {
        if negative_x == y {
            f64::from_bits(0x4002_d97c_7f33_21d2)
        } else {
            let ratio = (y as f64) / (x as f64);
            let pi = f64::from_bits(0x4009_21fb_5444_2d18);
            if ratio.abs() < f64::from_bits(0x3e90_0000_0000_0000) {
                let reduced_pi = f64::from_bits(0x4009_21fb_4008_0000);
                if y_bits >> 31 == 0 { reduced_pi } else { -reduced_pi }
            } else {
                atan2f_reduced(ratio) + if y_bits >> 31 == 0 { pi } else { -pi }
            }
        }
    } else {
        let ratio = (x as f64) / (y as f64);
        f64::from_bits(0x3ff9_21fb_5444_2d18) - atan2f_reduced(ratio)
    };
    narrow_f32(angle)
}

#[inline]
fn normalize_f32_magnitude(bits: u32) -> (u32, i32) {
    let field = bits >> 23;
    if field == 0 {
        let highest = 31 - bits.leading_zeros();
        (bits << (23 - highest), highest as i32 - 149)
    } else {
        ((bits & 0x007f_ffff) | 0x0080_0000, field as i32 - 127)
    }
}

/// Binary32 remainder with the sign of `x`, matching `fmodf`.
///
/// Finite nonzero inputs are reduced by integer significand arithmetic.
/// Subnormal results are formed by a final float multiply so the caller's
/// flush-to-zero mode applies, while an already-smaller input is returned
/// unchanged as in the system routine.
pub fn fmodf(x: f32, y: f32) -> f32 {
    let xb = x.to_bits();
    let yb = y.to_bits();
    let ax = xb & 0x7fff_ffff;
    let ay = yb & 0x7fff_ffff;
    let sign = xb & 0x8000_0000;
    if ax >= 0x7f80_0000 || ay >= 0x7f80_0000 || ax == 0 || ay == 0 {
        if ax > 0x7f80_0000 || ay > 0x7f80_0000 {
            return propagate_nan32(xb, yb);
        }
        if ax == 0x7f80_0000 || y == 0.0 {
            return f32::NAN;
        }
        if x == 0.0 || ay == 0x7f80_0000 {
            return x;
        }
    }
    if ax < ay {
        return x;
    }
    if ax == ay {
        return f32::from_bits(sign);
    }
    let (mut numerator, exponent_x) = normalize_f32_magnitude(ax);
    let (denominator, exponent_y) = normalize_f32_magnitude(ay);
    let mut shift = exponent_x - exponent_y;
    while shift > 0 {
        if numerator >= denominator {
            numerator -= denominator;
        }
        numerator <<= 1;
        shift -= 1;
    }
    if numerator >= denominator {
        numerator -= denominator;
    }
    if numerator == 0 {
        return f32::from_bits(sign);
    }
    let highest = 31 - numerator.leading_zeros();
    let exponent = exponent_y + highest as i32 - 23;
    if exponent >= -126 {
        let normalized = numerator << (23 - highest);
        let output = sign | (((exponent + 127) as u32) << 23) | (normalized & 0x007f_ffff);
        f32::from_bits(output)
    } else {
        let adjustment = exponent_y + 126;
        let fraction = if adjustment >= 0 {
            numerator << adjustment as u32
        } else {
            numerator >> (-adjustment) as u32
        };
        let normal = (fraction as f32) * f32::from_bits(0x3400_0000);
        let result = normal * f32::MIN_POSITIVE;
        f32::from_bits(result.to_bits() | sign)
    }
}

/// Binary exponent of `value`, matching the `logb` interface.
///
/// Normal and subnormal inputs are classified from their bits, including when
/// flush-to-zero is active. Zero maps to negative infinity and infinities to
/// positive infinity. NaNs pass through a floating-point multiplication.
pub fn logb(value: f64) -> f64 {
    let magnitude = value.to_bits() & 0x7fff_ffff_ffff_ffff;
    let exponent = (magnitude >> 52) as i32;
    match exponent {
        0 if magnitude == 0 => f64::NEG_INFINITY,
        0 => -((magnitude.leading_zeros() as i32 + 1011) as f64),
        0x7ff => value * value,
        _ => (exponent - 1023) as f64,
    }
}
/// Decompose into a signed fraction in [0.5,1) and an integral exponent.
/// Zero, infinity and NaN retain their original bits and return exponent zero.
pub fn frexp(value: f64) -> (f64, i32) {
    let bits=value.to_bits();
    let magnitude=bits & 0x7fff_ffff_ffff_ffff;
    let field=(magnitude>>52) as i32;
    if magnitude==0 || field==0x7ff { return (value,0); }
    let sign=bits & 0x8000_0000_0000_0000;
    if field!=0 {
        return (f64::from_bits(sign | 0x3fe0_0000_0000_0000 | (magnitude & 0x000f_ffff_ffff_ffff)),field-1022);
    }
    let highest=63-magnitude.leading_zeros();
    let fraction=(magnitude << (52-highest)) & 0x000f_ffff_ffff_ffff;
    (f64::from_bits(sign | 0x3fe0_0000_0000_0000 | fraction),highest as i32-1073)
}

/// Scale `value` by 2 raised to `exponent`, matching `scalbn`.
///
/// The implementation uses the same staged binary64 multiplications for
/// overflow and underflow, so the active floating-point flush mode matters.
pub fn scalbn(mut value: f64, exponent: i32) -> f64 {
    let mut n = exponent.clamp(-2100, 2100);
    if n >= 1024 {
        let scale = f64::from_bits(0x7fe0_0000_0000_0000); // 2^1023
        loop {
            value *= scale;
            n -= 1023;
            if n <= 1023 {
                break;
            }
        }
    } else if n <= -1023 {
        loop {
            let bits = value.to_bits();
            let e = ((bits >> 52) & 0x7ff) as i32;
            if e == 0 {
                // The reference compares before handling a subnormal. With
                // FPCR.FZ, that comparison also treats subnormals as zero.
                if value == 0.0 {
                    return value;
                }
                return f64::from_bits(bits & (1_u64 << 63));
            }
            if e + n < 0 {
                if e == 0x7ff {
                    return value;
                }
                if e + n <= -64 {
                    return f64::from_bits(bits & (1_u64 << 63));
                }
                let intermediate = (bits & 0x800f_ffff_ffff_ffff)
                    | (((e + n + 64) as u64) << 52);
                return f64::from_bits(intermediate)
                    * f64::from_bits(0x3bf0_0000_0000_0000); // 2^-64
            }
            value *= f64::MIN_POSITIVE; // 2^-1022
            n += 1022;
            if n > -1023 {
                break;
            }
        }
    }
    let factor = f64::from_bits(((n + 1023) as u64) << 52);
    value * factor
}

/// Repeated-squaring integer power in compiler-rt `__powidf2` order.
///
/// The signed exponent is halved toward zero, including `i32::MIN`; a
/// negative exponent takes a single reciprocal after the multiplication loop.
/// Floating-point flush mode affects the intermediate products and reciprocal.
pub fn powi_f64(mut value: f64, exponent: i32) -> f64 {
    let mut n = exponent;
    let mut result = if n & 1 != 0 { value } else { 1.0 };
    while n != 0 && n != 1 && n != -1 {
        n /= 2;
        value *= value;
        if n & 1 != 0 {
            result *= value;
        }
    }
    if exponent < 0 { 1.0 / result } else { result }
}
macro_rules! rounding {
    ($floor:ident,$ceil:ident,$round:ident,$trunc:ident,$ty:ty,$u:ty,$frac:expr,$bias:expr,$sign:expr,$nan:expr) => {
        /// Round toward negative infinity, preserving signed zero.
        pub fn $floor(value:$ty)->$ty {
            let bits=std::hint::black_box(value.to_bits());
            let magnitude=bits & !$sign;
            let field=(magnitude >> $frac) as i32;
            if field == 2*$bias+1 {
                return if magnitude & (((1 as $u)<<$frac)-1) == 0 { value }
                    else { <$ty>::from_bits(bits | $nan) };
            }
            if field==0 && value==0.0 { return <$ty>::from_bits(bits & $sign); }
            let exponent=field-$bias;
            if exponent<0 { return if bits & $sign!=0 { -1.0 } else { 0.0 }; }
            if exponent >= $frac { return value; }
            let shift=($frac-exponent) as u32;
            let mask=((1 as $u)<<shift)-1;
            if bits & mask==0 { return value; }
            let mut output=bits & !mask;
            if bits & $sign!=0 { output=output.wrapping_add((1 as $u)<<shift); }
            <$ty>::from_bits(output)
        }
        /// Round toward positive infinity, preserving signed zero.
        pub fn $ceil(value:$ty)->$ty {
            let bits=std::hint::black_box(value.to_bits());
            let magnitude=bits & !$sign;
            let field=(magnitude >> $frac) as i32;
            if field == 2*$bias+1 {
                return if magnitude & (((1 as $u)<<$frac)-1) == 0 { value }
                    else { <$ty>::from_bits(bits | $nan) };
            }
            if field==0 && value==0.0 { return <$ty>::from_bits(bits & $sign); }
            let exponent=field-$bias;
            if exponent<0 { return if bits & $sign!=0 { <$ty>::from_bits($sign) } else { 1.0 }; }
            if exponent >= $frac { return value; }
            let shift=($frac-exponent) as u32;
            let mask=((1 as $u)<<shift)-1;
            if bits & mask==0 { return value; }
            let mut output=bits & !mask;
            if bits & $sign==0 { output=output.wrapping_add((1 as $u)<<shift); }
            <$ty>::from_bits(output)
        }
        /// Round to nearest integral value, with halfway cases away from zero.
        pub fn $round(value:$ty)->$ty {
            let bits=value.to_bits();
            let magnitude=bits & !$sign;
            let field=(magnitude >> $frac) as i32;
            if field == 2*$bias+1 {
                return if magnitude & (((1 as $u)<<$frac)-1) == 0 { value }
                    else { <$ty>::from_bits(bits | $nan) };
            }
            let exponent=field-$bias;
            if exponent < -1 { return <$ty>::from_bits(bits & $sign); }
            if exponent == -1 { return <$ty>::from_bits((bits & $sign) | (($bias as $u)<<$frac)); }
            if exponent >= $frac { return value; }
            let shift=($frac-exponent) as u32;
            let mask=((1 as $u)<<shift)-1;
            let fraction=bits & mask;
            let mut output=bits & !mask;
            if fraction >= ((1 as $u)<<(shift-1)) { output=output.wrapping_add((1 as $u)<<shift); }
            <$ty>::from_bits(output)
        }
        /// Round toward zero, preserving signed zero.
        pub fn $trunc(value:$ty)->$ty {
            let bits=value.to_bits();
            let magnitude=bits & !$sign;
            let field=(magnitude >> $frac) as i32;
            if field == 2*$bias+1 {
                return if magnitude & (((1 as $u)<<$frac)-1) == 0 { value }
                    else { <$ty>::from_bits(bits | $nan) };
            }
            let exponent=field-$bias;
            if exponent<0 { return <$ty>::from_bits(bits & $sign); }
            if exponent >= $frac { return value; }
            let mask=((1 as $u)<<(($frac-exponent) as u32))-1;
            <$ty>::from_bits(bits & !mask)
        }
    };
}
rounding!(floor,ceil,round,trunc,f64,u64,52,1023,0x8000_0000_0000_0000_u64,0x0008_0000_0000_0000_u64);
rounding!(floorf,ceilf,roundf,truncf,f32,u32,23,127,0x8000_0000_u32,0x0040_0000_u32);

/// IEEE binary64 square root; negative nonzero inputs produce positive qNaN.
pub fn sqrt(value:f64)->f64 {
    let bits=value.to_bits();
    if bits & 0x7fff_ffff_ffff_ffff > 0x7ff0_0000_0000_0000 {
        return f64::from_bits(bits | 0x0008_0000_0000_0000);
    }
    if value < 0.0 { return f64::from_bits(0x7ff8_0000_0000_0000); }
    value.sqrt()
}
/// IEEE binary32 square root; negative nonzero inputs produce positive qNaN.
pub fn sqrtf(value:f32)->f32 {
    let bits=value.to_bits();
    if bits & 0x7fff_ffff > 0x7f80_0000 { return f32::from_bits(bits | 0x0040_0000); }
    if value < 0.0 { return f32::from_bits(0x7fc0_0000); }
    value.sqrt()
}
/// Round halfway away from zero; overflow saturates and NaN returns i64::MIN.
/// C `long` is 64-bit on the supported macOS/Linux targets.
pub fn lround(value: f64) -> i64 {
    if value.is_nan() { return i64::MIN; }
    let integer=value as i64;
    let fraction=value-(integer as f64);
    if fraction >= 0.5 { integer.saturating_add(1) }
    else if fraction <= -0.5 { integer.saturating_sub(1) }
    else { integer }
}
#[inline]
fn hyperbolic_parts(x:f64)->(i64,f64,f64) {
    let high=f64::from_bits(0x3ff7_1547_652b_82fe);
    let low=f64::from_bits(0x3c77_77d0_ffda_0d24);
    let product=x*high;
    let index=(product*128.0) as i64;
    let remainder=product-(index as f64*0.0078125);
    let error=(-x).mul_add(high,product);
    let correction=x.mul_add(low,-error);
    (index,remainder,correction)
}
#[inline]
fn hyperbolic_exp_poly(remainder:f64, correction:f64)->f64 {
    let first=remainder+f64::from_bits(0x401e_f158_4af1_7ecd);
    let second=remainder+f64::from_bits(0xbfe1_3b4d_4879_e55e);
    let r=remainder+correction;
    let a=r.mul_add(first,f64::from_bits(0x4034_ffde_7cc0_2cf4));
    let b=r.mul_add(second,f64::from_bits(0x4038_b31b_87eb_8904));
    ((r*f64::from_bits(0x3f55_e522_72e0_eaec))*a)*b
}
#[inline]
fn hyperbolic_large(x:f64)->f64 {
    let (index,remainder,correction)=hyperbolic_parts(x);
    let j=(index&127) as usize;
    let polynomial=hyperbolic_exp_poly(remainder,f64::from_bits(EXP64_LO[j])+correction);
    let high=f64::from_bits(EXP64_HI[j]);
    let scale=f64::from_bits(0x3fe0_0000_0000_0000_u64.wrapping_add(((index>>7) as u64)<<52));
    polynomial.mul_add(high,high)*scale
}
#[inline]
fn hyperbolic_middle(x:f64, subtract:bool)->f64 {
    let (index,remainder,correction)=hyperbolic_parts(x);
    let j=(index&127) as usize;
    let opposite=127-j;
    let positive_poly=hyperbolic_exp_poly(remainder,f64::from_bits(EXP64_LO[j])+correction);
    let other_remainder=0.0078125-remainder;
    let negative_poly=hyperbolic_exp_poly(other_remainder,f64::from_bits(EXP64_LO[opposite])-correction);
    let exponent=index>>7;
    let positive=f64::from_bits(EXP64_HI[j].wrapping_add(((exponent-1) as u64)<<52));
    let negative=f64::from_bits(EXP64_HI[opposite].wrapping_sub(((exponent+2) as u64)<<52));
    if subtract {
        let difference=positive-negative;
        let error=(difference-positive)+negative;
        let error=negative.mul_add(negative_poly,error);
        let error=(-positive).mul_add(positive_poly,error);
        difference-error
    } else {
        let sum=positive+negative;
        let error=(sum-positive)-negative;
        let error=negative.mul_add(negative_poly,-error);
        let error=positive.mul_add(positive_poly,error);
        sum+error
    }
}
/// Binary64 hyperbolic sine, matching reference scaling and polynomial order.
pub fn sinh(value:f64)->f64 {
    let bits=value.to_bits();
    let magnitude=bits & 0x7fff_ffff_ffff_ffff;
    if magnitude>0x7ff0_0000_0000_0000 { return f64::from_bits(bits | 0x0008_0000_0000_0000); }
    if magnitude>=0x4086_33ce_8fb9_f87e { return value*f64::MAX; }
    if magnitude<0x3e40_0000_0000_0000 {
        return value.mul_add(f64::from_bits(0x4360_0000_0000_0000),value)*f64::from_bits(0x3c80_0000_0000_0000);
    }
    if magnitude<0x3fd6_2e42_0000_0000 {
        let square=value*value;
        let p=square.mul_add(f64::from_bits(0x3de6_1bfa_2e91_919a),f64::from_bits(0x3e5a_e642_c86b_fbc7));
        let p=square.mul_add(p,f64::from_bits(0x3ec7_1de3_a5aa_7055));
        let p=square.mul_add(p,f64::from_bits(0x3f2a_01a0_1a01_96ae));
        let p=square.mul_add(p,f64::from_bits(0x3f81_1111_1111_1111));
        let p=square.mul_add(p,f64::from_bits(0x3fc5_5555_5555_5555));
        return (value*square).mul_add(p,value);
    }
    let x=f64::from_bits(magnitude);
    let result=if magnitude>=0x4035_0000_0000_0000 { hyperbolic_large(x) } else { hyperbolic_middle(x,true) };
    f64::from_bits(result.to_bits() | (bits & 0x8000_0000_0000_0000))
}
/// Binary64 hyperbolic cosine, matching reference scaling and polynomial order.
pub fn cosh(value:f64)->f64 {
    let magnitude=value.to_bits() & 0x7fff_ffff_ffff_ffff;
    if magnitude>0x7ff0_0000_0000_0000 { return f64::from_bits(magnitude | 0x0008_0000_0000_0000); }
    let x=f64::from_bits(magnitude);
    if magnitude>=0x4086_33ce_8fb9_f87e { return x*f64::MAX; }
    if magnitude<0x3e10_0000_0000_0000 { return x.mul_add(x,2.0)*0.5; }
    if magnitude<0x3fd6_2e42_0000_0000 {
        let square=x*x;
        let p=square.mul_add(f64::from_bits(0x3e21_f7f2_0c7f_8ea9),f64::from_bits(0x3e92_7e4d_41cf_7cf1));
        let p=square.mul_add(p,f64::from_bits(0x3efa_01a0_1a9e_8422));
        let p=square.mul_add(p,f64::from_bits(0x3f56_c16c_16c1_5a6c));
        let p=square.mul_add(p,f64::from_bits(0x3fa5_5555_5555_5557));
        let low=x*0.5;
        let p=square*p;
        let low=x.mul_add(p,low);
        return x.mul_add(low,1.0);
    }
    if magnitude>=0x4035_0000_0000_0000 { hyperbolic_large(x) } else { hyperbolic_middle(x,false) }
}
// Polynomial coefficients for eight centered magnitude intervals.
const TANH_TABLE: [[u64; 16]; 8] = [
    [
        0xbfbd20c2f0233edb, 0xc03343b38896f0b4, 0x40062a6dd642e5f2, 0x3ffd6a2b08287816,
        0xbff82c7d74f3eec6, 0xc007215acc9fb7f0, 0x3f53c077627b845c, 0x403dfa52b8813743,
        0x4000f1eaf9d69209, 0x400230c40dbb23a6, 0x40058dfd62bc4a9c, 0x4006d7ac95bb7ebb,
        0xbf32c5dbe59ba738, 0xbf2285e830f09106, 0x3feef20000000000, 0x3f6e000000000000,
    ],
    [
        0x3f8c8ea9f23e804c, 0x400760f0b7993b69, 0x400150bd04d7a1c5, 0xbfecbb7fbf48be5b,
        0xc002b4f4c7166628, 0xc00967616f164262, 0xbf7f0e0a35e743ee, 0x4002bfcc207fb647,
        0x40044f947dc5b290, 0x40052d29d5a7c63d, 0x40056dbce8bf8267, 0x4005933cf97f4ab1,
        0xbf63769443462e8e, 0xbf66371574fff23b, 0x3febfc0000000000, 0x3f9cc00000000000,
    ],
    [
        0x3f805149baef85ef, 0x4007dd95197635cc, 0x400401a9f73d2b4c, 0xbfdecab664089381,
        0xc002118c95acd0c0, 0xc00d4f1ee6952a16, 0xbf8083470d069524, 0x40029f96c2560ddb,
        0x4004d508bdb79078, 0x4008e8711607d045, 0x400b8b9b0e8d997b, 0x400fe3aaa6cddded,
        0x3f465a05505fff51, 0xbf583538a7c2274b, 0x3fe6400000000000, 0x3fbdf00000000000,
    ],
    [
        0x3ffcea67384b11e9, 0xbffb61dca98942f1, 0x4008f484bbf139fd, 0xbf9b3edb87fc9a2b,
        0xbffce191d611f94f, 0xc008f93f06e92964, 0xbfc4581fb52271d2, 0xbfc52176c275a3ab,
        0x400aad35d2a7a8bb, 0x4009a5abbcfde960, 0x4009c6f32a11f93f, 0x4009f0bf5d144f71,
        0x3f3815b62267a35a, 0x3f4e91115d517686, 0x3fe0400000000000, 0x3fd0700000000000,
    ],
    [
        0x3f79d573ac48a801, 0x400b2f66c7d28529, 0x3fe1984c3d542023, 0xbff5c0811965f6af,
        0xc00849d4c45d855d, 0xc0100ca2f60e8c0d, 0xbf7fc955f6cf414f, 0x400d178e287f7e41,
        0x400ee3aaa95874bc, 0x401023e37f392380, 0x4010c20ad086484e, 0x40111aae4ef8eaa5,
        0xbf2311e422ef2c53, 0xbf3212bb7f586ef1, 0x3fd6400000000000, 0x3fdaa00000000000,
    ],
    [
        0x3f77ce7bc458b46c, 0x4010c82b6cec1ffd, 0x3fec3c0474639899, 0xbff21c2bc70cf0da,
        0xc00725d4ba0681df, 0xc00f5a29bc4b0e92, 0xbf7d8d1f70cb1f6f, 0x4013ddf5a34313a8,
        0x401078e54f22290a, 0x40104dd7ea5a025a, 0x40105f93a44aaaf5, 0x40106e0d61650779,
        0xbf0c60bd458a1834, 0xbf22a7763e951b37, 0x3fcd280000000000, 0x3fe2160000000000,
    ],
    [
        0xc015516bf006503c, 0x40118016dbc7636e, 0x3ff5514cc150e4b9, 0xbfedebdb00c05dd1,
        0xc009ba10bb2b0a0a, 0xc0133877476ec678, 0x3fdbae9cc13f99d1, 0x4014740dbcf7677d,
        0x401476a144da27f9, 0x40158fe768dfbc76, 0x401757547139120d, 0x40192330f539ad04,
        0x3ed4a72990f33fe0, 0x3e9d115702afdd8d, 0x3fc2900000000000, 0x3fe60a0000000000,
    ],
    [
        0xc00354b959b1dff0, 0x4013a94fb28e608b, 0x3ffd3038014972b9, 0xbfd51fa3cad3528e,
        0xc003c48aa812bf6e, 0xc0103fdd839363fc, 0x3fcb8de916d04f6d, 0x401976f1f99190cf,
        0x4016028448cc199f, 0x40155491205f7ada, 0x40157086c28e6266, 0x40158eb8b8d53c8a,
        0x3edcc89f858814a5, 0x3ea62d021453335f, 0x3fb7300000000000, 0x3fe9120000000000,
    ],
];

const TANH_SMALL: [u64;9] = [
    0x400e0dabc398b233,0x3fe8671c1eaf3374,0xc0053f1e5aacd98c,0xc01391fe3509b143,
    0x4018aa753b569bd0,0x4019755323ab1cfd,0x401a5c956c18b14e,0x401af4617f8ecc54,
    0xbf29111400a41fbf,
];
const TANH_LARGE: [u64;8] = [
    0xbf8dc165dad1eab3,0x3f9fef26c0bbebc1,0xbfafffbdaa5e7368,0x3fbfffff6ee83673,
    0xbfcfffffff536d53,0x3fdfffffffff9b22,0xbfefffffffffffea,0x3ff0000000000000,
];
/// Binary64 hyperbolic tangent using the reference's centered approximations.
pub fn tanh(value:f64)->f64 {
    let bits=value.to_bits();
    let magnitude=bits & 0x7fff_ffff_ffff_ffff;
    let sign=bits & 0x8000_0000_0000_0000;
    if magnitude>0x7ff0_0000_0000_0000 { return f64::from_bits(bits | 0x0008_0000_0000_0000); }
    if magnitude>=0x4033_0fc1_931f_09ca { return f64::from_bits(sign | 0x3ff0_0000_0000_0000); }
    if magnitude<0x3e40_0000_0000_0000 {
        return value.mul_add(f64::from_bits(0x4360_0000_0000_0000),value)*f64::from_bits(0x3c80_0000_0000_0000);
    }
    let x=f64::from_bits(magnitude);
    if magnitude<0x3fc0_0000_0000_0000 {
        let square=value*value;
        let a=square.mul_add(square+f64::from_bits(TANH_SMALL[0]),f64::from_bits(TANH_SMALL[4]));
        let b=square.mul_add(square+f64::from_bits(TANH_SMALL[1]),f64::from_bits(TANH_SMALL[5]));
        let c=square.mul_add(square+f64::from_bits(TANH_SMALL[2]),f64::from_bits(TANH_SMALL[6]));
        let d=square.mul_add(square+f64::from_bits(TANH_SMALL[3]),f64::from_bits(TANH_SMALL[7]));
        let factor=(a*b)*(c*d);
        return ((value*square)*f64::from_bits(TANH_SMALL[8])).mul_add(factor,value);
    }
    if magnitude<0x4000_0000_0000_0000 {
        let j=(x*4.0) as usize;
        let center=(j as f64)*0.25+0.125;
        let r=x-center;
        let table=&TANH_TABLE[j];
        let a=r.mul_add(r+f64::from_bits(table[0]),f64::from_bits(table[6]));
        let b=r.mul_add(r+f64::from_bits(table[1]),f64::from_bits(table[7]));
        let c=r.mul_add(r+f64::from_bits(table[2]),f64::from_bits(table[8]));
        let d=r.mul_add(r+f64::from_bits(table[3]),f64::from_bits(table[9]));
        let e=r.mul_add(r+f64::from_bits(table[4]),f64::from_bits(table[10]));
        let f=r.mul_add(r+f64::from_bits(table[5]),f64::from_bits(table[11]));
        let g=r.mul_add(f64::from_bits(table[12]),f64::from_bits(table[13]));
        let polynomial=((a*b)*(c*d))*((e*f)*g);
        let high=f64::from_bits(magnitude & 0xffff_ffff_0000_0000);
        let low=x-high;
        let slope=f64::from_bits(table[14]);
        let result=low.mul_add(slope,polynomial)+high.mul_add(slope,f64::from_bits(table[15]));
        return f64::from_bits(result.to_bits() | sign);
    }
    let input=-(x+x);
    let (index,remainder,correction)=hyperbolic_parts(input);
    let j=(index&127) as usize;
    let p=hyperbolic_exp_poly(remainder,f64::from_bits(EXP64_LO[j])+correction);
    let high=f64::from_bits(EXP64_HI[j]);
    let scaled=p.mul_add(high,high);
    let t=f64::from_bits(scaled.to_bits().wrapping_add((((index>>7)+1) as u64)<<52));
    let mut result=t.mul_add(f64::from_bits(TANH_LARGE[0]),f64::from_bits(TANH_LARGE[1]));
    for &coefficient in &TANH_LARGE[2..] { result=t.mul_add(result,f64::from_bits(coefficient)); }
    f64::from_bits(result.to_bits() | sign)
}
/// Binary64 log(1+x) using the system's compensated range reduction.
pub fn log1p(value:f64)->f64 {
    let bits=value.to_bits();
    let magnitude=bits & 0x7fff_ffff_ffff_ffff;
    if magnitude>0x7ff0_0000_0000_0000 { return value; }
    if bits==0x7ff0_0000_0000_0000 { return value; }
    if value == -1.0 { return f64::NEG_INFINITY; }
    if value < -1.0 { return f64::from_bits(0x7ff8_0000_0000_0000); }
    if magnitude<=0x3e40_0000_0000_0000 {
        // The exact FMA result for the least positive normal input lies just
        // below the normal boundary. ARM FZ flushes before rounding; x86 FTZ
        // otherwise rounds it back to MIN_POSITIVE.
        #[cfg(target_arch = "x86_64")]
        if bits == 0x0010_0000_0000_0000
            && (std::hint::black_box(f64::MIN_POSITIVE) * std::hint::black_box(0.5_f64))
                .to_bits() == 0
        {
            return 0.0;
        }
        let factor=if magnitude < 0x0020_0000_0000_0000 { value } else { value*0.5 };
        return (-value).mul_add(factor,value);
    }
    let sum=value+1.0;
    let sum_bits=sum.to_bits();
    let fraction=sum_bits & 0x000f_ffff_ffff_ffff;
    let field=(sum_bits>>52) as i32;
    let index=((fraction+(1_u64<<44))>>45) as usize;
    let exponent=field-1023+i32::from(index>=64);
    let (inverse,high,low)=LOG64_TABLE[index];
    let inverse=f64::from_bits((inverse as u64)<<32);
    let center_high=f64::from_bits((high as u64)<<32);
    let center_low=f64::from_bits(low);
    let normalized=f64::from_bits(0x3ff0_0000_0000_0000 | fraction);
    let raw_remainder=normalized.mul_add(inverse,-1.0);
    let correction=(value-(sum-1.0))*inverse;
    let scale_field=(2046-field).max(0) as u64;
    let correction=correction*f64::from_bits(scale_field<<52);
    let a=raw_remainder.mul_add(f64::from_bits(0x3fc2_4944_9239_443d),f64::from_bits(0xbfc7_b47c_3003_7039));
    let remainder=raw_remainder+correction;
    let b=remainder.mul_add(raw_remainder+f64::from_bits(0x3ff6_09d1_4c7e_e9be),f64::from_bits(0x3ff9_de61_616c_df2b));
    let c=remainder.mul_add(raw_remainder+f64::from_bits(0xbff3_f6ab_6ec1_0818),f64::from_bits(0x3ffa_b7d4_f054_7db7));
    let polynomial=((remainder*remainder)*a*b)*c;
    if exponent!=0 {
        let low=(exponent as f64).mul_add(f64::from_bits(0x3e5d_f473_de6a_f279),center_low);
        let high=(exponent as f64).mul_add(f64::from_bits(0x3fe6_2e42_f000_0000),center_high);
        high+(remainder+(low+polynomial))
    } else {
        let product=inverse*normalized;
        let difference=product-1.0;
        let error=inverse.mul_add(normalized,-product);
        let high=f64::from_bits(difference.to_bits() & 0xffff_ffff_f800_0000);
        let low=((error+(difference-high))+correction)+center_low;
        (high+center_high)+(low+polynomial)
    }
}

/// Binary64 inverse hyperbolic sine using the reference's piecewise formula.
pub fn asinh(value:f64)->f64 {
    let bits=value.to_bits();
    let magnitude=bits & 0x7fff_ffff_ffff_ffff;
    if magnitude>0x7ff0_0000_0000_0000 {
        return f64::from_bits(bits | 0x0008_0000_0000_0000);
    }
    let x=f64::from_bits(magnitude);
    if magnitude<0x3e40_0000_0000_0000 {
        if value==0.0 { return value; }
        let result=(x*f64::from_bits(0x4360_0000_0000_0000)-f64::MIN_POSITIVE)*f64::from_bits(0x3c80_0000_0000_0000);
        return f64::from_bits(result.to_bits() | (bits & 0x8000_0000_0000_0000));
    }
    let result=if x<=f64::from_bits(0x3ff5_5555_5555_5555) {
        let reciprocal=1.0/x;
        let root=reciprocal.mul_add(reciprocal,1.0).sqrt();
        log1p(x+x/(reciprocal+root))
    } else if x<=f64::from_bits(0x41a0_0000_0000_0000) {
        let twice=x+x;
        let root=value.mul_add(value,1.0).sqrt();
        log(twice+1.0/(x+root))
    } else {
        log(x)+f64::from_bits(0x3fe6_2e42_fefa_39ef)
    };
    f64::from_bits(result.to_bits() | (bits & 0x8000_0000_0000_0000))
}
