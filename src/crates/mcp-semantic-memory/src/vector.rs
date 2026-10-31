//! Maths on vectors.

/// How far apart two vectors point: 0 for the same direction, 1 for unrelated,
/// 2 for opposite. A vector without length is unrelated to everything.
///
/// ```
/// use mcp_semantic_memory::vector::cosine_distance;
///
/// assert!(cosine_distance(&[1.0, 0.0], &[2.0, 0.0]).abs() < 1e-6);
/// assert!((cosine_distance(&[1.0, 0.0], &[0.0, 3.0]) - 1.0).abs() < 1e-6);
/// ```
pub fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
  let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
  let norm = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
  let (norm_a, norm_b) = (norm(a), norm(b));
  if norm_a == 0.0 || norm_b == 0.0 {
    1.0
  } else {
    1.0 - dot / (norm_a * norm_b)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn same_direction_is_zero() {
    assert!(cosine_distance(&[1.0, 0.0], &[2.0, 0.0]).abs() < 1e-6);
  }

  #[test]
  fn orthogonal_is_one() {
    assert!((cosine_distance(&[1.0, 0.0], &[0.0, 3.0]) - 1.0).abs() < 1e-6);
  }

  #[test]
  fn opposite_is_two() {
    assert!((cosine_distance(&[1.0, 0.0], &[-1.0, 0.0]) - 2.0).abs() < 1e-6);
  }

  #[test]
  fn empty_vector_is_unrelated() {
    assert_eq!(cosine_distance(&[0.0, 0.0], &[1.0, 0.0]), 1.0);
  }
}
