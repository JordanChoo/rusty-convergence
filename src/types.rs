use std::collections::HashSet;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum IntegrationMode {
    #[default]
    None,
    Claude,
    Human,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    pub name: String,
    pub description: Option<String>,
    pub provider: String,
    pub model: String,
    pub system_prompt: Option<String>,
    pub provider_params: Option<serde_json::Value>,
    pub documents: std::collections::HashMap<String, String>,
    pub template: Option<String>,
    pub template_with_impl: Option<String>,
    pub impl_every_n: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Round {
    pub workflow: String,
    pub round: u32,
    pub status: RoundStatus,
    pub content: Option<String>,
    pub partial_content: Option<String>,
    pub metrics: Option<DocumentMetrics>,
    pub convergence: Option<ConvergenceData>,
    pub usage: Option<UsageStats>,
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_params: Option<serde_json::Value>,
    pub include_impl: bool,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub failed_at: Option<String>,
    pub duration_seconds: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RoundStatus {
    Running,
    Complete,
    Failed,
    Stale,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentMetrics {
    pub words: u32,
    pub lines: u32,
    pub characters: u32,
    pub headings: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConvergenceData {
    pub score: Option<f64>,
    pub output_trend: Option<f64>,
    pub change_velocity: Option<f64>,
    pub similarity_trend: Option<f64>,
    pub estimated_remaining_rounds: Option<String>,
    pub recommendation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageStats {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub workflow: String,
    pub round_count: u32,
    pub latest_round: Option<u32>,
    pub latest_convergence: Option<f64>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stats {
    pub workflow: String,
    pub total_rounds: u32,
    pub latest_score: Option<f64>,
    pub latest_word_set: Option<HashSet<String>>,
    pub rounds: Vec<StatsRoundEntry>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsRoundEntry {
    pub round: u32,
    pub words: u32,
    pub delta_words: Option<u32>,
    pub similarity: Option<f64>,
    pub score: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lock {
    pub round: u32,
    pub started_at: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunOverrides {
    pub include_impl: Option<bool>,
    pub skip_sequence_check: Option<bool>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub provider_params: Option<serde_json::Value>,
}
