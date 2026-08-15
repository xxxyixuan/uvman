//! "did you mean" 相似度建议（参考 mise/uv 的拼写纠错设计）。

/// Damerau-Levenshtein 距离（OSA 变体，支持相邻字符转置，大小写不敏感）。
///
/// 相比标准 Levenshtein，额外识别 "mkae"→"make" 这类高频转置拼写错误。
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

    // prev2: i-2 行；prev: i-1 行；curr: 当前行。
    // prev2/prev 均初始化为第 0 行（i>=2 才会读取 prev2）
    let mut prev2: Vec<usize> = (0..=m).collect();
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut curr = vec![0usize; m + 1];
    for i in 1..=n {
        curr[0] = i;
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
            // 相邻转置：a[i-2..i] 与 b[j-2..j] 互为反转
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                curr[j] = curr[j].min(prev2[j - 2] + 1);
            }
        }
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[m]
}

/// 从候选中找出与输入最相似的名称，按距离升序返回至多 3 个。
///
/// 距离阈值为 `max(1, len/3)`：过短输入（如 "no"）只允许 1 处差异，
/// 避免给出离谱的建议。编辑距离无命中时回退到首段前缀匹配
/// （如 "no-such" → 以 "no" 开头的 "node"），覆盖连字符类拼写偏差。
pub fn did_you_mean(input: &str, candidates: &[String]) -> Vec<String> {
    let threshold = (input.len() / 3).max(1);
    let mut scored: Vec<(usize, String)> = candidates
        .iter()
        .filter(|c| !c.is_empty())
        .map(|c| (damerau_levenshtein(input, c), c.clone()))
        .filter(|(d, _)| *d <= threshold)
        .collect();
    if !scored.is_empty() {
        scored.sort_by(|x, y| x.0.cmp(&y.0).then_with(|| x.1.cmp(&y.1)));
        return scored.into_iter().map(|(_, c)| c).take(3).collect();
    }

    // 回退：取首个连字符段作为前缀（长度 >= 2 才有意义）
    let first_token = input.split('-').next().unwrap_or_default();
    if first_token.len() >= 2 {
        let prefix = first_token.to_lowercase();
        let mut matched: Vec<String> = candidates
            .iter()
            .filter(|c| c.to_lowercase().starts_with(&prefix))
            .cloned()
            .collect();
        matched.sort();
        matched.truncate(3);
        return matched;
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates() -> Vec<String> {
        ["node", "npm", "make", "cmake", "go", "rust"]
            .iter()
            .map(|s| s.to_string())
            .collect()
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
        // 输入过短时只允许 1 处差异，避免 "no" 匹配到 "go" 之外的远名
        assert_eq!(did_you_mean("np", &candidates()), vec!["npm"]);
    }

    #[test]
    fn test_hyphen_fallback_prefix() {
        // 编辑距离过远时按首段前缀回退："no-such" → "node"
        assert_eq!(did_you_mean("no-such", &candidates()), vec!["node"]);
    }

    #[test]
    fn test_hyphen_fallback_no_hit() {
        // 前缀也无线索时不给建议
        assert!(did_you_mean("zz-qq", &candidates()).is_empty());
    }

    #[test]
    fn test_damerau_levenshtein_basics() {
        assert_eq!(damerau_levenshtein("", ""), 0);
        assert_eq!(damerau_levenshtein("abc", "abc"), 0);
        assert_eq!(damerau_levenshtein("kitten", "sitting"), 3);
        assert_eq!(damerau_levenshtein("NODE", "node"), 0);
        // 转置算 1 次编辑
        assert_eq!(damerau_levenshtein("mkae", "make"), 1);
        assert_eq!(damerau_levenshtein("taeh", "teach"), 2);
    }
}
