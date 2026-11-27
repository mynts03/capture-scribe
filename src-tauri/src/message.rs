use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;

/// Language-neutral message at UI/process/persistence boundaries.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AppMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub params: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
}

impl AppMessage {
    pub fn known(code: &str, raw: impl Into<String>) -> Self {
        Self {
            code: Some(code.into()),
            raw: Some(raw.into()),
            ..Self::default()
        }
    }

    pub fn progress(code: &str) -> Self {
        Self {
            code: Some(code.into()),
            ..Self::default()
        }
    }

    pub fn param(mut self, name: &str, value: impl Into<Value>) -> Self {
        self.params.insert(name.into(), value.into());
        self
    }
}

impl From<String> for AppMessage {
    fn from(raw: String) -> Self {
        Self {
            raw: Some(raw),
            ..Self::default()
        }
    }
}
impl From<&str> for AppMessage {
    fn from(raw: &str) -> Self {
        raw.to_string().into()
    }
}
impl fmt::Display for AppMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}",
            self.raw
                .as_deref()
                .or(self.code.as_deref())
                .unwrap_or("Unknown error")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_json_preserves_specific_cause_params_and_raw() {
        let message: AppMessage = serde_json::from_str(
            r#"{"code":"transcription.empty","params":{"fileName":"音声.mp4"},"raw":"CLIError.emptyTranscript"}"#,
        ).unwrap();
        let serialized = serde_json::to_value(&message).unwrap();
        assert_eq!(serialized["code"], "transcription.empty");
        assert_eq!(serialized["params"]["fileName"], "音声.mp4");
        assert_eq!(serialized["raw"], "CLIError.emptyTranscript");
        let unknown: AppMessage = "AVFoundationErrorDomain Code=-11800".into();
        assert!(unknown.code.is_none());
        assert_eq!(unknown.raw.unwrap(), "AVFoundationErrorDomain Code=-11800");
    }
}
