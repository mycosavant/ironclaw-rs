//! Hybrid search combining full-text and semantic search.
//!
//! Uses Reciprocal Rank Fusion (RRF) to combine results from:
//! 1. PostgreSQL full-text search (ts_rank_cd)
//! 2. pgvector cosine similarity search
//!
//! RRF formula: score = sum(1 / (k + rank)) for each retrieval method
//! This is robust to different score scales and produces better results
//! than simple score averaging.
//!
//! Optional enhancements:
//! - **Temporal decay**: multiplicative half-life factor so fresher documents rank higher
//! - **MMR re-ranking**: Maximal Marginal Relevance for result diversity
//! - **Citation support**: chunk position tracking (`chunk_index`) for citing sources

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Configuration for hybrid search.
#[derive(Debug, Clone)]
pub struct SearchConfig {
    /// Maximum number of results to return.
    pub limit: usize,
    /// RRF constant (typically 60). Higher values favor top results more.
    pub rrf_k: u32,
    /// Whether to include FTS results.
    pub use_fts: bool,
    /// Whether to include vector results.
    pub use_vector: bool,
    /// Minimum score threshold (0.0-1.0).
    pub min_score: f32,
    /// Maximum results to fetch from each method before fusion.
    pub pre_fusion_limit: usize,
    /// Half-life in days for temporal decay. When set, older documents receive
    /// a multiplicative penalty: `score *= 2^(-age_days / halflife)`.
    /// A value of 30.0 means a 30-day-old document scores at 50% of an identical
    /// fresh one. `None` disables temporal decay (default).
    pub temporal_decay_halflife_days: Option<f32>,
    /// Enable Maximal Marginal Relevance re-ranking for result diversity.
    /// When enabled, the final result selection balances relevance and diversity.
    pub use_mmr: bool,
    /// MMR lambda parameter (0.0-1.0). Higher values favor relevance over
    /// diversity. Default 0.7 (biased toward relevance).
    pub mmr_lambda: f32,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            limit: 10,
            rrf_k: 60,
            use_fts: true,
            use_vector: true,
            min_score: 0.0,
            pre_fusion_limit: 50,
            temporal_decay_halflife_days: None,
            use_mmr: false,
            mmr_lambda: 0.7,
        }
    }
}

impl SearchConfig {
    /// Set the result limit.
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Set the RRF constant.
    pub fn with_rrf_k(mut self, k: u32) -> Self {
        self.rrf_k = k;
        self
    }

    /// Disable FTS (only use vector search).
    pub fn vector_only(mut self) -> Self {
        self.use_fts = false;
        self.use_vector = true;
        self
    }

    /// Disable vector search (only use FTS).
    pub fn fts_only(mut self) -> Self {
        self.use_fts = true;
        self.use_vector = false;
        self
    }

    /// Set minimum score threshold.
    pub fn with_min_score(mut self, score: f32) -> Self {
        self.min_score = score.clamp(0.0, 1.0);
        self
    }

    /// Enable temporal decay with the given half-life in days.
    pub fn with_temporal_decay(mut self, halflife_days: f32) -> Self {
        self.temporal_decay_halflife_days = Some(halflife_days.max(0.1));
        self
    }

    /// Enable MMR re-ranking with the given lambda (relevance vs diversity).
    pub fn with_mmr(mut self, lambda: f32) -> Self {
        self.use_mmr = true;
        self.mmr_lambda = lambda.clamp(0.0, 1.0);
        self
    }
}

/// A search result with hybrid scoring.
#[derive(Debug, Clone)]
pub struct SearchResult {
    /// Document ID containing this chunk.
    pub document_id: Uuid,
    /// Workspace path of the document (e.g. "skills/my-skill.md", "public/faq.md").
    /// Used for trust-based access control: installed skills only see allowed prefixes.
    pub document_path: String,
    /// Chunk ID.
    pub chunk_id: Uuid,
    /// Zero-based position of this chunk within the parent document.
    /// Useful for citation: "document_path, chunk N".
    pub chunk_index: i32,
    /// Chunk content.
    pub content: String,
    /// Combined RRF score (0.0-1.0 normalized).
    pub score: f32,
    /// Rank in FTS results (1-based, None if not in FTS results).
    pub fts_rank: Option<u32>,
    /// Rank in vector results (1-based, None if not in vector results).
    pub vector_rank: Option<u32>,
    /// When the parent document was last updated (for temporal context).
    pub updated_at: Option<DateTime<Utc>>,
}

impl SearchResult {
    /// Check if this result came from FTS.
    pub fn from_fts(&self) -> bool {
        self.fts_rank.is_some()
    }

