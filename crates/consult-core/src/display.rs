//! Progress and summary styles, read from `display.json` on every call.
//!
//! Read per call, not at startup: the server may be one process shared by every
//! session on the machine, and a display change should not need its restart. Anything
//! missing or odd means the default, so a typo degrades the display instead of breaking
//! a consult.

use std::fmt;
use std::path::Path;

use serde_json::Value;

use crate::paths::display_path;
use crate::util::strip_bom;

/// How the progress line of a running call reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ProgressStyle {
    /// Elapsed, cost, tokens, finished count. The default.
    #[default]
    Full,
    /// Only who is being asked.
    Quiet,
    /// Finished count and tool calls.
    Count,
    /// One mark per reviewer.
    Marks,
    /// Finished count as a percentage (sends a total).
    Percent,
    /// The most recently active reviewer's latest step.
    Latest,
    /// Elapsed and cost so far.
    Ticker,
}

impl ProgressStyle {
    /// Every style, the default first.
    pub const ALL: [ProgressStyle; 7] = [
        Self::Full,
        Self::Quiet,
        Self::Count,
        Self::Marks,
        Self::Percent,
        Self::Latest,
        Self::Ticker,
    ];

    /// The name used in display.json and on the command line.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Quiet => "quiet",
            Self::Count => "count",
            Self::Marks => "marks",
            Self::Percent => "percent",
            Self::Latest => "latest",
            Self::Ticker => "ticker",
        }
    }

    /// The style with this exact name.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == name)
    }
}

impl fmt::Display for ProgressStyle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How the one-line summary under Claude's reply is drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SummaryStyle {
    /// Dim terminal text. The default.
    #[default]
    Dim,
    /// Markdown italics.
    Italic,
    /// A markdown quote.
    Quote,
    /// No summary; the display hook is not registered.
    Off,
}

impl SummaryStyle {
    /// Every style, the default first.
    pub const ALL: [SummaryStyle; 4] = [Self::Dim, Self::Italic, Self::Quote, Self::Off];

    /// The name used in display.json and on the command line.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dim => "dim",
            Self::Italic => "italic",
            Self::Quote => "quote",
            Self::Off => "off",
        }
    }

    /// The style with this exact name.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == name)
    }
}

impl fmt::Display for SummaryStyle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// display.json as a JSON object, or `None` when it is missing, unreadable or not an object.
pub fn read_display(install_dir: &Path) -> Option<serde_json::Map<String, Value>> {
    let bytes = std::fs::read(display_path(install_dir)).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    // A hand edit saved from PowerShell 5.1 or Notepad starts with a BOM.
    match serde_json::from_str::<Value>(strip_bom(&text)).ok()? {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

/// The progress style from display.json; anything missing or odd means the default.
pub fn load_progress_style(install_dir: &Path) -> ProgressStyle {
    read_display(install_dir)
        .and_then(|d| {
            d.get("progress")
                .and_then(Value::as_str)
                .and_then(ProgressStyle::parse)
        })
        .unwrap_or_default()
}

/// The summary style from display.json; anything missing or odd means the default.
pub fn load_summary_style(install_dir: &Path) -> SummaryStyle {
    read_display(install_dir)
        .and_then(|d| {
            d.get("summary")
                .and_then(Value::as_str)
                .and_then(SummaryStyle::parse)
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_means_full() {
        let tmp = tempfile::tempdir().expect("tmp");
        assert_eq!(load_progress_style(tmp.path()), ProgressStyle::Full);
        assert_eq!(load_summary_style(tmp.path()), SummaryStyle::Dim);
    }

    #[test]
    fn every_known_style_is_read() {
        let tmp = tempfile::tempdir().expect("tmp");
        for style in ProgressStyle::ALL {
            let text = format!(r#"{{"progress": "{style}", "summary": "quote"}}"#);
            std::fs::write(display_path(tmp.path()), text).expect("write");
            assert_eq!(load_progress_style(tmp.path()), style);
            assert_eq!(load_summary_style(tmp.path()), SummaryStyle::Quote);
        }
    }

    #[test]
    fn unknown_or_malformed_means_the_default() {
        let tmp = tempfile::tempdir().expect("tmp");
        for text in [
            r#"{"progress": "loud"}"#,
            r#"{"progress": 3}"#,
            r#"{"summary": "off"}"#,
            "not json",
            r#"["marks"]"#,
            "",
        ] {
            std::fs::write(display_path(tmp.path()), text).expect("write");
            assert_eq!(
                load_progress_style(tmp.path()),
                ProgressStyle::Full,
                "{text}"
            );
        }
    }

    #[test]
    fn byte_order_mark_is_tolerated() {
        let tmp = tempfile::tempdir().expect("tmp");
        std::fs::write(
            display_path(tmp.path()),
            "\u{feff}{\"progress\": \"count\", \"summary\": \"off\"}",
        )
        .expect("write");
        assert_eq!(load_progress_style(tmp.path()), ProgressStyle::Count);
        assert_eq!(load_summary_style(tmp.path()), SummaryStyle::Off);
    }
}
