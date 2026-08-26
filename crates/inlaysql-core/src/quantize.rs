//! Deterministic symmetric scalar quantisation used by rows and ANN indexes.

use alloc::vec::Vec;

/// One vector represented by signed bytes and a per-vector scale.
///
/// `value ~= code * scale`. `-128` is deliberately unused so the positive and
/// negative ranges are symmetric around zero.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Q8Vector {
    pub(crate) scale: f32,
    pub(crate) values: Vec<i8>,
}

impl Q8Vector {
    /// Quantise one vector deterministically.
    pub(crate) fn from_f32(values: &[f32]) -> Self {
        let max_abs = values
            .iter()
            .fold(0.0f32, |largest, value| largest.max(value.abs()));
        let scale = if max_abs == 0.0 || !max_abs.is_finite() {
            1.0
        } else {
            max_abs / 127.0
        };
        let values = values
            .iter()
            .map(|value| libm::roundf(*value / scale).clamp(-127.0, 127.0) as i8)
            .collect();
        Self { scale, values }
    }

    /// Reconstruct the `f32` values exposed through the public SQL API.
    pub(crate) fn to_f32(&self) -> Vec<f32> {
        self.values
            .iter()
            .map(|value| *value as f32 * self.scale)
            .collect()
    }

    /// Approximate dot product without allocating a dequantised vector.
    pub(crate) fn dot_f32(&self, other: &[f32]) -> f32 {
        self.values
            .iter()
            .zip(other)
            .map(|(left, right)| *left as f32 * self.scale * right)
            .sum()
    }

    /// Approximate dot product between two quantised vectors.
    pub(crate) fn dot_q8(&self, other: &Self) -> f32 {
        let scale = self.scale * other.scale;
        let integer_dot: i64 = self
            .values
            .iter()
            .zip(&other.values)
            .map(|(left, right)| i64::from(*left) * i64::from(*right))
            .sum();
        integer_dot as f32 * scale
    }

    /// Approximate squared Euclidean distance to a full-precision vector.
    ///
    /// Reconstructs `code * scale` per component rather than dequantising into
    /// a `Vec` first, for the same reason [`Q8Vector::dot_f32`] does. Unlike
    /// the dot products above this one is *not* scale-invariant — squared
    /// distance is what L2 measures, and the quantisation error rides along
    /// with it — which is why the int8 recall loss under L2 is measured
    /// separately from the exact one rather than assumed to be the same.
    pub(crate) fn l2_f32(&self, other: &[f32]) -> f32 {
        self.values
            .iter()
            .zip(other)
            .map(|(left, right)| {
                let delta = *left as f32 * self.scale - *right;
                delta * delta
            })
            .sum()
    }

    /// Approximate squared Euclidean distance between two quantised vectors.
    ///
    /// The integer trick [`Q8Vector::dot_q8`] uses does not apply: the two
    /// vectors carry different scales, so the difference has to be taken after
    /// reconstruction, not before.
    pub(crate) fn l2_q8(&self, other: &Self) -> f32 {
        self.values
            .iter()
            .zip(&other.values)
            .map(|(left, right)| {
                let delta = *left as f32 * self.scale - *right as f32 * other.scale;
                delta * delta
            })
            .sum()
    }

    pub(crate) fn payload_bytes(&self) -> usize {
        core::mem::size_of::<f32>() + self.values.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_quantisation_is_symmetric_and_bounded() {
        let q = Q8Vector::from_f32(&[-2.0, -1.0, 0.0, 1.0, 2.0]);
        assert_eq!(q.values, alloc::vec![-127, -64, 0, 64, 127]);
        let restored = q.to_f32();
        assert!((restored[0] + 2.0).abs() < 0.0001);
        assert!((restored[1] + 1.0).abs() < 0.01);
        assert!((restored[4] - 2.0).abs() < 0.0001);
    }

    #[test]
    fn zero_vector_has_a_stable_representation() {
        let q = Q8Vector::from_f32(&[0.0; 4]);
        assert_eq!(q.scale, 1.0);
        assert_eq!(q.values, alloc::vec![0; 4]);
    }
}