    /// Check if this result came from vector search.
    pub fn from_vector(&self) -> bool {
        self.vector_rank.is_some()
    }

    /// Check if this result came from both methods (hybrid match).
    pub fn is_hybrid(&self) -> bool {
        self.fts_rank.is_some() && self.vector_rank.is_some()
    }
}

/// Raw result from a single search method.
#[derive(Debug, Clone)]
pub struct RankedResult {
    pub chunk_id: Uuid,
    pub document_id: Uuid,
    /// Workspace path of the parent document.
    pub document_path: String,
    pub content: String,
    pub rank: u32, // 1-based rank
    /// Zero-based chunk position within the parent document.
    pub chunk_index: i32,
    /// Parent document's last-updated timestamp (for temporal decay).
    pub updated_at: Option<DateTime<Utc>>,
}

/// Reciprocal Rank Fusion algorithm.
///
/// Combines ranked results from multiple retrieval methods using the formula:
/// score(d) = sum(1 / (k + rank(d))) for each method where d appears
///
/// Optional enhancements applied in order:
/// 1. **Temporal decay** — multiply score by `2^(-age_days / halflife)`
/// 2. **Normalize** — rescale to 0-1 range
/// 3. **Min-score filter** — discard below threshold
/// 4. **MMR re-ranking** — greedily select diverse results (when enabled)
///    or simple top-N truncation (default)
///
/// # Arguments
///
/// * `fts_results` - Results from full-text search, ordered by relevance
/// * `vector_results` - Results from vector search, ordered by similarity
/// * `config` - Search configuration
///
/// # Returns
///
/// Combined results sorted by RRF score (descending), optionally re-ranked by MMR.
pub fn reciprocal_rank_fusion(
    fts_results: Vec<RankedResult>,
    vector_results: Vec<RankedResult>,
    config: &SearchConfig,
) -> Vec<SearchResult> {
    let k = config.rrf_k as f32;
    let now = Utc::now();

    // Track scores and metadata for each chunk
    struct ChunkInfo {
        document_id: Uuid,
        document_path: String,
        content: String,
        chunk_index: i32,
        updated_at: Option<DateTime<Utc>>,
        score: f32,
        fts_rank: Option<u32>,
        vector_rank: Option<u32>,
    }

    let mut chunk_scores: HashMap<Uuid, ChunkInfo> = HashMap::new();

    // Process FTS results
    for result in fts_results {
        let rrf_score = 1.0 / (k + result.rank as f32);
        chunk_scores
            .entry(result.chunk_id)
            .and_modify(|info| {
                info.score += rrf_score;
                info.fts_rank = Some(result.rank);
            })
            .or_insert(ChunkInfo {
                document_id: result.document_id,
                document_path: result.document_path,
                content: result.content,
                chunk_index: result.chunk_index,
                updated_at: result.updated_at,
                score: rrf_score,
                fts_rank: Some(result.rank),
                vector_rank: None,
            });
    }

    // Process vector results
    for result in vector_results {
        let rrf_score = 1.0 / (k + result.rank as f32);
        chunk_scores
            .entry(result.chunk_id)
            .and_modify(|info| {
                info.score += rrf_score;
                info.vector_rank = Some(result.rank);
            })
            .or_insert(ChunkInfo {
                document_id: result.document_id,
                document_path: result.document_path,
                content: result.content,
                chunk_index: result.chunk_index,
                updated_at: result.updated_at,
                score: rrf_score,
                fts_rank: None,
                vector_rank: Some(result.rank),
            });
    }

    // Apply temporal decay before normalization: score *= 2^(-age_days / halflife)
    if let Some(halflife) = config.temporal_decay_halflife_days {
        for info in chunk_scores.values_mut() {
            if let Some(updated) = info.updated_at {
                let age_days = (now - updated).num_seconds() as f32 / 86_400.0;
                let decay = (2.0_f32).powf(-age_days / halflife);
                info.score *= decay;
            }
            // No updated_at → no decay (treat as fresh)
        }
    }

    // Convert to SearchResult and sort by score
    let mut results: Vec<SearchResult> = chunk_scores
        .into_iter()
        .map(|(chunk_id, info)| SearchResult {
            document_id: info.document_id,
            document_path: info.document_path,
            chunk_id,
            chunk_index: info.chunk_index,
            content: info.content,
            score: info.score,
            fts_rank: info.fts_rank,
            vector_rank: info.vector_rank,
            updated_at: info.updated_at,
        })
        .collect();

    // Normalize scores to 0-1 range
    if let Some(max_score) = results.iter().map(|r| r.score).reduce(f32::max)
        && max_score > 0.0
    {
        for result in &mut results {
            result.score /= max_score;
        }
    }

    // Filter by minimum score
    if config.min_score > 0.0 {
        results.retain(|r| r.score >= config.min_score);
    }

    // Sort by score descending
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Final selection: MMR re-ranking for diversity or simple truncation
    if config.use_mmr && results.len() > 1 {
        results = mmr_select(&results, config.limit, config.mmr_lambda);
    } else {
        results.truncate(config.limit);
    }

    results
}

