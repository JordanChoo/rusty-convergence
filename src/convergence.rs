use std::collections::HashSet;

use worker::kv::KvStore;

use crate::error::now_iso8601;
use crate::storage::{kv_get, kv_put, meta_key, stats_key};
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

pub struct ComputedConvergence {
    pub convergence: ConvergenceData,
    pub updated_stats: Stats,
}

pub async fn compute_stats_update(
    kv: &KvStore,
    workflow: &str,
    round_number: u32,
    content: &str,
    word_count: u32,
) -> worker::Result<ComputedConvergence> {
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

    Ok(ComputedConvergence {
        convergence,
        updated_stats: stats,
    })
}

pub async fn commit_stats(kv: &KvStore, workflow: &str, stats: &Stats) -> worker::Result<()> {
    write_stats(kv, workflow, stats).await
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
        assert!(
            (trend - expected).abs() < 0.001,
            "trend={trend}, expected={expected}"
        );
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

    // --- Convergence accuracy integration tests (PRD criterion 7) ---

    fn make_round_content(base_words: &[&str], extra: &[&str]) -> String {
        let mut words: Vec<&str> = base_words.to_vec();
        words.extend_from_slice(extra);
        words.join(" ")
    }

    #[test]
    fn test_five_round_incremental_convergence_accuracy() {
        // Simulate 5 rounds with known content, compute convergence incrementally,
        // and verify against hand-calculated values.
        let base = vec![
            "architecture",
            "security",
            "protocol",
            "api",
            "design",
            "implementation",
            "rust",
            "cloudflare",
            "worker",
            "streaming",
        ];

        let round1_content = make_round_content(
            &base,
            &[
                "major",
                "rewrite",
                "fundamental",
                "overhaul",
                "restructure",
                "vulnerability",
                "critical",
                "flaw",
                "redesign",
                "breaking",
            ],
        );
        let round2_content = make_round_content(
            &base,
            &[
                "refinement",
                "improvement",
                "adjustment",
                "overhaul",
                "restructure",
                "optimization",
                "enhancement",
                "interface",
                "redesign",
                "update",
            ],
        );
        let round3_content = make_round_content(
            &base,
            &[
                "refinement",
                "improvement",
                "adjustment",
                "polish",
                "tweak",
                "optimization",
                "enhancement",
                "interface",
                "cleanup",
                "update",
            ],
        );
        let round4_content = make_round_content(
            &base,
            &[
                "refinement",
                "improvement",
                "adjustment",
                "polish",
                "tweak",
                "optimization",
                "enhancement",
                "interface",
                "cleanup",
                "minor",
            ],
        );
        let round5_content = make_round_content(
            &base,
            &[
                "refinement",
                "improvement",
                "adjustment",
                "polish",
                "tweak",
                "optimization",
                "enhancement",
                "interface",
                "cleanup",
                "minor",
            ],
        );

        let rounds = [
            &round1_content,
            &round2_content,
            &round3_content,
            &round4_content,
            &round5_content,
        ];

        let mut prev_word_set: Option<HashSet<String>> = None;
        let mut word_counts: Vec<u32> = Vec::new();
        let mut scores: Vec<Option<f64>> = Vec::new();

        for (i, content) in rounds.iter().enumerate() {
            let wc = content.split_whitespace().count() as u32;
            word_counts.push(wc);

            let current_set = tokenize_to_word_set(content);

            let similarity = prev_word_set
                .as_ref()
                .map(|prev| compute_similarity(prev, &current_set));

            if word_counts.len() < 2 {
                scores.push(None);
            } else {
                let ot = compute_output_trend(&word_counts);
                let cv = compute_change_velocity(&word_counts);
                let st = similarity.unwrap_or(0.0);
                let c = compute_convergence(ot, cv, st);
                scores.push(c.score);

                // Verify each signal is in [0, 1]
                assert!(ot >= 0.0 && ot <= 1.0, "Round {}: output_trend={ot}", i + 1);
                assert!(
                    cv >= 0.0 && cv <= 1.0,
                    "Round {}: change_velocity={cv}",
                    i + 1
                );
                assert!(st >= 0.0 && st <= 1.0, "Round {}: similarity={st}", i + 1);
            }

            prev_word_set = Some(current_set);
        }

        // Round 1: no score (null convergence)
        assert!(scores[0].is_none());

        // Scores should be monotonically non-decreasing (convergence improves)
        for i in 2..scores.len() {
            let prev = scores[i - 1].unwrap();
            let curr = scores[i].unwrap();
            assert!(
                curr >= prev - 0.01,
                "Score decreased at round {}: {prev} -> {curr}",
                i + 1
            );
        }

        // Final score should be high since rounds 4 and 5 are identical
        let final_score = scores[4].unwrap();
        assert!(
            final_score >= 0.5,
            "Final convergence score {final_score} should be >= 0.5"
        );
    }

    #[test]
    fn test_convergence_identical_rounds_converge_to_high() {
        let content = "the architecture uses streaming with cloudflare workers";
        let wc = content.split_whitespace().count() as u32;
        let word_set = tokenize_to_word_set(content);

        // All rounds identical → should converge strongly
        let word_counts = vec![wc; 5];
        let ot = compute_output_trend(&word_counts);
        let cv = compute_change_velocity(&word_counts);
        let sim = compute_similarity(&word_set, &word_set);

        assert_eq!(ot, 0.0); // no decrease when all same
        assert_eq!(cv, 1.0); // zero delta
        assert_eq!(sim, 1.0); // identical sets

        let c = compute_convergence(ot, cv, sim);
        // 0.35*0 + 0.35*1 + 0.30*1 = 0.65
        let expected = 0.35 * 0.0 + 0.35 * 1.0 + 0.30 * 1.0;
        assert!(
            (c.score.unwrap() - expected).abs() < 0.001,
            "score={}, expected={expected}",
            c.score.unwrap()
        );
        assert_eq!(c.recommendation.as_deref(), Some("continue"));
    }

    #[test]
    fn test_convergence_strongly_decreasing_high_similarity() {
        // Simulate shrinking output with high overlap → strong convergence
        let word_counts = vec![5000u32, 4000, 3200, 2700, 2400];
        let ot = compute_output_trend(&word_counts);
        let cv = compute_change_velocity(&word_counts);

        // output_trend: 1 - 2400/5000 = 0.52
        let expected_ot = 1.0 - (2400.0 / 5000.0);
        assert!((ot - expected_ot).abs() < 0.001);

        // deltas: [1000, 800, 500, 300], max=1000, latest=300
        // velocity: 1 - 300/1000 = 0.7
        let expected_cv = 1.0 - (300.0 / 1000.0);
        assert!((cv - expected_cv).abs() < 0.001);

        let sim = 0.85;
        let c = compute_convergence(ot, cv, sim);
        let expected_score = 0.35 * expected_ot + 0.35 * expected_cv + 0.30 * sim;
        assert!(
            (c.score.unwrap() - expected_score).abs() < 0.01,
            "score={}, expected={expected_score}",
            c.score.unwrap()
        );
    }

    #[test]
    fn test_convergence_weight_formula_exact() {
        // PRD section 9.1: score = 0.35*ot + 0.35*cv + 0.30*sim
        for (ot, cv, sim) in [
            (0.0, 0.0, 0.0),
            (1.0, 0.0, 0.0),
            (0.0, 1.0, 0.0),
            (0.0, 0.0, 1.0),
            (0.5, 0.5, 0.5),
            (0.3, 0.7, 0.9),
        ] {
            let c = compute_convergence(ot, cv, sim);
            let expected = 0.35 * ot + 0.35 * cv + 0.30 * sim;
            assert!(
                (c.score.unwrap() - expected).abs() < 0.0001,
                "ot={ot}, cv={cv}, sim={sim}: score={}, expected={expected}",
                c.score.unwrap()
            );
        }
    }

    #[test]
    fn test_convergence_recommendation_boundaries() {
        // Test exact boundaries from PRD section 9.5
        let c90 = compute_convergence(0.9, 0.9, 0.9);
        assert_eq!(c90.recommendation.as_deref(), Some("stop"));

        // Just below 0.90 → "almost"
        let c89 = compute_convergence(0.88, 0.88, 0.88);
        assert!(c89.score.unwrap() < 0.90);
        assert_eq!(c89.recommendation.as_deref(), Some("almost"));

        // 0.35*0.75 + 0.35*0.75 + 0.30*0.75 = 0.74999... (float rounding)
        // Falls just below 0.75 → "continue"
        let c75 = compute_convergence(0.75, 0.75, 0.75);
        assert_eq!(c75.recommendation.as_deref(), Some("continue"));

        // Slightly above threshold → "almost"
        let c76 = compute_convergence(0.76, 0.76, 0.76);
        assert_eq!(c76.recommendation.as_deref(), Some("almost"));

        // Just below 0.50 → "early"
        let c49 = compute_convergence(0.49, 0.49, 0.49);
        assert!(c49.score.unwrap() < 0.50);
        assert_eq!(c49.recommendation.as_deref(), Some("early"));
    }

    #[test]
    fn test_division_by_zero_edge_cases() {
        // All zeros
        assert_eq!(compute_output_trend(&[0, 0, 0]), 0.0);
        assert_eq!(compute_change_velocity(&[0, 0, 0]), 1.0);

        // Single zero
        assert_eq!(compute_output_trend(&[0]), 0.0);

        // Empty sets
        let empty: HashSet<String> = HashSet::new();
        let nonempty: HashSet<String> = ["a"].iter().map(|s| s.to_string()).collect();
        assert_eq!(compute_similarity(&empty, &nonempty), 0.0);
        assert_eq!(compute_similarity(&empty, &empty), 1.0);
    }
}
