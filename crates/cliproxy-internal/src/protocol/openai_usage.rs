//! Token counts from OpenAI-style usage objects on Chat Completions and Responses.

use super::json::Res;

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct TokenCounts {
    pub input: Option<i64>,
    pub output: Option<i64>,
    pub total: Option<i64>,
    pub cache_read: Option<i64>,
    pub cache_creation: Option<i64>,
    pub reasoning: Option<i64>,
}

impl TokenCounts {
    /// The first present alias wins, including a reported zero.
    pub(crate) fn parse(node: &Res<'_>) -> Option<Self> {
        if !node.is_object() {
            return None;
        }
        let count = |paths: &[&str]| {
            paths
                .iter()
                .map(|p| node.get(*p))
                .find(Res::exists)
                .map(|value| value.int())
        };
        let counts = Self {
            input: count(&["prompt_tokens", "input_tokens"]),
            output: count(&["completion_tokens", "output_tokens"]),
            total: count(&["total_tokens"]),
            cache_read: count(&[
                "prompt_tokens_details.cached_tokens",
                "input_tokens_details.cached_tokens",
            ]),
            cache_creation: count(&[
                "input_tokens_details.cache_creation_tokens",
                "input_tokens_details.cache_write_tokens",
                "prompt_tokens_details.cache_creation_tokens",
                "prompt_tokens_details.cache_write_tokens",
            ]),
            reasoning: count(&[
                "completion_tokens_details.reasoning_tokens",
                "output_tokens_details.reasoning_tokens",
            ]),
        };
        (counts.total.is_some() || counts.has_buckets()).then_some(counts)
    }

    pub(crate) fn has_buckets(&self) -> bool {
        self.buckets().iter().any(Option::is_some)
    }

    pub(crate) fn has_nonzero(&self) -> bool {
        self.buckets()
            .into_iter()
            .chain([self.total])
            .flatten()
            .any(|count| count != 0)
    }

    fn buckets(&self) -> [Option<i64>; 5] {
        [
            self.input,
            self.output,
            self.cache_read,
            self.cache_creation,
            self.reasoning,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::json;

    #[test]
    fn missing_zero_and_total_only_usage_are_distinct() {
        assert_eq!(TokenCounts::parse(&json::parse(br#"{"cost_in_usd_ticks":5}"#)), None);
        let zero = TokenCounts::parse(&json::parse(br#"{"total_tokens":0}"#)).unwrap();
        assert_eq!(zero.total, Some(0));
        assert!(!zero.has_buckets());
        assert!(!zero.has_nonzero());

        let counts = TokenCounts::parse(&json::parse(
            br#"{"input_tokens":9,"prompt_tokens":0,
            "input_tokens_details":{"cache_creation_tokens":0,"cache_write_tokens":5},
            "output_tokens_details":{"reasoning_tokens":2}}"#,
        ))
        .unwrap();
        assert_eq!(counts.input, Some(0));
        assert_eq!(counts.cache_creation, Some(0));
        assert_eq!(counts.reasoning, Some(2));
        assert!(counts.has_buckets());
        assert!(counts.has_nonzero());
    }
}
