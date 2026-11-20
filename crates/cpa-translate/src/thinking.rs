//! Translator entry points into internal/thinking, served by `cpa_common::thinking`.

use cpa_common::thinking as ct;

pub use ct::{convert_level_to_budget, has_level, map_to_claude_effort};

/// The summary intent read from the client body (ExtractTranslatedSummaryConfig).
pub type Summary = ct::SummaryConfig;

pub fn extract_translated_summary(body: &[u8], source: &str, target: &str) -> Summary {
    ct::extract_translated_summary_config(body, source, target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_come_from_every_static_catalog() {
        // gpt-5.5 is not a Claude model; Go still finds its levels through the static
        // lookup, which is why Claude-bound translation uses adaptive thinking for it.
        let gpt = cpa_core::registry::lookup_model(None, " gpt-5.5 ", Some("claude"))
            .unwrap()
            .thinking
            .unwrap();
        assert!(!gpt.levels.is_empty());
        assert!(!has_level(&gpt.levels, "max"));
        assert_eq!(map_to_claude_effort("xhigh", false), Some("high"));
        assert_eq!(map_to_claude_effort("xhigh", true), Some("max"));
        assert_eq!(map_to_claude_effort("auto", true), Some("high"));
        assert_eq!(map_to_claude_effort("bogus", true), None);
    }

    #[test]
    fn level_budgets_are_case_insensitive() {
        assert_eq!(convert_level_to_budget("MAX"), Some(128000));
        assert_eq!(convert_level_to_budget(" high"), None);
    }
}
