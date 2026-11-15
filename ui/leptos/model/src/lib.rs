use serde::{Deserialize, Serialize};

pub const INITIAL_CONFIG: &str = "# Disposable sandbox configuration\nrouting:\n  strategy: soonest-reset\n  retry:\n    request-retry: 3\nobservability:\n  usage:\n    enabled: true\nstreaming:\n  keepalive-seconds: 0\n";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigSnapshot {
    pub version: u64,
    pub yaml: String,
}

impl ConfigSnapshot {
    pub fn etag(&self) -> String {
        format!("\"sandbox-{}\"", self.version)
    }
}

impl Default for ConfigSnapshot {
    fn default() -> Self {
        Self {
            version: 1,
            yaml: INITIAL_CONFIG.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Completed,
    Limited,
    Failed,
}

impl Outcome {
    pub fn label(self) -> &'static str {
        match self {
            Self::Completed => "Completed",
            Self::Limited => "Limited",
            Self::Failed => "Failed",
        }
    }

    pub fn class(self) -> &'static str {
        match self {
            Self::Completed => "ok",
            Self::Limited => "warn",
            Self::Failed => "bad",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    pub id: u64,
    pub time: String,
    pub client: String,
    pub provider: String,
    pub model: String,
    pub outcome: Outcome,
    pub duration_ms: u32,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActivityPage {
    pub cursor: u64,
    pub records: Vec<Activity>,
    pub gap: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewedWrite {
    pub base: ConfigSnapshot,
    pub yaml: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WritePhase {
    Editing,
    Reviewing(ReviewedWrite),
    Saving(ReviewedWrite),
    Conflict {
        review: ReviewedWrite,
        latest: ConfigSnapshot,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Editor {
    pub base: ConfigSnapshot,
    pub text: String,
    pub history: Vec<String>,
    pub phase: WritePhase,
}

pub fn validate_yaml(text: &str) -> Result<(), String> {
    match serde_yaml_ng::from_str::<serde_yaml_ng::Value>(text) {
        Ok(serde_yaml_ng::Value::Mapping(map)) if !map.is_empty() => Ok(()),
        Ok(_) => Err("Use a non-empty YAML mapping.".into()),
        Err(error) => Err(error.to_string()),
    }
}

impl Editor {
    pub fn new(base: ConfigSnapshot) -> Self {
        Self {
            text: base.yaml.clone(),
            base,
            history: Vec::new(),
            phase: WritePhase::Editing,
        }
    }

    pub fn dirty(&self) -> bool {
        self.text != self.base.yaml
    }

    pub fn edit(&mut self, text: String) {
        if self.phase == WritePhase::Editing {
            self.text = text;
        }
    }

    pub fn review(&mut self) -> Result<(), String> {
        if self.phase != WritePhase::Editing {
            return Err("Return to editing before reviewing another change.".into());
        }
        validate_yaml(&self.text)?;
        if !self.dirty() {
            return Err("Make a change before reviewing.".into());
        }
        self.phase = WritePhase::Reviewing(ReviewedWrite {
            base: self.base.clone(),
            yaml: self.text.clone(),
        });
        Ok(())
    }

    pub fn begin_save(&mut self) -> Option<ReviewedWrite> {
        let WritePhase::Reviewing(review) = &self.phase else {
            return None;
        };
        let review = review.clone();
        self.phase = WritePhase::Saving(review.clone());
        Some(review)
    }

    pub fn saved(&mut self, snapshot: ConfigSnapshot) {
        if matches!(self.phase, WritePhase::Saving(_)) {
            *self = Self {
                history: self.history.clone(),
                ..Self::new(snapshot)
            };
        }
    }

    pub fn conflict(&mut self, latest: ConfigSnapshot) {
        if let WritePhase::Saving(review) = &self.phase {
            self.phase = WritePhase::Conflict {
                review: review.clone(),
                latest,
            };
        }
    }

    pub fn retry_after_error(&mut self) {
        if let WritePhase::Saving(review) = &self.phase {
            self.phase = WritePhase::Reviewing(review.clone());
        }
    }

    pub fn back_to_edit(&mut self) {
        if matches!(self.phase, WritePhase::Reviewing(_)) {
            self.phase = WritePhase::Editing;
        }
    }

    pub fn reconcile(&mut self, keep_draft: bool) {
        if let WritePhase::Conflict { latest, .. } = &self.phase {
            self.history.push(self.text.clone());
            if !keep_draft {
                self.text = latest.yaml.clone();
            }
            self.base = latest.clone();
            self.phase = WritePhase::Editing;
        }
    }

    pub fn restore_previous(&mut self) {
        if self.phase == WritePhase::Editing
            && let Some(previous) = self.history.pop()
        {
            self.text = previous;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_freezes_bytes_and_version_until_save() {
        let mut editor = Editor::new(ConfigSnapshot::default());
        let yaml = "# Keep this exact comment\nrouting: {strategy: round-robin}\n";
        editor.edit(yaml.into());
        editor.review().unwrap();
        editor.edit("routing: {}".into());
        let write = editor.begin_save().unwrap();
        assert_eq!(write.yaml, yaml);
        assert_eq!(write.base.etag(), "\"sandbox-1\"");
        assert!(editor.begin_save().is_none());
    }

    #[test]
    fn repeated_conflicts_retain_draft_and_previous_versions() {
        let mut editor = Editor::new(ConfigSnapshot::default());
        editor.edit("routing: {strategy: round-robin}\n".into());
        for version in [2, 3] {
            editor.review().unwrap();
            editor.begin_save().unwrap();
            editor.conflict(ConfigSnapshot {
                version,
                yaml: format!("other: {version}\n"),
            });
            editor.reconcile(true);
            assert_eq!(editor.text, "routing: {strategy: round-robin}\n");
            assert_eq!(editor.base.version, version);
        }
        assert_eq!(editor.history.len(), 2);
        editor.review().unwrap();
        assert_eq!(editor.begin_save().unwrap().base.version, 3);
    }

    #[test]
    fn accepting_server_version_keeps_a_recoverable_draft() {
        let mut editor = Editor::new(ConfigSnapshot::default());
        editor.edit("routing: {strategy: round-robin}\n".into());
        editor.review().unwrap();
        editor.begin_save().unwrap();
        editor.conflict(ConfigSnapshot {
            version: 2,
            yaml: "new: value\n".into(),
        });
        editor.reconcile(false);
        assert!(!editor.dirty());
        editor.restore_previous();
        assert!(editor.dirty());
        assert_eq!(editor.text, "routing: {strategy: round-robin}\n");
    }

    #[test]
    fn yaml_boundary_rejects_invalid_and_non_mapping_documents() {
        for invalid in ["routing: [", "[]", "null", "{}", "", "a: 1\na: 2\n"] {
            assert!(validate_yaml(invalid).is_err(), "{invalid}");
        }
    }
}
