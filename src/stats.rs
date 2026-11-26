//! Quick numbers for the cells selected in a grid: how many,
//! and for the numbers among them the sum, average, minimum and maximum.

/// The status-bar line for `cells`, or empty for a single cell.
#[must_use]
pub fn summary<'a>(cells: impl Iterator<Item = Option<&'a str>>) -> String {
    let mut count = 0_u64;
    let mut nulls = 0_u64;
    let mut numbers: Vec<f64> = Vec::new();
    for cell in cells {
        count += 1;
        match cell {
            None => nulls += 1,
            Some(v) => {
                if let Ok(n) = v.trim().parse::<f64>()
                    && n.is_finite()
                {
                    numbers.push(n);
                }
            },
        }
    }
    if count < 2 {
        return String::new();
    }
    let mut parts = vec![format!("Count {count}")];
    if nulls > 0 {
        parts.push(format!("NULL {nulls}"));
    }
    if !numbers.is_empty() {
        let sum: f64 = numbers.iter().sum();
        #[allow(clippy::cast_precision_loss)]
        let avg = sum / numbers.len() as f64;
        let min = numbers.iter().copied().fold(f64::INFINITY, f64::min);
        let max = numbers.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        parts.push(format!("Sum {}", short(sum)));
        parts.push(format!("Avg {}", short(avg)));
        parts.push(format!("Min {}", short(min)));
        parts.push(format!("Max {}", short(max)));
    }
    parts.join("  ·  ")
}

fn short(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        return format!("{v:.0}");
    }
    let s = format!("{v:.4}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

#[cfg(test)]
mod tests {
    use super::summary;

    #[test]
    fn numbers_and_nulls() {
        let cells = [Some("10"), Some("20.5"), None, Some("x")];
        assert_eq!(
            summary(cells.into_iter()),
            "Count 4  ·  NULL 1  ·  Sum 30.5  ·  Avg 15.25  ·  Min 10  ·  Max 20.5"
        );
        assert_eq!(summary([Some("1")].into_iter()), "");
        assert_eq!(summary([Some("a"), Some("b")].into_iter()), "Count 2");
    }
}
