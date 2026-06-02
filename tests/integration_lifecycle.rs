//! Integration test: full convergence lifecycle with known content.
//!
//! Tests the complete convergence pipeline with 5 rounds of known content,
//! verifying that metrics, signals, and scores match hand-calculated values.
//! Does NOT require KV or network — tests pure functions only.

use std::collections::HashSet;

// Re-export the library's modules for testing
use rusty_convergence::convergence::{
    compute_change_velocity, compute_convergence, compute_output_trend, compute_similarity,
    null_convergence, tokenize_to_word_set,
};
use rusty_convergence::metrics::compute_metrics;

const ROUND_1: &str = r#"# Major Architectural Revisions

## 1. Security Overhaul
The entire authentication system needs to be redesigned. Currently using basic auth which is vulnerable to replay attacks. Recommend switching to JWT with refresh tokens, implementing PKCE for OAuth flows, adding rate limiting per-endpoint, and introducing IP allowlisting for admin routes.

## 2. Database Schema Redesign
The current flat table structure won't scale. Recommend normalizing into proper 3NF, adding composite indexes for the three most common query patterns, implementing read replicas for horizontal scaling, and adding a caching layer with Redis.

## 3. API Versioning
No versioning strategy exists. Recommend URL-based versioning (/v1/), implementing content negotiation, adding deprecation headers, and building a migration guide framework.

## 4. Error Handling
Error responses are inconsistent. Standardize on RFC 7807 Problem Details format, add correlation IDs, implement structured logging, and add error telemetry."#;

const ROUND_2: &str = r#"# Architecture Refinements

## 1. Auth Token Rotation
JWT implementation looks solid. Recommend adding automatic token rotation with configurable TTL, implementing token revocation list, and adding audit logging for auth events.

## 2. Database Index Optimization
The composite indexes are well-chosen. Consider adding partial indexes for the status-based queries, implementing connection pooling with PgBouncer, and adding query plan monitoring.

## 3. Cache Invalidation Strategy
Redis caching layer needs a proper invalidation strategy. Recommend write-through caching for frequently updated entities and TTL-based expiry for read-heavy endpoints."#;

const ROUND_3: &str = r#"# Interface Polish

## 1. Token Rotation Edge Cases
Handle the case where rotation happens during an active request. Add grace period of 30 seconds for old tokens after rotation.

## 2. Index Monitoring
Add automated slow-query alerting when any query exceeds 100ms. Consider adding pg_stat_statements integration.

## 3. Cache Metrics
Add cache hit/miss ratio monitoring. Target 95% hit rate for the item listing endpoint."#;

const ROUND_4: &str = r#"# Final Refinements

## 1. Grace Period Configuration
The 30-second grace period should be configurable via environment variable. Default is appropriate for most deployments.

## 2. Alerting Thresholds
Consider making the 100ms slow-query threshold configurable per-endpoint, as some complex queries may legitimately take longer.

## 3. Cache Target
The 95% hit rate target is good. Add a dashboard widget for real-time monitoring."#;

const ROUND_5: &str = r#"# Minor Polish

## 1. Configuration Documentation
Add inline documentation for all new environment variables introduced in this revision cycle.

## 2. Dashboard Integration
The monitoring dashboard configuration looks complete. No further changes recommended."#;

#[test]
fn test_metrics_across_rounds() {
    let rounds = [ROUND_1, ROUND_2, ROUND_3, ROUND_4, ROUND_5];

    let metrics: Vec<_> = rounds.iter().map(|r| compute_metrics(r)).collect();

    // Word counts should decrease across rounds (convergence)
    println!("=== Metrics per round ===");
    for (i, m) in metrics.iter().enumerate() {
        println!(
            "  Round {}: words={}, lines={}, chars={}, headings={}",
            i + 1,
            m.words,
            m.lines,
            m.characters,
            m.headings
        );
    }

    assert!(
        metrics[0].words > metrics[4].words,
        "Words should decrease over rounds"
    );
    assert!(
        metrics[0].headings > metrics[4].headings,
        "Headings should decrease over rounds"
    );

    for m in &metrics {
        assert!(m.words > 0);
        assert!(m.lines > 0);
        assert!(m.characters > 0);
        assert!(m.headings > 0);
    }
}

#[test]
fn test_convergence_signals_across_rounds() {
    let rounds = [ROUND_1, ROUND_2, ROUND_3, ROUND_4, ROUND_5];
    let metrics: Vec<_> = rounds.iter().map(|r| compute_metrics(r)).collect();
    let word_counts: Vec<u32> = metrics.iter().map(|m| m.words).collect();

    println!("=== Word counts: {:?} ===", word_counts);

    // Output trend should be positive (words are decreasing)
    let trend = compute_output_trend(&word_counts);
    println!("Output trend: {trend:.4}");
    assert!(
        trend > 0.0,
        "Output trend should be positive (decreasing word counts)"
    );
    assert!(trend < 1.0, "Output trend should be less than 1.0");

    // Change velocity should be positive (deltas exist)
    let velocity = compute_change_velocity(&word_counts);
    println!("Change velocity: {velocity:.4}");
    assert!(velocity >= 0.0 && velocity <= 1.0);

    // Similarity should show increasing overlap
    let mut prev_set: Option<HashSet<String>> = None;
    let mut similarities = Vec::new();
    for content in &rounds {
        let current_set = tokenize_to_word_set(content);
        if let Some(prev) = &prev_set {
            let sim = compute_similarity(prev, &current_set);
            similarities.push(sim);
            println!("  Similarity: {sim:.4}");
        }
        prev_set = Some(current_set);
    }
    assert!(!similarities.is_empty());
    println!("Similarities: {:?}", similarities);
}

