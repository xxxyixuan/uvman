//! "did you mean" similarity suggestions (spell-correction design from
//! mise/uv).

/// Damerau-Levenshtein distance (OSA variant; supports adjacent transpositions,
/// case-insensitive).
///
/// Unlike plain Levenshtein, recognizes frequent transposition typos like
/// "mkae"→"make".
fn damerau_levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.to_lowercase().chars().collect();
    let b: Vec<char> = b.to_lowercase().chars().collect();
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }

    // prev2: row i-2; prev: row i-1; curr: current row.
    // prev2/prev are both initialized to row 0 (prev2 is read only when i>=2)
    let mut prev2: Vec<usize> = (0..=m).collect();
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut curr = vec![0usize; m + 1];
    for i in 1..=n {
        curr[0] = i;
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
            // adjacent transposition: a[i-2..i] is the reverse of b[j-2..j]
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                curr[j] = curr[j].min(prev2[j - 2] + 1);
            }
        }
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[m]
}

/// Find names in candidates most similar to input, returning up to 3 ascending
/// by distance.
///
/// Threshold is `max(1, len/3)`: very short input (e.g. "no") allows only 1
/// edit, avoiding absurd suggestions. When no edit-distance hit, fall back to
/// first-segment prefix match (e.g. "no-such" → "node") to cover hyphenated
/// spelling drift.
pub fn did_you_mean(input: &str, candidates: &[String]) -> Vec<String> {
    let threshold = (input.len() / 3).max(1);
    // Score on borrowed &str; only the (at most 3) returned names are cloned
    let mut scored: Vec<(usize, &str)> = candidates
        .iter()
        .map(String::as_str)
        .filter(|c| !c.is_empty())
        .map(|c| (damerau_levenshtein(input, c), c))
        .filter(|(d, _)| *d <= threshold)
        .collect();
    if !scored.is_empty() {
        scored.sort_unstable_by(|x, y| x.0.cmp(&y.0).then_with(|| x.1.cmp(y.1)));
        return scored.into_iter().take(3).map(|(_, c)| c.to_string()).collect();
    }

    // Fallback: first hyphen-segment as a prefix (meaningful only if len >= 2)
    let first_token = input.split('-').next().unwrap_or_default();
    if first_token.len() >= 2 {
        let prefix = first_token.to_lowercase();
        let mut matched: Vec<String> =
            candidates.iter().filter(|c| c.to_lowercase().starts_with(&prefix)).cloned().collect();
        matched.sort_unstable();
        matched.truncate(3);
        return matched;
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates() -> Vec<String> {
        ["node", "npm", "make", "cmake", "go", "rust"].iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_exact_prefix() {
        assert_eq!(did_you_mean("nod", &candidates()), vec!["node"]);
    }

    #[test]
    fn test_typo() {
        assert_eq!(did_you_mean("mkae", &candidates()), vec!["make"]);
    }

    #[test]
    fn test_no_match() {
        assert!(did_you_mean("zzzzzzzz", &candidates()).is_empty());
    }

    #[test]
    fn test_short_input_tight_threshold() {
        // Short input allows only 1 edit to avoid matching distant names
        assert_eq!(did_you_mean("np", &candidates()), vec!["npm"]);
    }

    #[test]
    fn test_hyphen_fallback_prefix() {
        // When edits are too far, fall back to first-segment prefix: "no-such" → "node"
        assert_eq!(did_you_mean("no-such", &candidates()), vec!["node"]);
    }

    #[test]
    fn test_hyphen_fallback_no_hit() {
        // No suggestion when the prefix gives no lead
        assert!(did_you_mean("zz-qq", &candidates()).is_empty());
    }

    #[test]
    fn test_damerau_levenshtein_basics() {
        assert_eq!(damerau_levenshtein("", ""), 0);
        assert_eq!(damerau_levenshtein("abc", "abc"), 0);
        assert_eq!(damerau_levenshtein("kitten", "sitting"), 3);
        assert_eq!(damerau_levenshtein("NODE", "node"), 0);
        // transposition counts as one edit
        assert_eq!(damerau_levenshtein("mkae", "make"), 1);
        assert_eq!(damerau_levenshtein("taeh", "teach"), 2);
    }
}
