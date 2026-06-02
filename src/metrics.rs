use crate::types::DocumentMetrics;

pub fn compute_metrics(content: &str) -> DocumentMetrics {
    if content.is_empty() {
        return DocumentMetrics {
            words: 0,
            lines: 0,
            characters: 0,
            headings: 0,
        };
    }

    let words = content.split_whitespace().count() as u32;
    let characters = content.len() as u32;

    let mut lines: u32 = 0;
    let mut headings: u32 = 0;
    for line in content.lines() {
        lines += 1;
        let trimmed = line.trim_start();
        if trimmed.starts_with("# ")
            || trimmed.starts_with("## ")
            || trimmed.starts_with("### ")
            || trimmed.starts_with("#### ")
            || trimmed.starts_with("##### ")
            || trimmed.starts_with("###### ")
        {
            headings += 1;
        }
    }

    DocumentMetrics {
        words,
        lines,
        characters,
        headings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_content() {
        let m = compute_metrics("");
        assert_eq!(m.words, 0);
        assert_eq!(m.lines, 0);
        assert_eq!(m.characters, 0);
        assert_eq!(m.headings, 0);
    }

    #[test]
    fn test_single_word() {
        let m = compute_metrics("hello");
        assert_eq!(m.words, 1);
        assert_eq!(m.lines, 1);
        assert_eq!(m.characters, 5);
        assert_eq!(m.headings, 0);
    }

    #[test]
    fn test_multiline() {
        let m = compute_metrics("hello\nworld\nfoo");
        assert_eq!(m.words, 3);
        assert_eq!(m.lines, 3);
    }

    #[test]
    fn test_trailing_newline() {
        let m = compute_metrics("hello\n");
        assert_eq!(m.lines, 1);
    }

    #[test]
    fn test_multiple_whitespace() {
        let m = compute_metrics("hello   world\t\ttab");
        assert_eq!(m.words, 3);
    }

    #[test]
    fn test_unicode() {
        let m = compute_metrics("héllo wörld");
        assert_eq!(m.words, 2);
        assert!(m.characters > 11);
    }

    #[test]
    fn test_headings() {
        let content = "# H1\n## H2\n### H3\n#### H4\n##### H5\n###### H6";
        let m = compute_metrics(content);
        assert_eq!(m.headings, 6);
    }

    #[test]
    fn test_heading_no_space() {
        let m = compute_metrics("#nospace");
        assert_eq!(m.headings, 0);
    }

    #[test]
    fn test_heading_too_many_hashes() {
        let m = compute_metrics("####### H7");
        assert_eq!(m.headings, 0);
    }

    #[test]
    fn test_characters_is_byte_length() {
        let m = compute_metrics("é");
        assert_eq!(m.characters, 2);
    }

    #[test]
    fn test_heading_in_code_block_still_counts() {
        // PRD says count lines matching regex ^#{1,6}\s — no code block awareness
        let content = "```\n# heading inside code\n```";
        let m = compute_metrics(content);
        assert_eq!(m.headings, 1);
    }

    #[test]
    fn test_indented_heading() {
        let m = compute_metrics("  # indented heading");
        assert_eq!(m.headings, 1);
    }

    #[test]
    fn test_only_whitespace_content() {
        let m = compute_metrics("   \n  \n\t");
        assert_eq!(m.words, 0);
        assert_eq!(m.lines, 3);
    }

    #[test]
    fn test_large_content_metrics() {
        let word = "specification ";
        let content = word.repeat(1000);
        let m = compute_metrics(&content);
        assert_eq!(m.words, 1000);
        assert_eq!(m.lines, 1);
        assert_eq!(m.headings, 0);
    }
}
