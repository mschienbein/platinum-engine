//! Final Pro-core interpolator, corresponding to PitchShiftUnit `0x0e808`.
//!
//! This is the constructor-selected cubic-Hermite branch, not the distinct
//! interpolator used by PSOLA. Positions are binary64; sample operations and
//! the step supplied by the caller are binary32.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterpolateError {
    InvalidChannelCount,
    UnequalInputLengths,
    InputTooShort,
    InputTooLong,
    InvalidStep,
    OutputTooShort,
}

/// Stateful final interpolator. The caller supplies planar blocks and output
/// storage; exactly three samples of history per channel are retained.
#[derive(Clone, Debug)]
pub struct HermiteInterpolator {
    history: Vec<[f32; 3]>,
    input_scratch: Vec<Vec<f32>>,
    position: f64,
    index: i32,
    fraction: f32,
}

impl HermiteInterpolator {
    pub fn new(channels: usize) -> Result<Self, InterpolateError> {
        if !(1..=2).contains(&channels) {
            return Err(InterpolateError::InvalidChannelCount);
        }
        Ok(Self {
            history: vec![[0.0; 3]; channels],
            input_scratch: Vec::new(),
            position: 0.0,
            index: 0,
            fraction: 0.0,
        })
    }

    pub fn channels(&self) -> usize {
        self.history.len()
    }

    #[cfg(test)]
    pub(super) fn reference_state(&self) -> (f64, f32, i32) {
        (self.position, self.fraction, self.index)
    }

    pub fn reset(&mut self) {
        self.history.fill([0.0; 3]);
        self.position = 0.0;
        self.index = 0;
        self.fraction = 0.0;
    }

    /// The native caller may use the same planes for input and output:
    /// `0x0e808` first copies the complete input block to its own scratch.
    /// Each plane must contain `frames` input samples and enough total space
    /// for the generated output. The scratch allocation is retained.
    pub fn process_in_place(
        &mut self,
        planes: &mut [Vec<f32>],
        frames: usize,
        step: f32,
    ) -> Result<usize, InterpolateError> {
        if planes.len() != self.channels() { return Err(InterpolateError::InvalidChannelCount); }
        if planes.iter().any(|plane| plane.len() < frames) { return Err(InterpolateError::InputTooShort); }
        if frames > i32::MAX as usize { return Err(InterpolateError::InputTooLong); }
        if !step.is_finite() || step <= 0.0 { return Err(InterpolateError::InvalidStep); }
        let mut scratch = std::mem::take(&mut self.input_scratch);
        if scratch.is_empty() { scratch = (0..self.channels()).map(|_| Vec::new()).collect(); }
        for (copy, plane) in scratch.iter_mut().zip(planes.iter()) {
            copy.clear();
            copy.extend_from_slice(&plane[..frames]);
        }
        let result = if planes.len() == 1 {
            self.process_block(&[scratch[0].as_slice()], &mut [planes[0].as_mut_slice()], step)
        } else {
            let (left, right) = planes.split_at_mut(1);
            self.process_block(&[scratch[0].as_slice(), scratch[1].as_slice()],
                &mut [left[0].as_mut_slice(), right[0].as_mut_slice()], step)
        };
        self.input_scratch = scratch;
        result
    }

    /// Process one complete input block, returning the number of output frames.
    /// All output planes must have room for that many frames. If they do not,
    /// state is unchanged, although output planes may already contain a prefix.
    /// `step` is the native positive finite f32 input-position increment.
    pub fn process_block(
        &mut self,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        step: f32,
    ) -> Result<usize, InterpolateError> {
        if input.len() != self.channels() || output.len() != self.channels() {
            return Err(InterpolateError::InvalidChannelCount);
        }
        let len = input[0].len();
        if input.iter().any(|plane| plane.len() != len) {
            return Err(InterpolateError::UnequalInputLengths);
        }
        if len > i32::MAX as usize {
            return Err(InterpolateError::InputTooLong);
        }
        if !step.is_finite() || step <= 0.0 {
            return Err(InterpolateError::InvalidStep);
        }

        let output_capacity = output.iter().map(|plane| plane.len()).min().unwrap();
        let mut position = self.position;
        let mut index = self.index;
        let mut fraction = self.fraction;
        let mut written = 0;
        while index < len as i32 {
            if written == output_capacity {
                return Err(InterpolateError::OutputTooShort);
            }
            let t = fraction;
            let t2 = t * t;
            let t3 = t * t2;
            let twice_t3 = t3 + t3;
            let three_t2 = t2 * 3.0;
            let h0 = (twice_t3 - three_t2) + 1.0;
            let h1 = t + (t3 - (t2 + t2));
            let h2 = t3 - t2;
            let h3 = three_t2 - twice_t3;

            for channel in 0..input.len() {
                let samples = input[channel];
                let previous = self.history[channel];
                let x0 = sample(previous, samples, index - 3);
                let x1 = sample(previous, samples, index - 2);
                let x2 = sample(previous, samples, index - 1);
                let x3 = sample(previous, samples, index);
                let m0 = (((x1 - x0) + x2) - x1) * 0.5;
                let m1 = (((x2 - x1) + x3) - x2) * 0.5;
                let y = (((h0 * x1) + (m0 * h1)) + (m1 * h2)) + (h3 * x2);
                output[channel][written] = y;
            }
            written += 1;
            position += f64::from(step);
            index = position as i32;
            fraction = (position - f64::from(index)) as f32;
        }

        for (history, plane) in self.history.iter_mut().zip(input) {
            *history = [
                sample(*history, plane, len as i32 - 3),
                sample(*history, plane, len as i32 - 2),
                sample(*history, plane, len as i32 - 1),
            ];
        }
        position -= len as f64;
        self.position = position;
        self.index = position as i32;
        // The reference retains the fractional f32 computed before subtracting
        // the integral input length; it does not recompute it at this point.
        self.fraction = fraction;
        Ok(written)
    }
}

