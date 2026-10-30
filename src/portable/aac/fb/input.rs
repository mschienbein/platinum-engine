//! F0: input path: client float scaling/clamping, the 2112-float history ring (1088-sample delay), 1024-frame framing
//! with EOS flush, access-unit count and the `iTunSMPB` numbers.

use crate::portable::arm::{max_number, min_number};

/// Frames per `AACEncoder::EncodeFrame` / access unit.
pub const FRAME_LEN: usize = 1024;
/// Delay of the coding block inside `_ComputeBlockMode`'s ring: `L + L/16`.
pub const RING_DELAY: usize = FRAME_LEN + FRAME_LEN / 16;
/// Ring size `2L + L/16` (observed live: `chan+0x138` = `{size 2112, wpos, ..}`).
pub const RING_SIZE: usize = 2 * FRAME_LEN + FRAME_LEN / 16;
/// `kAudioConverterPrimeInfo.leadingFrames` = `GetBaseAACDelay` = 1088 + 1024.
pub const PRIMING_FRAMES: usize = RING_DELAY + FRAME_LEN;

/// `ACMP4AACBaseEncoder::AppendInputData` -> `ClampAndScale` (0x60ad44) followed by the clamp at the top of `AACEncoder::EncodeFrame`
/// (0x12ed60):
///
/// ```text
/// v = fmaxnm(x, -1000.0f); v = fminnm(v, +1000.0f); s = v * 32768.0f; s = fminnm(s, 163840.0f); s = fmaxnm(s, -163840.0f)
/// ```
///
/// `fmaxnm/fminnm` return the numeric operand for a quiet NaN: NaN -> -1000 -> -32768000 -> -163840; +-inf saturate; denormals scale normally;
/// -0.0 stays -0.0. Implemented with the exact FMAXNM/FMINNM value semantics of `crate::portable::arm` (default FPCR, FZ off).
#[inline]
pub fn clamp_and_scale(x: f32) -> f32 {
    let v = max_number(x, -1000.0);
    let v = min_number(v, 1000.0);
    let s = v * 32768.0;
    let s = min_number(s, 163840.0);
    max_number(s, -163840.0)
}

/// Per-channel history ring of `_ComputeBlockMode` (0x47ddc, `chan+0x138`): `push` returns the 1024-sample block delayed by 1088 samples
/// (zeros before the first sample) and stores the 1024 new samples. Read happens before the write, as in the binary; the two spans are
/// disjoint (`2112 = 1088 + 1024`).
#[derive(Clone, Debug)]
pub struct HistoryRing {
    pub buf: Vec<f32>,
    pub wpos: usize,
}

impl Default for HistoryRing {
    fn default() -> Self {
        Self::new()
    }
}

impl HistoryRing {
    pub fn new() -> Self {
        Self { buf: vec![0.0; RING_SIZE], wpos: 0 }
    }

    /// `rpos = (S - D + W) mod S`; copy `L` samples (wrapping) out, write `L` new samples at `W` (wrapping), `W = (W + L) mod S`.
    pub fn push(&mut self, new: &[f32], delayed: &mut [f32]) {
        assert!(new.len() == FRAME_LEN && delayed.len() == FRAME_LEN);
        let s = RING_SIZE;
        let rpos = (s - RING_DELAY + self.wpos) % s;
        let n1 = (s - rpos).min(FRAME_LEN);
        delayed[..n1].copy_from_slice(&self.buf[rpos..rpos + n1]);
        if n1 != FRAME_LEN {
            delayed[n1..].copy_from_slice(&self.buf[..FRAME_LEN - n1]);
        }
        let w = self.wpos;
        let n1 = (s - w).min(FRAME_LEN);
        self.buf[w..w + n1].copy_from_slice(&new[..n1]);
        if n1 != FRAME_LEN {
            self.buf[..FRAME_LEN - n1].copy_from_slice(&new[n1..]);
        }
        self.wpos = (w + FRAME_LEN) % s;
    }
}

