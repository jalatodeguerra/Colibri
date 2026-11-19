//! "Did you mean" suggestions for unknown names.

/// The edit distance between two strings, counting characters: insertions, deletions,
/// substitutions and swaps of two neighbours (so `Quti` is one edit from `Quit`).
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut older: Vec<usize> = Vec::new();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, &ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            let mut best = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
            if i > 0 && j > 0 && ca == b[j - 1] && a[i - 1] == cb {
                best = best.min(older[j - 1] + 1);
            }
            cur.push(best);
        }
        older = std::mem::replace(&mut prev, cur);
    }
    prev[b.len()]
}

/// The candidate closest to `name`, if one is close enough to be a plausible typo.
pub fn closest<'a>(name: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let limit = (name.chars().count() / 3).max(1);
    candidates
        .into_iter()
        .filter(|c| *c != name)
        .map(|c| (edit_distance(name, c), c))
        .filter(|&(d, _)| d <= limit)
        .min_by_key(|&(d, c)| (d, c))
        .map(|(_, c)| c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distances() {
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("same", "same"), 0);
        assert_eq!(edit_distance("Quti", "Quit"), 1);
        assert_eq!(edit_distance("ab", "ba"), 1);
    }

    #[test]
    fn suggests_only_near_misses() {
        let names = ["score", "scale", "hello"];
        assert_eq!(closest("scor", names), Some("score"));
        assert_eq!(closest("zzzzz", names), None);
        assert_eq!(closest("scare", names), Some("scale"));
    }
}
