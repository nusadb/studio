//! The embedding map on the Vectors tab: a two-dimensional projection (principal component
//! analysis) of a sample of vectors, so clusters and outliers can be seen. Computed here on at
//! most a few thousand rows by power iteration; no linear-algebra crate needed.

/// Iterations per component; plenty for a stable direction on a sample.
const ITERATIONS: usize = 60;

/// Project `vectors` (all the same length) onto their first two principal components. Returns
/// one (x, y) per vector, scaled to 0..1, and the share of the variance each axis explains.
#[must_use]
pub fn project(vectors: &[Vec<f64>]) -> (Vec<(f64, f64)>, [f64; 2]) {
    let n = vectors.len();
    let Some(d) = vectors.first().map(Vec::len) else {
        return (Vec::new(), [0.0, 0.0]);
    };
    if d == 0 || vectors.iter().any(|v| v.len() != d) {
        return (Vec::new(), [0.0, 0.0]);
    }
    #[allow(clippy::cast_precision_loss)]
    let nf = n as f64;
    let mut mean = vec![0.0; d];
    for v in vectors {
        for (m, x) in mean.iter_mut().zip(v) {
            *m += x / nf;
        }
    }
    let centred: Vec<Vec<f64>> = vectors
        .iter()
        .map(|v| v.iter().zip(&mean).map(|(x, m)| x - m).collect())
        .collect();
    let total: f64 = centred.iter().flatten().map(|x| x * x).sum::<f64>() / nf;

    // C·v without forming C: Xᵀ(X v) / n.
    let apply = |v: &[f64]| -> Vec<f64> {
        let mut out = vec![0.0; d];
        for row in &centred {
            let dot: f64 = row.iter().zip(v).map(|(a, b)| a * b).sum();
            for (o, r) in out.iter_mut().zip(row) {
                *o += dot * r / nf;
            }
        }
        out
    };
    let normalise = |v: &mut Vec<f64>| {
        let len = v.iter().map(|x| x * x).sum::<f64>().sqrt();
        if len > 0.0 {
            v.iter_mut().for_each(|x| *x /= len);
        }
        len
    };
    let component = |deflate: Option<&[f64]>| -> (Vec<f64>, f64) {
        // A fixed, uneven start so results are reproducible.
        #[allow(clippy::cast_precision_loss)]
        let mut v: Vec<f64> = (0..d).map(|i| 1.0 + (i % 7) as f64 * 0.13).collect();
        let mut value = 0.0;
        for _ in 0..ITERATIONS {
            let mut next = apply(&v);
            if let Some(p) = deflate {
                let dot: f64 = next.iter().zip(p).map(|(a, b)| a * b).sum();
                next.iter_mut().zip(p).for_each(|(x, q)| *x -= dot * q);
            }
            value = normalise(&mut next);
            v = next;
        }
        (v, value)
    };
    let (pc1, l1) = component(None);
    let (pc2, l2) = component(Some(&pc1));
    let coords: Vec<(f64, f64)> = centred
        .iter()
        .map(|r| {
            (
                r.iter().zip(&pc1).map(|(a, b)| a * b).sum(),
                r.iter().zip(&pc2).map(|(a, b)| a * b).sum(),
            )
        })
        .collect();
    let range = |f: fn(&(f64, f64)) -> f64| {
        let lo = coords.iter().map(f).fold(f64::INFINITY, f64::min);
        let hi = coords.iter().map(f).fold(f64::NEG_INFINITY, f64::max);
        (lo, if hi > lo { hi - lo } else { 1.0 })
    };
    let (xl, xr) = range(|p| p.0);
    let (yl, yr) = range(|p| p.1);
    let scaled = coords
        .iter()
        .map(|(x, y)| (0.04 + 0.92 * (x - xl) / xr, 0.04 + 0.92 * (y - yl) / yr))
        .collect();
    let share = |l: f64| if total > 0.0 { l / total } else { 0.0 };
    (scaled, [share(l1), share(l2)])
}

#[cfg(test)]
mod tests {
    use super::project;

    #[test]
    fn two_clusters_separate_on_the_first_axis() {
        let mut v = Vec::new();
        for i in 0..10 {
            let e = f64::from(i) * 0.01;
            v.push(vec![1.0 + e, 0.0, 0.1]);
            v.push(vec![-1.0 - e, 0.0, 0.1]);
        }
        let (p, share) = project(&v);
        assert_eq!(p.len(), 20);
        assert!(share[0] > 0.9);
        let a = p[0].0;
        let b = p[1].0;
        assert!((a - b).abs() > 0.8);
        assert!(
            p.iter()
                .all(|(x, y)| (0.0..=1.0).contains(x) && (0.0..=1.0).contains(y))
        );
    }
}