#[inline]
fn sample(history: [f32; 3], input: &[f32], index: i32) -> f32 {
    if index < 0 {
        history[(index + 3) as usize]
    } else {
        input[index as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::{HermiteInterpolator, InterpolateError};

    #[test]
    fn unity_step_has_the_native_two_sample_history_delay() {
        let mut interpolation = HermiteInterpolator::new(1).unwrap();
        let mut first = [0.0; 4];
        let written = interpolation.process_block(&[&[1.0, 2.0, 3.0, 4.0]], &mut [&mut first], 1.0).unwrap();
        assert_eq!(written, 4);
        assert_eq!(first, [0.0, 0.0, 1.0, 2.0]);
        let mut second = [0.0; 2];
        assert_eq!(interpolation.process_block(&[&[5.0, 6.0]], &mut [&mut second], 1.0).unwrap(), 2);
        assert_eq!(second, [3.0, 4.0]);
    }

    #[test]
    fn half_step_uses_hermite_tangents_in_reference_order() {
        let mut interpolation = HermiteInterpolator::new(1).unwrap();
        let mut output = [0.0; 6];
        let n = interpolation.process_block(&[&[1.0, 2.0, 3.0]], &mut [&mut output], 0.5).unwrap();
        assert_eq!(n, 6);
        assert_eq!(output.map(f32::to_bits), [0.0f32, -0.0625, 0.0, 0.4375, 1.0, 1.5].map(f32::to_bits));
    }

    #[test]
    fn in_place_interpolation_copies_input_before_writing_output() {
        let mut separate = HermiteInterpolator::new(2).unwrap();
        let mut in_place = HermiteInterpolator::new(2).unwrap();
        let left = [1.0, -0.25, 0.5];
        let right = [0.75, 0.125, -1.0];
        let mut expected_left = [0.0; 10];
        let mut expected_right = [0.0; 10];
        let count = separate.process_block(&[&left, &right], &mut [&mut expected_left, &mut expected_right], 0.5).unwrap();
        let mut work = vec![vec![1.0, -0.25, 0.5, 9.0, 9.0, 9.0, 9.0, 9.0, 9.0, 9.0],
                            vec![0.75, 0.125, -1.0, 9.0, 9.0, 9.0, 9.0, 9.0, 9.0, 9.0]];
        assert_eq!(in_place.process_in_place(&mut work, 3, 0.5).unwrap(), count);
        assert_eq!(&work[0][..count], &expected_left[..count]);
        assert_eq!(&work[1][..count], &expected_right[..count]);
    }

    #[test]
    fn one_sample_blocks_preserve_stereo_history_and_fraction() {
        let left = [0.125, -0.75, 0.5, 1.0, -0.25, 0.375, 0.25];
        let right = [-0.5, 0.0, 0.25, -1.0, 0.125, -0.125, 0.75];
        let mut whole = HermiteInterpolator::new(2).unwrap();
        let mut whole_l = [0.0; 32];
        let mut whole_r = [0.0; 32];
        let count = whole.process_block(&[&left, &right], &mut [&mut whole_l, &mut whole_r], 0.75).unwrap();
        let mut split = HermiteInterpolator::new(2).unwrap();
        let mut split_l = [0.0; 32];
        let mut split_r = [0.0; 32];
        let mut position = 0;
        for i in 0..left.len() {
            position += split.process_block(&[&left[i..i + 1], &right[i..i + 1]],
                &mut [&mut split_l[position..], &mut split_r[position..]], 0.75).unwrap();
        }
        assert_eq!(position, count);
        assert_eq!(whole_l[..count].iter().map(|x| x.to_bits()).collect::<Vec<_>>(), split_l[..count].iter().map(|x| x.to_bits()).collect::<Vec<_>>());
        assert_eq!(whole_r[..count].iter().map(|x| x.to_bits()).collect::<Vec<_>>(), split_r[..count].iter().map(|x| x.to_bits()).collect::<Vec<_>>());
    }

    #[test]
    fn output_shortage_does_not_advance_state() {
        let mut interpolation = HermiteInterpolator::new(1).unwrap();
        let mut short = [0.0; 1];
        assert_eq!(interpolation.process_block(&[&[1.0, 2.0]], &mut [&mut short], 1.0), Err(InterpolateError::OutputTooShort));
        let mut result = [1.0; 2];
        assert_eq!(interpolation.process_block(&[&[1.0, 2.0]], &mut [&mut result], 1.0), Ok(2));
        assert_eq!(result, [0.0, 0.0]);
        interpolation.reset();
        assert_eq!(interpolation.process_block(&[&[1.0, 2.0]], &mut [&mut result], 1.0), Ok(2));
        assert_eq!(result, [0.0, 0.0]);
    }
}