/// Number of access units (= `EncodeFrame` calls = packets) the converter emits for `n` input frames: `ceil((n + 2112) / 1024)`.
/// (Empirical rule of the original; every access-unit count of the fixtures follows it.)
pub fn au_count(n_frames: u64) -> u64 {
    (n_frames + PRIMING_FRAMES as u64).div_ceil(FRAME_LEN as u64)
}

/// `ExtAudioFileWrite` chunks of any size -> exact 1024-frame client-float blocks; at EOS the partial block is zero padded and zero blocks follow
/// until `au_count(n)` blocks (= access units) have been produced (`ProduceOutputPackets` EOF path).
pub struct InputFramer {
    nch: usize,
    pending: Vec<Vec<f32>>,
    total_frames: u64,
    blocks_emitted: u64,
}

impl InputFramer {
    pub fn new(nch: usize) -> Self {
        assert!(nch > 0);
        Self { nch, pending: vec![Vec::new(); nch], total_frames: 0, blocks_emitted: 0 }
    }

    /// Append interleaved client floats (any number of whole frames).
    pub fn push_interleaved(&mut self, samples: &[f32]) {
        assert!(samples.len() % self.nch == 0);
        let n = samples.len() / self.nch;
        for i in 0..n {
            for c in 0..self.nch {
                self.pending[c].push(samples[i * self.nch + c]);
            }
        }
        self.total_frames += n as u64;
    }

    /// Next full block (planar, client floats, 1024 frames per channel) if buffered.
    pub fn pop_block(&mut self) -> Option<Vec<Vec<f32>>> {
        if self.pending[0].len() < FRAME_LEN {
            return None;
        }
        let blk = self.pending.iter_mut().map(|p| p.drain(..FRAME_LEN).collect()).collect();
        self.blocks_emitted += 1;
        Some(blk)
    }

    /// End of stream: remaining blocks (the zero-padded partial block and all-zero flush blocks) so that `au_count(total frames)` blocks have
    /// been produced in total. Consumes the framer.
    pub fn finish(mut self) -> Vec<Vec<Vec<f32>>> {
        let target = au_count(self.total_frames);
        let mut out = Vec::new();
        while self.blocks_emitted < target {
            for p in &mut self.pending {
                p.resize(FRAME_LEN.max(p.len()), 0.0);
            }
            let blk: Vec<Vec<f32>> = self.pending.iter_mut().map(|p| p.drain(..FRAME_LEN).collect()).collect();
            out.push(blk);
            self.blocks_emitted += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_clamp_semantics() {
        assert_eq!(clamp_and_scale(0.5), 16384.0);
        assert_eq!(clamp_and_scale(-5.0), -163840.0);
        assert_eq!(clamp_and_scale(f32::NAN), -163840.0);
        assert_eq!(clamp_and_scale(f32::INFINITY), 163840.0);
        assert_eq!(clamp_and_scale(-0.0).to_bits(), (-0.0f32).to_bits());
    }

    #[test]
    fn ring_delays_by_1088() {
        let mut ring = HistoryRing::new();
        let mut out = vec![0.0f32; FRAME_LEN];
        let mut stream = Vec::new();
        for f in 0..6usize {
            let blk: Vec<f32> = (0..FRAME_LEN).map(|i| (f * FRAME_LEN + i + 1) as f32).collect();
            stream.extend_from_slice(&blk);
            ring.push(&blk, &mut out);
            for (k, v) in out.iter().enumerate() {
                let p = (FRAME_LEN * f + k) as isize - RING_DELAY as isize;
                let want = if p < 0 { 0.0 } else { stream[p as usize] };
                assert_eq!(*v, want, "frame {f} sample {k}");
            }
        }
    }

    #[test]
    fn au_count_examples() {
        assert_eq!(au_count(1), 3); // ceil(2113 / 1024)
        assert_eq!(au_count(4096), 7);
    }
}
