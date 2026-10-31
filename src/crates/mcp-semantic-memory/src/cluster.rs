//! Finding interests: weighted k-means over the memory vectors. Pure
//! functions with no I/O. Nothing in them is random: the same points always
//! give the same clusters.

use crate::ranking::count_as_float;
use crate::types::Vector;

/// A memory's vector and how much it pulls on the centre of its cluster.
#[derive(Debug, Clone, Copy)]
pub struct Point<'a> {
  /// The embedding of the memory.
  pub vector: &'a [f32],
  /// How much the memory counts. Zero or less counts for nothing.
  pub weight: f32,
}

/// What k-means found.
#[derive(Debug, Clone, PartialEq)]
pub struct Clusters {
  /// The centres, each of length one.
  pub centres: Vec<Vector>,
  /// The index of the centre of every point, in the order the points came.
  pub assignments: Vec<usize>,
}

/// How many interests to look for: the logarithm of the memory count to the
/// given base, between `min` and `max`, and never more than there are
/// memories.
///
/// ```
/// use mcp_semantic_memory::cluster::cluster_count;
///
/// assert_eq!(cluster_count(1_000, std::f32::consts::E, 2, 16), 6);
/// assert_eq!(cluster_count(3, std::f32::consts::E, 2, 16), 2);
/// assert_eq!(cluster_count(1, std::f32::consts::E, 2, 16), 1);
/// assert_eq!(cluster_count(1_000_000, 1.5, 2, 16), 16);
/// ```
pub fn cluster_count(
  memories: usize,
  base: f32,
  min: usize,
  max: usize,
) -> usize {
  let target = count_as_float(memories);
  let mut count = 0_usize;
  let mut reach = 1.0_f32;
  while count < max && base > 1.0 && reach * base <= target {
    reach *= base;
    count = count.saturating_add(1);
  }
  count.max(min).min(max).min(memories)
}

/// Spherical k-means: vectors are compared by their direction, as the search
/// compares them. The starting centres are picked one by one: the first is the
/// heaviest point, and each next one is the point with the most weight times
/// distance from the centres picked so far. Then points and centres are pulled
/// together for at most `iterations` rounds, or until no point changes
/// cluster.
///
/// There is nothing to find with no points or no clusters, and then nothing
/// comes back.
pub fn kmeans(points: &[Point<'_>], k: usize, iterations: usize) -> Clusters {
  if points.is_empty() || k == 0 {
    return Clusters {
      centres: Vec::new(),
      assignments: Vec::new(),
    };
  }

  let units: Vec<Vector> =
    points.iter().map(|point| unit(point.vector)).collect();
  let weights: Vec<f32> =
    points.iter().map(|point| point.weight.max(0.0)).collect();

  let mut centres = starting_centres(&units, &weights, k);
  let mut assignments = assign(&units, &centres);
  for _ in 0..iterations {
    centres = move_centres(&units, &weights, &assignments, centres);
    let next = assign(&units, &centres);
    if next == assignments {
      break;
    }
    assignments = next;
  }
  Clusters {
    centres,
    assignments,
  }
}

fn unit(vector: &[f32]) -> Vector {
  let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
  if norm == 0.0 {
    vector.to_vec()
  } else {
    vector.iter().map(|x| x / norm).collect()
  }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
  a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn starting_centres(
  units: &[Vector],
  weights: &[f32],
  k: usize,
) -> Vec<Vector> {
  let mut chosen: Vec<usize> = Vec::new();
  let mut distance = vec![f32::INFINITY; units.len()];

  while chosen.len() < k.min(units.len()) {
    let pull: Vec<f32> = if chosen.is_empty() {
      weights.to_vec()
    } else {
      weights.iter().zip(&distance).map(|(w, d)| w * d).collect()
    };
    let picked = strongest(&pull)
      .filter(|index| !chosen.contains(index))
      .or_else(|| (0..units.len()).find(|index| !chosen.contains(index)));
    let Some(index) = picked else {
      break;
    };
    chosen.push(index);
    if let Some(centre) = units.get(index) {
      for (nearest, unit) in distance.iter_mut().zip(units) {
        *nearest = nearest.min((1.0 - dot(centre, unit)).max(0.0));
      }
    }
  }
  chosen
    .iter()
    .filter_map(|index| units.get(*index).cloned())
    .collect()
}

/// The nearest centre of every point. A tie goes to the lower index.
fn assign(units: &[Vector], centres: &[Vector]) -> Vec<usize> {
  units
    .iter()
    .map(|point| {
      centres
        .iter()
        .enumerate()
        .map(|(index, centre)| (index, dot(point, centre)))
        .max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0)))
        .map_or(0, |(index, _)| index)
    })
    .collect()
}

/// Each centre moves to the weighted mean direction of its points. One that
/// has no points, or whose points cancel out, stays where it is.
fn move_centres(
  units: &[Vector],
  weights: &[f32],
  assignments: &[usize],
  centres: Vec<Vector>,
) -> Vec<Vector> {
  centres
    .into_iter()
    .enumerate()
    .map(|(index, previous)| {
      let mut sum = vec![0.0_f32; previous.len()];
      for ((point, weight), assigned) in
        units.iter().zip(weights).zip(assignments)
      {
        if *assigned == index {
          for (total, part) in sum.iter_mut().zip(point) {
            *total += part * weight;
          }
        }
      }
      if sum.iter().any(|part| *part != 0.0) {
        unit(&sum)
      } else {
        previous
      }
    })
    .collect()
}

