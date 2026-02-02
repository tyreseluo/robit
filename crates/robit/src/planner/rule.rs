use serde_json::{json, Value};

use crate::types::{ActionRequest, PlannerResponse};

pub struct RulePlanner;

impl RulePlanner {
    pub fn new() -> Self {
        Self
    }

    pub fn plan(&self, input: &str) -> PlannerResponse {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return PlannerResponse::Unknown {
                message: "empty input".to_string(),
            };
        }

        if let Some(request) = self.parse_explicit_action(trimmed) {
            return PlannerResponse::Action(request);
        }

        if self.matches_desktop_organize(trimmed) {
            return PlannerResponse::Action(ActionRequest {
                name: "fs.organize_directory".to_string(),
                params: json!({
                    "path": "~/Desktop",
                    "mode": "extension"
                }),
                raw_input: trimmed.to_string(),
            });
        }

        PlannerResponse::Unknown {
            message: "no rule matched".to_string(),
        }
    }

    fn parse_explicit_action(&self, input: &str) -> Option<ActionRequest> {
        let trimmed = input.trim();
        let rest = if let Some(rest) = trimmed.strip_prefix("action:") {
            rest.trim()
        } else if let Some(rest) = trimmed.strip_prefix("action ") {
            rest.trim()
        } else {
            return None;
        };

        if rest.is_empty() {
            return None;
        }

        let mut parts = rest.splitn(2, char::is_whitespace);
        let name = parts.next()?.trim();
        let params_raw = parts.next().unwrap_or("").trim();
        let params = if params_raw.is_empty() {
            json!({})
        } else if params_raw.starts_with('{') {
            serde_json::from_str(params_raw).unwrap_or_else(|_| json!({}))
        } else {
            parse_kv_params(params_raw)
        };

        Some(ActionRequest {
            name: name.to_string(),
            params,
            raw_input: trimmed.to_string(),
        })
    }

    fn matches_desktop_organize(&self, input: &str) -> bool {
        let lower = input.to_lowercase();
        input.contains("整理桌面") || (lower.contains("organize") && lower.contains("desktop"))
    }
}

fn parse_kv_params(input: &str) -> Value {
    let mut map = serde_json::Map::new();
    for token in input.split_whitespace() {
        let Some((key, value)) = token.split_once('=') else {
            continue;
        };
        map.insert(key.to_string(), parse_value(value));
    }
    Value::Object(map)
}

fn parse_value(raw: &str) -> Value {
    let trimmed = raw.trim_matches('"');
    if trimmed.eq_ignore_ascii_case("true") {
        return Value::Bool(true);
    }
    if trimmed.eq_ignore_ascii_case("false") {
        return Value::Bool(false);
    }
    if let Ok(int_val) = trimmed.parse::<i64>() {
        return Value::Number(int_val.into());
    }
    if let Ok(float_val) = trimmed.parse::<f64>() {
        if let Some(num) = serde_json::Number::from_f64(float_val) {
            return Value::Number(num);
        }
    }
    Value::String(trimmed.to_string())
}