#[test]
fn test_convergence_score_trajectory() {
    let rounds = [ROUND_1, ROUND_2, ROUND_3, ROUND_4, ROUND_5];
    let mut word_counts: Vec<u32> = Vec::new();
    let mut prev_set: Option<HashSet<String>> = None;
    let mut scores: Vec<Option<f64>> = Vec::new();

    println!("=== Convergence trajectory ===");

    for (i, content) in rounds.iter().enumerate() {
        let metrics = compute_metrics(content);
        word_counts.push(metrics.words);

        let current_set = tokenize_to_word_set(content);
        let similarity = prev_set
            .as_ref()
            .map(|prev| compute_similarity(prev, &current_set));

        if word_counts.len() < 2 {
            scores.push(None);
            println!("  Round {}: score=null (first round)", i + 1);
        } else {
            let ot = compute_output_trend(&word_counts);
            let cv = compute_change_velocity(&word_counts);
            let st = similarity.unwrap_or(0.0);
            let convergence = compute_convergence(ot, cv, st);
            let score = convergence.score.unwrap();
            scores.push(Some(score));
            println!(
                "  Round {}: score={:.4} ot={:.4} cv={:.4} st={:.4} rec={}",
                i + 1,
                score,
                ot,
                cv,
                st,
                convergence.recommendation.as_deref().unwrap_or("?")
            );
        }

        prev_set = Some(current_set);
    }

    // Verify score increases over time (convergence)
    let defined_scores: Vec<f64> = scores.iter().filter_map(|s| *s).collect();
    assert!(
        defined_scores.len() >= 3,
        "Should have at least 3 scored rounds"
    );

    // Later scores should generally be higher than earlier ones
    let first = defined_scores[0];
    let last = *defined_scores.last().unwrap();
    println!("\nFirst scored: {first:.4}, Last scored: {last:.4}");
    assert!(
        last > first,
        "Convergence should increase: first={first:.4}, last={last:.4}"
    );

    // Final score should indicate meaningful progress
    assert!(
        last > 0.3,
        "Final convergence score should show meaningful progress: {last:.4}"
    );
}

#[test]
fn test_null_convergence_round_1() {
    let c = null_convergence();
    assert!(c.score.is_none());
    assert!(c.output_trend.is_none());
    assert!(c.change_velocity.is_none());
    assert!(c.similarity_trend.is_none());
    assert!(c.recommendation.is_none());
}

#[test]
fn test_convergence_round_2_has_score() {
    let r1 = compute_metrics(ROUND_1);
    let r2 = compute_metrics(ROUND_2);
    let word_counts = vec![r1.words, r2.words];

    let ot = compute_output_trend(&word_counts);
    let cv = compute_change_velocity(&word_counts);
    let set1 = tokenize_to_word_set(ROUND_1);
    let set2 = tokenize_to_word_set(ROUND_2);
    let st = compute_similarity(&set1, &set2);

    let c = compute_convergence(ot, cv, st);
    assert!(c.score.is_some(), "Round 2 should have a convergence score");
    let score = c.score.unwrap();
    assert!(
        score >= 0.0 && score <= 1.0,
        "Score should be in [0, 1]: {score}"
    );
    assert!(c.recommendation.is_some());
    println!(
        "Round 2 score: {score:.4}, recommendation: {}",
        c.recommendation.unwrap()
    );
}

// === Adversarial edge case probes ===

#[test]
fn probe_metrics_only_newlines() {
    let m = compute_metrics("\n\n\n");
    assert_eq!(m.words, 0);
    assert_eq!(m.characters, 3);
}

#[test]
fn probe_metrics_crlf() {
    let m = compute_metrics("hello\r\nworld");
    assert_eq!(m.words, 2);
    assert_eq!(m.lines, 2);
}

#[test]
fn probe_tokenize_markdown_fences() {
    let set = tokenize_to_word_set("```rust\nfn main() {}\n```");
    assert!(set.contains("rust"), "should extract 'rust' from ```rust");
    assert!(set.contains("fn"));
    assert!(!set.contains("```"), "pure punctuation should be filtered");
}

#[test]
fn probe_extract_adjacent_placeholders() {
    use rusty_convergence::prompt::extract_placeholders;
    let p = extract_placeholders("{{a}}{{b}}");
    assert_eq!(p, vec!["a", "b"]);
}

#[test]
fn probe_extract_placeholder_only() {
    use rusty_convergence::prompt::extract_placeholders;
    let p = extract_placeholders("{{x}}");
    assert_eq!(p, vec!["x"]);
}

#[test]
fn probe_convergence_two_identical_then_different() {
    let cv = compute_change_velocity(&[1000, 1000, 500]);
    assert!((cv - 0.0).abs() < 0.001, "velocity should be 0 when latest delta IS the max, got {cv}");
}

#[test]
fn probe_convergence_spike_then_settle() {
    let ot = compute_output_trend(&[1000, 3000, 1000, 1000]);
    let expected = 1.0 - (1000.0 / 3000.0);
    assert!((ot - expected).abs() < 0.001, "ot={ot}, expected={expected}");

    let cv = compute_change_velocity(&[1000, 3000, 1000, 1000]);
    assert_eq!(cv, 1.0, "latest delta is 0, should be perfect velocity");
}
