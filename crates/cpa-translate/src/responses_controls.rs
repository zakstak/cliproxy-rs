//! The output limit and parallel-tool control a client states, as the registered
//! client -> Codex (Responses shape) pairs convert them. Those pairs serve the Grok and
//! Muse executors, whose Responses APIs take `max_output_tokens` and
//! `parallel_tool_calls`, so the conversion keeps both (Go v8.0.67 `01b24f1f`,
//! `99d1698f`). It is format conversion only: the Codex translated route
//! (`crate::codex_strict`) and native Codex keep their own rules.
//!
//! A value the Responses field cannot carry (an output limit that is not a number, a
//! parallel-tool flag that is not a boolean) is refused instead of being dropped or
//! replaced by the default, which Go does (docs/DIFFERENCES-FROM-GO.md).

use cpa_common::json::{self as gj, Kind};

use crate::Error;

fn refused(field: &str, want: &str) -> Error {
    Error(format!(
        "invalid request: `{field}` must be {want} or null; it cannot be sent as a Responses control"
    ))
}

/// The first of `paths` that holds a number, as raw JSON bytes. Absent and `null`
/// fields are skipped; any other value is refused.
pub(crate) fn output_limit(raw: &[u8], paths: &[&str]) -> Result<Option<Vec<u8>>, Error> {
    for path in paths {
        let value = gj::get(raw, path);
        match value.kind {
            Kind::Number => return Ok(Some(value.raw.into_owned())),
            Kind::Null => {}
            _ if !value.exists() => {}
            _ => return Err(refused(path, "a number")),
        }
    }
    Ok(None)
}

/// Refuses a `path` that is present, not `null` and not a boolean.
pub(crate) fn check_bool(raw: &[u8], path: &str) -> Result<(), Error> {
    let value = gj::get(raw, path);
    match value.kind {
        Kind::True | Kind::False | Kind::Null => Ok(()),
        _ if !value.exists() => Ok(()),
        _ => Err(refused(path, "a boolean")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_limit_takes_the_first_number_and_refuses_other_values() {
        let paths = ["max_completion_tokens", "max_tokens"];
        assert_eq!(output_limit(br#"{}"#, &paths).unwrap(), None);
        assert_eq!(
            output_limit(br#"{"max_completion_tokens":null,"max_tokens":17}"#, &paths).unwrap(),
            Some(b"17".to_vec())
        );
        assert_eq!(
            output_limit(br#"{"max_completion_tokens":9,"max_tokens":17}"#, &paths).unwrap(),
            Some(b"9".to_vec())
        );
        // The client's number keeps its bytes.
        assert_eq!(
            output_limit(br#"{"max_tokens":1.7e1}"#, &paths).unwrap(),
            Some(b"1.7e1".to_vec())
        );
        for bad in [
            r#"{"max_tokens":"17"}"#,
            r#"{"max_completion_tokens":{},"max_tokens":17}"#,
        ] {
            let err = output_limit(bad.as_bytes(), &paths).unwrap_err();
            assert!(err.0.contains("must be a number"), "{bad}: {err}");
        }
    }

    #[test]
    fn check_bool_accepts_booleans_null_and_absence() {
        for ok in [r#"{}"#, r#"{"p":null}"#, r#"{"p":true}"#, r#"{"p":false}"#] {
            check_bool(ok.as_bytes(), "p").unwrap();
        }
        for bad in [r#"{"p":"false"}"#, r#"{"p":0}"#, r#"{"p":[]}"#] {
            let err = check_bool(bad.as_bytes(), "p").unwrap_err();
            assert!(err.0.contains("`p` must be a boolean"), "{bad}: {err}");
        }
    }
}