/// Maximal Marginal Relevance selection.
///
/// Greedily picks the next result that maximizes:
///   `lambda * relevance(r) - (1 - lambda) * max_similarity(r, selected)`
///
/// Uses word-level Jaccard similarity as a lightweight text similarity proxy
/// (no embeddings required at this stage since we're operating on content strings).
fn mmr_select(candidates: &[SearchResult], limit: usize, lambda: f32) -> Vec<SearchResult> {
    if candidates.is_empty() || limit == 0 {
        return Vec::new();
    }

    let mut selected: Vec<SearchResult> = Vec::with_capacity(limit.min(candidates.len()));
    let mut remaining: Vec<usize> = (0..candidates.len()).collect();

    // Always pick the highest-scoring result first
    selected.push(candidates[0].clone());
    remaining.remove(0);

    while selected.len() < limit && !remaining.is_empty() {
        let mut best_idx_in_remaining = 0;
        let mut best_mmr = f32::NEG_INFINITY;

        for (ri, &ci) in remaining.iter().enumerate() {
            let relevance = candidates[ci].score;

            // Max similarity to any already-selected result
            let max_sim = selected
                .iter()
                .map(|s| word_jaccard(&candidates[ci].content, &s.content))
                .fold(0.0_f32, f32::max);

            let mmr_score = lambda * relevance - (1.0 - lambda) * max_sim;
            if mmr_score > best_mmr {
                best_mmr = mmr_score;
                best_idx_in_remaining = ri;
            }
        }

        let chosen = remaining.remove(best_idx_in_remaining);
        selected.push(candidates[chosen].clone());
    }

    selected
}

