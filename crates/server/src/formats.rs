//! Output formats of the transcription API.

use serde::Serialize;
use uuid::Uuid;
use verbarelay_protocol::transcript::{Segment, count_words, join_segments};

/// Formats a request may ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Json,
    Text,
    Srt,
    Vtt,
}

impl OutputFormat {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "json" => Some(Self::Json),
            "text" | "txt" => Some(Self::Text),
            "srt" => Some(Self::Srt),
            "vtt" | "webvtt" => Some(Self::Vtt),
            _ => None,
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Self::Json => "application/json",
            Self::Text => "text/plain; charset=utf-8",
            Self::Srt => "application/x-subrip; charset=utf-8",
            Self::Vtt => "text/vtt; charset=utf-8",
        }
    }
}

/// The complete result of a transcription request.
#[derive(Debug, Clone, Serialize)]
pub struct FinalTranscript {
    pub id: Uuid,
    pub text: String,
    pub language: String,
    pub model: Option<String>,
    pub duration_ms: u64,
    pub words: usize,
    pub parts: usize,
    pub segments: Vec<Segment>,
    /// Informational messages, e.g. that a model override was rejected.
    pub notices: Vec<String>,
}

impl FinalTranscript {
    pub fn new(
        id: Uuid,
        segments: Vec<Segment>,
        language: String,
        model: Option<String>,
        duration_ms: u64,
        parts: usize,
        notices: Vec<String>,
    ) -> Self {
        let text = join_segments(&segments);
        let words = count_words(&text);
        Self {
            id,
            text,
            language,
            model,
            duration_ms,
            words,
            parts,
            segments,
            notices,
        }
    }

    /// Renders the transcript in the requested format.
    pub fn render(&self, format: OutputFormat) -> String {
        match format {
            OutputFormat::Json => serde_json::to_string_pretty(self).unwrap_or_default(),
            OutputFormat::Text => format!("{}\n", self.text),
            OutputFormat::Srt => to_srt(&self.segments),
            OutputFormat::Vtt => to_vtt(&self.segments),
        }
    }
}

fn timestamp(ms: u64, decimal: char) -> String {
    let h = ms / 3_600_000;
    let m = ms / 60_000 % 60;
    let s = ms / 1000 % 60;
    let f = ms % 1000;
    format!("{h:02}:{m:02}:{s:02}{decimal}{f:03}")
}

fn cues(segments: &[Segment]) -> impl Iterator<Item = &Segment> {
    segments.iter().filter(|s| !s.text.trim().is_empty())
}

/// SubRip subtitles.
pub fn to_srt(segments: &[Segment]) -> String {
    let mut out = String::new();
    for (i, s) in cues(segments).enumerate() {
        out.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            i + 1,
            timestamp(s.start_ms, ','),
            timestamp(s.end_ms, ','),
            s.text.trim()
        ));
    }
    out
}

/// WebVTT subtitles.
pub fn to_vtt(segments: &[Segment]) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for s in cues(segments) {
        out.push_str(&format!(
            "{} --> {}\n{}\n\n",
            timestamp(s.start_ms, '.'),
            timestamp(s.end_ms, '.'),
            s.text.trim()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segments() -> Vec<Segment> {
        vec![
            Segment {
                start_ms: 0,
                end_ms: 1500,
                text: " Hallo Welt.".into(),
            },
            Segment {
                start_ms: 3_723_004,
                end_ms: 3_725_000,
                text: "Zweite Zeile".into(),
            },
        ]
    }

    #[test]
    fn srt_and_vtt() {
        let srt = to_srt(&segments());
        assert!(srt.starts_with("1\n00:00:00,000 --> 00:00:01,500\nHallo Welt.\n\n2\n01:02:03,004"));
        let vtt = to_vtt(&segments());
        assert!(vtt.starts_with("WEBVTT\n\n00:00:00.000 --> 00:00:01.500\nHallo Welt."));
    }

    #[test]
    fn final_transcript_counts_words() {
        let t = FinalTranscript::new(Uuid::nil(), segments(), "de".into(), None, 4_000_000, 1, vec![]);
        assert_eq!(t.text, "Hallo Welt. Zweite Zeile");
        assert_eq!(t.words, 4);
        assert_eq!(OutputFormat::parse("SRT"), Some(OutputFormat::Srt));
        assert_eq!(OutputFormat::parse("pdf"), None);
    }
}