/// The index of the biggest number, if any is above zero. A tie goes to the
/// lower index.
fn strongest(pull: &[f32]) -> Option<usize> {
  pull
    .iter()
    .enumerate()
    .filter(|(_, part)| **part > 0.0)
    .max_by(|a, b| a.1.total_cmp(b.1).then(b.0.cmp(&a.0)))
    .map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
  use super::*;

  const EAST: [f32; 2] = [1.0, 0.0];
  const NORTH: [f32; 2] = [0.0, 1.0];

  fn point(vector: &[f32]) -> Point<'_> {
    Point {
      vector,
      weight: 1.0,
    }
  }

  #[test]
  fn the_count_follows_the_logarithm_within_its_bounds() {
    let e = std::f32::consts::E;
    assert_eq!(cluster_count(1_000, e, 2, 16), 6);
    assert_eq!(cluster_count(20, e, 2, 16), 2);
    assert_eq!(cluster_count(1_000_000, e, 2, 5), 5);
    assert_eq!(cluster_count(2, e, 4, 16), 2);
    assert_eq!(cluster_count(0, e, 2, 16), 0);
  }

  #[test]
  fn a_base_of_one_or_less_gives_the_minimum() {
    assert_eq!(cluster_count(1_000, 1.0, 3, 16), 3);
    assert_eq!(cluster_count(1_000, 0.5, 3, 16), 3);
  }

  #[test]
  fn two_groups_of_points_make_two_clusters() {
    let vectors = [
      [1.0, 0.1],
      [0.9, 0.0],
      [1.0, -0.1],
      [0.0, 1.0],
      [0.1, 0.9],
      [-0.1, 1.0],
    ];
    let points: Vec<Point<'_>> = vectors.iter().map(|v| point(v)).collect();
    let clusters = kmeans(&points, 2, 50);

    assert_eq!(clusters.centres.len(), 2);
    let [a, b, c, d, e, f] = clusters.assignments[..] else {
      panic!("one assignment per point");
    };
    assert_eq!([a, b], [b, c]);
    assert_eq!([d, e], [e, f]);
    assert_ne!(a, d);
  }

  #[test]
  fn the_centres_have_length_one() {
    let vectors = [[3.0, 0.0], [5.0, 0.5], [0.0, 2.0]];
    let points: Vec<Point<'_>> = vectors.iter().map(|v| point(v)).collect();
    for centre in kmeans(&points, 2, 50).centres {
      let length = centre.iter().map(|x| x * x).sum::<f32>().sqrt();
      assert!((length - 1.0).abs() < 1e-5, "{length}");
    }
  }

  #[test]
  fn the_same_points_give_the_same_clusters() {
    let vectors: Vec<[f32; 2]> = (0..40_u8)
      .map(|n| {
        let angle = f32::from(n) * 0.37;
        [angle.cos(), angle.sin()]
      })
      .collect();
    let points: Vec<Point<'_>> = vectors.iter().map(|v| point(v)).collect();
    let first = kmeans(&points, 5, 50);
    assert_eq!(first, kmeans(&points, 5, 50));
  }

  #[test]
  fn a_heavy_point_pulls_its_centre() {
    let vectors = [[1.0, 0.2], [1.0, -0.2]];
    let heavy = [
      Point {
        vector: &vectors[0],
        weight: 9.0,
      },
      Point {
        vector: &vectors[1],
        weight: 1.0,
      },
    ];
    let clusters = kmeans(&heavy, 1, 50);
    assert!(clusters.centres[0][1] > 0.0, "{:?}", clusters.centres);
  }

  #[test]
  fn there_are_never_more_clusters_than_points() {
    let points = [point(&EAST), point(&NORTH)];
    let clusters = kmeans(&points, 5, 50);
    assert_eq!(clusters.centres.len(), 2);
  }

  #[test]
  fn identical_points_still_make_the_clusters_asked_for() {
    let points = [point(&EAST), point(&EAST), point(&EAST)];
    let clusters = kmeans(&points, 2, 50);
    assert_eq!(clusters.centres.len(), 2);
    assert_eq!(clusters.assignments.len(), 3);
  }

  #[test]
  fn nothing_to_cluster_gives_nothing() {
    let clusters = kmeans(&[], 3, 50);
    assert!(clusters.centres.is_empty() && clusters.assignments.is_empty());
    assert!(kmeans(&[point(&EAST)], 0, 50).centres.is_empty());
  }

  #[test]
  fn the_first_centre_is_the_heaviest_and_the_next_the_most_pulling() {
    let near = [0.99, 0.1];
    let units: Vec<Vector> = vec![EAST.to_vec(), near.to_vec(), NORTH.to_vec()];
    let weights = [5.0, 4.0, 1.0];
    let centres = starting_centres(&units, &weights, 2);
    assert_eq!(centres, [EAST.to_vec(), NORTH.to_vec()]);
  }
}
