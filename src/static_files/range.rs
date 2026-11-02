#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeDecision {
    Full,
    Partial { start: u64, length: u64 },
    Unsatisfiable,
}

fn number(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(value.bytes().fold(0_u64, |number, digit| {
        number
            .saturating_mul(10)
            .saturating_add(u64::from(digit - b'0'))
    }))
}

fn greater_decimal(first: &str, last: &str) -> bool {
    let first = first.trim_start_matches('0');
    let last = last.trim_start_matches('0');
    (first.len(), first) > (last.len(), last)
}

pub fn select(value: &str, length: u64) -> RangeDecision {
    let Some((unit, range)) = value.trim().split_once('=') else {
        return RangeDecision::Full;
    };
    if !unit.eq_ignore_ascii_case("bytes") || range.contains(',') {
        return RangeDecision::Full;
    }

    let Some((first, last)) = range.split_once('-') else {
        return RangeDecision::Full;
    };

    if first.is_empty() {
        let Some(suffix) = number(last) else {
            return RangeDecision::Full;
        };
        if suffix == 0 || length == 0 {
            return RangeDecision::Unsatisfiable;
        }
        let selected = suffix.min(length);
        return RangeDecision::Partial {
            start: length - selected,
            length: selected,
        };
    }

    let Some(start) = number(first) else {
        return RangeDecision::Full;
    };
    let end = if last.is_empty() {
        None
    } else {
        let Some(end) = number(last) else {
            return RangeDecision::Full;
        };
        if greater_decimal(first, last) {
            return RangeDecision::Full;
        }
        Some(end)
    };

    if start >= length {
        return RangeDecision::Unsatisfiable;
    }

    let end = end.unwrap_or(length - 1).min(length - 1);
    RangeDecision::Partial {
        start,
        length: end - start + 1,
    }
}

#[cfg(test)]
mod tests {
    use super::{RangeDecision, select};

    #[test]
    fn select_overflowing_decimals() {
        let cases = [
            (
                "bytes=-18446744073709551616",
                RangeDecision::Partial {
                    start: 0,
                    length: 10,
                },
            ),
            ("bytes=18446744073709551616-", RangeDecision::Unsatisfiable),
            (
                "bytes=2-18446744073709551616",
                RangeDecision::Partial {
                    start: 2,
                    length: 8,
                },
            ),
            (
                "bytes=18446744073709551617-18446744073709551616",
                RangeDecision::Full,
            ),
            (
                "bytes=00018446744073709551616-18446744073709551617",
                RangeDecision::Unsatisfiable,
            ),
        ];
        for (value, expected) in cases {
            assert_eq!(select(value, 10), expected, "{value}");
        }
    }
}