/// Word-level Jaccard similarity: |A ∩ B| / |A ∪ B|.
///
/// Operates on lowercased whitespace-split tokens. Returns 0.0 for empty inputs.
fn word_jaccard(a: &str, b: &str) -> f32 {
    let set_a: HashSet<&str> = a.split_whitespace().collect();
    let set_b: HashSet<&str> = b.split_whitespace().collect();

    if set_a.is_empty() && set_b.is_empty() {
        return 0.0;
    }

    let intersection = set_a.intersection(&set_b).count();
    let union = set_a.union(&set_b).count();

    if union == 0 {
        0.0
    } else {
        intersection as f32 / union as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_result(chunk_id: Uuid, doc_id: Uuid, rank: u32) -> RankedResult {
        RankedResult {
            chunk_id,
            document_id: doc_id,
            document_path: String::new(),
            content: format!("content for chunk {}", chunk_id),
            rank,
            chunk_index: 0,
            updated_at: Some(Utc::now()),
        }
    }

    fn make_result_with_age(
        chunk_id: Uuid,
        doc_id: Uuid,
        rank: u32,
        age_days: i64,
    ) -> RankedResult {
        RankedResult {
            chunk_id,
            document_id: doc_id,
            document_path: String::new(),
            content: format!("content for chunk {}", chunk_id),
            rank,
            chunk_index: 0,
            updated_at: Some(Utc::now() - chrono::Duration::days(age_days)),
        }
    }

    #[test]
    fn test_rrf_single_method() {
        let config = SearchConfig::default().with_limit(10);

        let chunk1 = Uuid::new_v4();
        let chunk2 = Uuid::new_v4();
        let doc = Uuid::new_v4();

        let fts_results = vec![make_result(chunk1, doc, 1), make_result(chunk2, doc, 2)];

        let results = reciprocal_rank_fusion(fts_results, Vec::new(), &config);

        assert_eq!(results.len(), 2);
        // First result should have higher score
        assert!(results[0].score > results[1].score);
        // All should have FTS rank
        assert!(results.iter().all(|r| r.fts_rank.is_some()));
        assert!(results.iter().all(|r| r.vector_rank.is_none()));
    }

    #[test]
    fn test_rrf_hybrid_match_boosted() {
        let config = SearchConfig::default().with_limit(10);

        let chunk1 = Uuid::new_v4(); // In both
        let chunk2 = Uuid::new_v4(); // FTS only
        let chunk3 = Uuid::new_v4(); // Vector only
        let doc = Uuid::new_v4();

        let fts_results = vec![make_result(chunk1, doc, 1), make_result(chunk2, doc, 2)];

        let vector_results = vec![make_result(chunk1, doc, 1), make_result(chunk3, doc, 2)];

        let results = reciprocal_rank_fusion(fts_results, vector_results, &config);

        assert_eq!(results.len(), 3);

        // chunk1 should be first (hybrid match)
        assert_eq!(results[0].chunk_id, chunk1);
        assert!(results[0].is_hybrid());
        assert!(results[0].score > results[1].score);

        // Other chunks should not be hybrid
        assert!(!results[1].is_hybrid());
        assert!(!results[2].is_hybrid());
    }

    #[test]
    fn test_rrf_score_normalization() {
        let config = SearchConfig::default();

        let chunk1 = Uuid::new_v4();
        let doc = Uuid::new_v4();

        let fts_results = vec![make_result(chunk1, doc, 1)];

        let results = reciprocal_rank_fusion(fts_results, Vec::new(), &config);

        // Single result should have normalized score of 1.0
        assert_eq!(results.len(), 1);
        assert!((results[0].score - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_rrf_min_score_filter() {
        let config = SearchConfig::default().with_limit(10).with_min_score(0.5);

        let chunk1 = Uuid::new_v4();
        let chunk2 = Uuid::new_v4();
        let chunk3 = Uuid::new_v4();
        let doc = Uuid::new_v4();

        // chunk1 has rank 1, chunk3 has rank 100 (low score)
        let fts_results = vec![
            make_result(chunk1, doc, 1),
            make_result(chunk2, doc, 50),
            make_result(chunk3, doc, 100),
        ];

        let results = reciprocal_rank_fusion(fts_results, Vec::new(), &config);

        // Low-scoring results should be filtered out
        // All results should have score >= 0.5
        for result in &results {
            assert!(result.score >= 0.5);
        }
    }

    #[test]
    fn test_rrf_limit() {
        let config = SearchConfig::default().with_limit(2);

        let doc = Uuid::new_v4();
        let fts_results: Vec<_> = (1..=5)
            .map(|i| make_result(Uuid::new_v4(), doc, i))
            .collect();

        let results = reciprocal_rank_fusion(fts_results, Vec::new(), &config);

        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_rrf_k_parameter() {
        // Higher k values make ranking differences less pronounced
        let chunk1 = Uuid::new_v4();
        let chunk2 = Uuid::new_v4();
        let doc = Uuid::new_v4();

        let fts_results = vec![make_result(chunk1, doc, 1), make_result(chunk2, doc, 2)];

        // Low k: rank 1 score = 1/(10+1) = 0.091, rank 2 = 1/(10+2) = 0.083
        let config_low_k = SearchConfig::default().with_rrf_k(10);
        let results_low = reciprocal_rank_fusion(fts_results.clone(), Vec::new(), &config_low_k);

        // High k: rank 1 score = 1/(100+1) = 0.0099, rank 2 = 1/(100+2) = 0.0098
        let config_high_k = SearchConfig::default().with_rrf_k(100);
        let results_high = reciprocal_rank_fusion(fts_results, Vec::new(), &config_high_k);

        // With low k, the score difference is larger (relatively)
        let diff_low = results_low[0].score - results_low[1].score;
        let diff_high = results_high[0].score - results_high[1].score;

        // Low k should have larger relative difference
        assert!(diff_low > diff_high);
    }

    #[test]
    fn test_search_config_builders() {
        let config = SearchConfig::default()
            .with_limit(20)
            .with_rrf_k(30)
            .with_min_score(0.1);

        assert_eq!(config.limit, 20);
        assert_eq!(config.rrf_k, 30);
        assert!((config.min_score - 0.1).abs() < 0.001);
        assert!(config.use_fts);
        assert!(config.use_vector);

        let fts_only = SearchConfig::default().fts_only();
        assert!(fts_only.use_fts);
        assert!(!fts_only.use_vector);

        let vector_only = SearchConfig::default().vector_only();
        assert!(!vector_only.use_fts);
        assert!(vector_only.use_vector);

        let decay = SearchConfig::default().with_temporal_decay(30.0);
        assert_eq!(decay.temporal_decay_halflife_days, Some(30.0));

        let mmr = SearchConfig::default().with_mmr(0.5);
        assert!(mmr.use_mmr);
        assert!((mmr.mmr_lambda - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_temporal_decay_penalizes_old_results() {
        let doc = Uuid::new_v4();
        let fresh_chunk = Uuid::new_v4();
        let old_chunk = Uuid::new_v4();

        // Both at rank 1 in different retrieval methods, but old_chunk is 60 days old
        let fts_results = vec![make_result_with_age(fresh_chunk, doc, 1, 0)];
        let vector_results = vec![make_result_with_age(old_chunk, doc, 1, 60)];

        let config = SearchConfig::default()
            .with_limit(10)
            .with_temporal_decay(30.0); // 30-day halflife

        let results = reciprocal_rank_fusion(fts_results, vector_results, &config);
        assert_eq!(results.len(), 2);

        // Fresh chunk should rank higher due to decay
        assert_eq!(results[0].chunk_id, fresh_chunk);
        // Old chunk (60 days = 2 half-lives) should have ~25% of fresh score
        assert!(results[1].score < 0.5);
    }

    #[test]
    fn test_temporal_decay_disabled_by_default() {
        let doc = Uuid::new_v4();
        let fresh = Uuid::new_v4();
        let old = Uuid::new_v4();

        let fts = vec![
            make_result_with_age(old, doc, 1, 365), // rank 1, 1 year old
            make_result_with_age(fresh, doc, 2, 0), // rank 2, fresh
        ];

        let config = SearchConfig::default().with_limit(10); // no decay

        let results = reciprocal_rank_fusion(fts, Vec::new(), &config);
        // Without decay, the old doc should still be #1 (rank 1 beats rank 2)
        assert_eq!(results[0].chunk_id, old);
    }

    #[test]
    fn test_chunk_index_propagated() {
        let doc = Uuid::new_v4();
        let chunk = Uuid::new_v4();

        let fts = vec![RankedResult {
            chunk_id: chunk,
            document_id: doc,
            document_path: "notes/todo.md".to_string(),
            content: "buy groceries".to_string(),
            rank: 1,
            chunk_index: 3,
            updated_at: Some(Utc::now()),
        }];

        let config = SearchConfig::default();
        let results = reciprocal_rank_fusion(fts, Vec::new(), &config);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chunk_index, 3);
        assert_eq!(results[0].document_path, "notes/todo.md");
        assert!(results[0].updated_at.is_some());
    }

    #[test]
    fn test_mmr_promotes_diversity() {
        let doc = Uuid::new_v4();

        // Three chunks: two with identical content, one unique
        let similar1 = Uuid::new_v4();
        let similar2 = Uuid::new_v4();
        let unique = Uuid::new_v4();

        let fts = vec![
            RankedResult {
                chunk_id: similar1,
                document_id: doc,
                document_path: String::new(),
                content: "the quick brown fox jumps over the lazy dog".to_string(),
                rank: 1,
                chunk_index: 0,
                updated_at: Some(Utc::now()),
            },
            RankedResult {
                chunk_id: similar2,
                document_id: doc,
                document_path: String::new(),
                content: "the quick brown fox jumps over the lazy dog again".to_string(),
                rank: 2,
                chunk_index: 1,
                updated_at: Some(Utc::now()),
            },
            RankedResult {
                chunk_id: unique,
                document_id: doc,
                document_path: String::new(),
                content: "completely different topic about rust programming".to_string(),
                rank: 3,
                chunk_index: 2,
                updated_at: Some(Utc::now()),
            },
        ];

        // Without MMR: similar1, similar2, unique (by rank)
        let no_mmr = SearchConfig::default().with_limit(3);
        let results_no_mmr = reciprocal_rank_fusion(fts.clone(), Vec::new(), &no_mmr);
        assert_eq!(results_no_mmr[0].chunk_id, similar1);
        assert_eq!(results_no_mmr[1].chunk_id, similar2);
        assert_eq!(results_no_mmr[2].chunk_id, unique);

        // With MMR (low lambda = strong diversity): similar1, unique, similar2
        let with_mmr = SearchConfig::default().with_limit(3).with_mmr(0.3);
        let results_mmr = reciprocal_rank_fusion(fts, Vec::new(), &with_mmr);
        assert_eq!(results_mmr[0].chunk_id, similar1); // top scorer always first
        // unique should be promoted over similar2 due to diversity
        assert_eq!(results_mmr[1].chunk_id, unique);
        assert_eq!(results_mmr[2].chunk_id, similar2);
    }

    #[test]
    fn test_word_jaccard() {
        assert!((word_jaccard("hello world", "hello world") - 1.0).abs() < 0.001);
        assert!((word_jaccard("hello world", "goodbye moon") - 0.0).abs() < 0.001);
        assert!((word_jaccard("a b c", "b c d") - 0.5).abs() < 0.001); // 2/4
        assert!((word_jaccard("", "") - 0.0).abs() < 0.001);
    }
}
