use std::collections::HashSet;

use worker::kv::KvStore;

use crate::error::now_iso8601;
use crate::storage::{kv_get, kv_put, stats_key, meta_key};
use crate::types::{ConvergenceData, Meta, Stats, StatsRoundEntry};

pub fn compute_output_trend(word_counts: &[u32]) -> f64 {
    if word_counts.len() < 2 {
        return 0.0;
    }
    let max = *word_counts.iter().max().unwrap_or(&0) as f64;
    if max == 0.0 {
        return 0.0;
    }
    let latest = *word_counts.last().unwrap() as f64;
    (1.0 - latest / max).clamp(0.0, 1.0)
}

pub fn compute_change_velocity(word_counts: &[u32]) -> f64 {
    if word_counts.len() < 2 {
        return 0.0;
    }
    let deltas: Vec<u32> = word_counts
        .windows(2)
        .map(|w| (w[0] as i64 - w[1] as i64).unsigned_abs() as u32)
        .collect();
    let max_delta = *deltas.iter().max().unwrap_or(&0) as f64;
    if max_delta == 0.0 {
        return 1.0;
    }
    let latest_delta = *deltas.last().unwrap() as f64;
    (1.0 - latest_delta / max_delta).clamp(0.0, 1.0)
}

pub fn tokenize_to_word_set(content: &str) -> HashSet<String> {
    content
        .split_whitespace()
        .filter_map(|w| {
            let lower = w.to_lowercase();
            let trimmed = lower.trim_matches(|c: char| !c.is_alphanumeric());
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
        .collect()
}

pub fn compute_similarity(prev_set: &HashSet<String>, current_set: &HashSet<String>) -> f64 {
    let intersection = prev_set.intersection(current_set).count();
    let union = prev_set.union(current_set).count();
    if union == 0 {
        return 1.0;
    }
    intersection as f64 / union as f64
}

pub fn compute_convergence(
    output_trend: f64,
    change_velocity: f64,
    similarity_trend: f64,
) -> ConvergenceData {
    let score = 0.35 * output_trend + 0.35 * change_velocity + 0.30 * similarity_trend;

    let (recommendation, estimated) = if score >= 0.90 {
        ("stop", "0")
    } else if score >= 0.75 {
        ("almost", "1-2")
    } else if score >= 0.50 {
        ("continue", "3-5")
    } else {
        ("early", "5+")
    };

    ConvergenceData {
        score: Some(score),
        output_trend: Some(output_trend),
        change_velocity: Some(change_velocity),
        similarity_trend: Some(similarity_trend),
        estimated_remaining_rounds: Some(estimated.to_string()),
        recommendation: Some(recommendation.to_string()),
    }
}

pub fn null_convergence() -> ConvergenceData {
    ConvergenceData {
        score: None,
        output_trend: None,
        change_velocity: None,
        similarity_trend: None,
        estimated_remaining_rounds: None,
        recommendation: None,
    }
}

pub async fn read_stats(kv: &KvStore, workflow: &str) -> worker::Result<Option<Stats>> {
    kv_get::<Stats>(kv, &stats_key(workflow)).await
}

pub async fn write_stats(kv: &KvStore, workflow: &str, stats: &Stats) -> worker::Result<()> {
    kv_put(kv, &stats_key(workflow), stats).await
}

pub async fn update_stats_after_round(
    kv: &KvStore,
    workflow: &str,
    round_number: u32,
    content: &str,
    word_count: u32,
) -> worker::Result<ConvergenceData> {
    let mut stats = read_stats(kv, workflow).await?.unwrap_or_else(|| Stats {
        workflow: workflow.to_string(),
        total_rounds: 0,
        latest_score: None,
        latest_word_set: None,
        rounds: Vec::new(),
        updated_at: now_iso8601(),
    });

    let current_word_set = tokenize_to_word_set(content);

    let similarity = stats
        .latest_word_set
        .as_ref()
        .map(|prev_set| compute_similarity(prev_set, &current_word_set));

    let prev_words = stats.rounds.last().map(|e| e.words);
    let delta_words = prev_words.map(|pw| (pw as i64 - word_count as i64).unsigned_abs() as u32);

    let word_counts: Vec<u32> = stats
        .rounds
        .iter()
        .map(|e| e.words)
        .chain(std::iter::once(word_count))
        .collect();

    let output_trend = compute_output_trend(&word_counts);
    let change_velocity = compute_change_velocity(&word_counts);
    let similarity_trend = similarity.unwrap_or(0.0);

    let convergence = if word_counts.len() < 2 {
        null_convergence()
    } else {
        compute_convergence(output_trend, change_velocity, similarity_trend)
    };

    let entry = StatsRoundEntry {
        round: round_number,
        words: word_count,
        delta_words,
        similarity,
        score: convergence.score,
    };
    stats.rounds.push(entry);
    stats.total_rounds = stats.rounds.len() as u32;
    stats.latest_score = convergence.score;
    stats.latest_word_set = Some(current_word_set);
    stats.updated_at = now_iso8601();

    write_stats(kv, workflow, &stats).await?;

    Ok(convergence)
}

pub async fn update_meta_after_round(
    kv: &KvStore,
    workflow: &str,
    round_number: u32,
    convergence_score: Option<f64>,
) -> worker::Result<()> {
    let now = now_iso8601();
    let mut meta = kv_get::<Meta>(kv, &meta_key(workflow))
        .await?
        .unwrap_or_else(|| Meta {
            workflow: workflow.to_string(),
            round_count: 0,
            latest_round: None,
            latest_convergence: None,
            created_at: now.clone(),
            updated_at: now.clone(),
        });

    meta.round_count += 1;
    meta.latest_round = Some(round_number);
    meta.latest_convergence = convergence_score;
    meta.updated_at = now_iso8601();

    kv_put(kv, &meta_key(workflow), &meta).await
}

pub async fn rebuild_stats_from_rounds(
    kv: &KvStore,
    workflow: &str,
    rounds: &[(u32, String, u32)],
) -> worker::Result<Stats> {
    let mut stats = Stats {
        workflow: workflow.to_string(),
        total_rounds: 0,
        latest_score: None,
        latest_word_set: None,
        rounds: Vec::new(),
        updated_at: now_iso8601(),
    };

    for (round_number, content, word_count) in rounds {
        let current_word_set = tokenize_to_word_set(content);

        let similarity = stats
            .latest_word_set
            .as_ref()
            .map(|prev_set| compute_similarity(prev_set, &current_word_set));

        let prev_words = stats.rounds.last().map(|e| e.words);
        let delta_words =
            prev_words.map(|pw| (pw as i64 - *word_count as i64).unsigned_abs() as u32);

        let word_counts: Vec<u32> = stats
            .rounds
            .iter()
            .map(|e| e.words)
            .chain(std::iter::once(*word_count))
            .collect();

        let output_trend = compute_output_trend(&word_counts);
        let change_velocity = compute_change_velocity(&word_counts);
        let similarity_trend = similarity.unwrap_or(0.0);

        let convergence = if word_counts.len() < 2 {
            null_convergence()
        } else {
            compute_convergence(output_trend, change_velocity, similarity_trend)
        };

        stats.rounds.push(StatsRoundEntry {
            round: *round_number,
            words: *word_count,
            delta_words,
            similarity,
            score: convergence.score,
        });

        stats.latest_score = convergence.score;
        stats.latest_word_set = Some(current_word_set);
    }

    stats.total_rounds = stats.rounds.len() as u32;
    stats.updated_at = now_iso8601();

    write_stats(kv, workflow, &stats).await?;

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_output_trend_decreasing() {
        let counts = vec![4201, 3856, 3102, 2756];
        let trend = compute_output_trend(&counts);
        let expected = 1.0 - (2756.0 / 4201.0);
        assert!((trend - expected).abs() < 0.001, "trend={trend}, expected={expected}");
    }

    #[test]
    fn test_output_trend_same_counts() {
        let trend = compute_output_trend(&[1000, 1000, 1000]);
        assert_eq!(trend, 0.0);
    }

    #[test]
    fn test_output_trend_increasing() {
        let trend = compute_output_trend(&[1000, 2000, 3000]);
        assert_eq!(trend, 0.0);
    }

    #[test]
    fn test_output_trend_single() {
        assert_eq!(compute_output_trend(&[1000]), 0.0);
    }

    #[test]
    fn test_output_trend_empty() {
        assert_eq!(compute_output_trend(&[]), 0.0);
    }

    #[test]
    fn test_change_velocity_decreasing_deltas() {
        let counts = vec![4201, 3856, 3102, 2987, 2847, 2790, 2756];
        let velocity = compute_change_velocity(&counts);
        assert!(velocity > 0.9, "velocity={velocity}, expected > 0.9");
    }

    #[test]
    fn test_change_velocity_same_counts() {
        let velocity = compute_change_velocity(&[1000, 1000, 1000]);
        assert_eq!(velocity, 1.0);
    }

    #[test]
    fn test_change_velocity_single() {
        assert_eq!(compute_change_velocity(&[1000]), 0.0);
    }

    #[test]
    fn test_jaccard_identical() {
        let set: HashSet<String> = ["hello", "world"].iter().map(|s| s.to_string()).collect();
        assert_eq!(compute_similarity(&set, &set), 1.0);
    }

    #[test]
    fn test_jaccard_disjoint() {
        let a: HashSet<String> = ["hello", "world"].iter().map(|s| s.to_string()).collect();
        let b: HashSet<String> = ["foo", "bar"].iter().map(|s| s.to_string()).collect();
        assert_eq!(compute_similarity(&a, &b), 0.0);
    }

    #[test]
    fn test_jaccard_overlap() {
        let a: HashSet<String> = ["hello", "world", "foo"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let b: HashSet<String> = ["hello", "world", "bar"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let sim = compute_similarity(&a, &b);
        assert!((sim - 0.5).abs() < 0.01, "sim={sim}");
    }

    #[test]
    fn test_jaccard_empty() {
        let empty: HashSet<String> = HashSet::new();
        assert_eq!(compute_similarity(&empty, &empty), 1.0);
    }

    #[test]
    fn test_tokenize_strips_punctuation() {
        let set = tokenize_to_word_set("hello, world! foo's bar.");
        assert!(set.contains("hello"));
        assert!(set.contains("world"));
        assert!(set.contains("foo's"));
        assert!(set.contains("bar"));
    }

    #[test]
    fn test_tokenize_lowercase_dedup() {
        let set = tokenize_to_word_set("Hello hello HELLO");
        assert_eq!(set.len(), 1);
        assert!(set.contains("hello"));
    }

    #[test]
    fn test_tokenize_discards_pure_punctuation() {
        let set = tokenize_to_word_set("--- *** !!!");
        assert!(set.is_empty());
    }

    #[test]
    fn test_convergence_score_weights() {
        let c = compute_convergence(0.5, 0.5, 0.5);
        assert!((c.score.unwrap() - 0.5).abs() < 0.001);
        assert_eq!(c.recommendation.as_deref(), Some("continue"));
    }

    #[test]
    fn test_convergence_stop() {
        let c = compute_convergence(1.0, 1.0, 1.0);
        assert_eq!(c.recommendation.as_deref(), Some("stop"));
        assert_eq!(c.estimated_remaining_rounds.as_deref(), Some("0"));
    }

    #[test]
    fn test_convergence_early() {
        let c = compute_convergence(0.1, 0.1, 0.1);
        assert_eq!(c.recommendation.as_deref(), Some("early"));
    }

    #[test]
    fn test_null_convergence() {
        let c = null_convergence();
        assert!(c.score.is_none());
        assert!(c.recommendation.is_none());
    }

    #[test]
    fn test_convergence_almost() {
        let c = compute_convergence(0.8, 0.8, 0.8);
        assert_eq!(c.recommendation.as_deref(), Some("almost"));
        assert_eq!(c.estimated_remaining_rounds.as_deref(), Some("1-2"));
    }

    #[test]
    fn test_output_trend_prd_example() {
        // PRD section 9.2 example: max=4201, latest=2756 → 1.0-(2756/4201)=0.344
        let counts = vec![4201, 3856, 3102, 2987, 2847, 2790, 2756];
        let trend = compute_output_trend(&counts);
        let expected = 1.0 - (2756.0 / 4201.0);
        assert!((trend - expected).abs() < 0.001);
    }

    #[test]
    fn test_change_velocity_prd_example() {
        // PRD section 9.3: deltas=[345,754,115,140,57,34], max=754, latest=34
        // velocity = 1.0 - 34/754 = 0.955
        let counts = vec![4201, 3856, 3102, 2987, 2847, 2790, 2756];
        let velocity = compute_change_velocity(&counts);
        let expected = 1.0 - (34.0 / 754.0);
        assert!(
            (velocity - expected).abs() < 0.001,
            "velocity={velocity}, expected={expected}"
        );
    }

    #[test]
    fn test_end_to_end_convergence_seven_rounds() {
        let word_counts = vec![4201u32, 3856, 3102, 2987, 2847, 2790, 2756];
        let output_trend = compute_output_trend(&word_counts);
        let change_velocity = compute_change_velocity(&word_counts);
        let similarity = 0.797; // example from PRD sec 9.4
        let c = compute_convergence(output_trend, change_velocity, similarity);
        assert!(c.score.unwrap() > 0.5);
        assert!(
            c.recommendation.as_deref() == Some("continue")
                || c.recommendation.as_deref() == Some("almost")
        );
    }
}
