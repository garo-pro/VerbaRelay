//! Transcription results.

use serde::{Deserialize, Serialize};

/// One timed piece of text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

/// The final result of a peer for one job (or one part of a split job).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptResult {
    /// Language code detected or forced, e.g. "de".
    pub language: String,
    pub segments: Vec<Segment>,
    /// Wall-clock time the peer spent transcribing, in milliseconds.
    pub processing_ms: u64,
    /// Time from receiving the last audio chunk to the first segment.
    pub first_segment_ms: Option<u64>,
}

impl TranscriptResult {
    /// The plain text of all segments.
    pub fn text(&self) -> String {
        join_segments(&self.segments)
    }
}

/// Joins segment texts into one string.
pub fn join_segments(segments: &[Segment]) -> String {
    segments
        .iter()
        .map(|s| s.text.trim())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Counts words the simple way: runs of non-whitespace characters.
pub fn count_words(text: &str) -> usize {
    text.split_whitespace().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_and_counts() {
        let segments = vec![
            Segment {
                start_ms: 0,
                end_ms: 1000,
                text: " Hello there.".into(),
            },
            Segment {
                start_ms: 1000,
                end_ms: 2000,
                text: "  ".into(),
            },
            Segment {
                start_ms: 2000,
                end_ms: 3000,
                text: "General Kenobi!".into(),
            },
        ];
        let text = join_segments(&segments);
        assert_eq!(text, "Hello there. General Kenobi!");
        assert_eq!(count_words(&text), 4);
    }
}
