//! S1: `_getMsMDCT` (0x44bbc): the mid/side candidate spectra.

/// `_getMsMDCT(n, L, R, M, S)`: `M[k] = (L[k] + R[k]) * 0.5`, `S[k] = (L[k] - R[k]) * 0.5` for `k < n` (`vDSP_vasm(L, R, &0.5, M)` and
/// `vDSP_vsbsm(L, R, &0.5, S)`: an add/subtract followed by a multiply, never fused).  `n` = 1024 for every block type (long/start/stop:
/// first ushort of the sfb table; short: 128 * 8); for EIGHT_SHORT frames `L`/`R` are the unsorted window-major spectra (the result is sorted
/// afterwards, which is the same as sorting L and R first).
pub fn get_ms_mdct(n: usize, l: &[f32], r: &[f32], m: &mut [f32], s: &mut [f32]) {
    for k in 0..n {
        m[k] = (l[k] + r[k]) * 0.5;
        s[k] = (l[k] - r[k]) * 0.5;
    }
}
