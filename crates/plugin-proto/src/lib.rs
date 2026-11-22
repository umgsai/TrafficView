//! `trafficview-plugin-v1`：单行 JSON 子进程协议。
//!
//! 宿主每个采样周期向插件 stdin 写入一行 request（`t` 为 Unix 秒浮点，
//! `interval_ms` 为插件自身周期）；插件 stdout 回写一行 response
//! （`items` 描述可展示项，`values` 提供当前值）。stdout 只允许一帧，
//! 单帧上限 1 MiB，且必须是合法 UTF-8。
//!
//! 该 crate 只包含纯协议类型与校验，不启动进程、不依赖宿主状态。

use std::collections::{BTreeMap, HashSet};
use std::fmt;

use serde::{Deserialize, Serialize};

/// 单帧最大字节数（1 MiB）。
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// 插件 id 与 item id 的最大长度。
pub const MAX_ID_LEN: usize = 64;

/// 宿主发往插件的请求。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginRequest {
    /// 当前 Unix 时间（秒，浮点）。
    pub t: f64,
    /// 插件声明的采样周期。
    pub interval_ms: u32,
}

/// 插件返回的单个可展示项。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginItem {
    pub id: String,
    pub name: String,
    pub unit: String,
    pub format: String,
}

/// 插件返回的一帧数据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginResponse {
    pub items: Vec<PluginItem>,
    pub values: BTreeMap<String, String>,
}

/// 协议层错误；宿主据此隔离并汇报插件故障。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoError {
    FrameTooLarge(usize),
    EmptyFrame,
    NonUtf8,
    Json(String),
    EmptyItems,
    InvalidId(String),
    DuplicateId(String),
    UndeclaredValue(String),
    ValueHasNewline(String),
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FrameTooLarge(len) => {
                write!(f, "plugin frame is {len} bytes; limit is {MAX_FRAME_BYTES}")
            }
            Self::EmptyFrame => write!(f, "plugin wrote an empty frame"),
            Self::NonUtf8 => write!(f, "plugin frame is not valid UTF-8"),
            Self::Json(message) => write!(f, "plugin frame is not valid JSON: {message}"),
            Self::EmptyItems => write!(f, "plugin response declares no items"),
            Self::InvalidId(id) => write!(f, "invalid plugin/item id '{id}'"),
            Self::DuplicateId(id) => write!(f, "duplicate plugin item id '{id}'"),
            Self::UndeclaredValue(id) => {
                write!(f, "plugin value '{id}' is not declared in items")
            }
            Self::ValueHasNewline(id) => {
                write!(f, "plugin value '{id}' contains a newline")
            }
        }
    }
}

impl std::error::Error for ProtoError {}

/// id 仅允许 ASCII 字母、数字、`_` 与 `-`，长度 1..=64。
///
/// 禁止 `.` 是因为快照键使用 `<plugin-id>.<item-id>` 拼接。
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// 在 JSON 解析前检查帧长度，避免为超长输入分配内存。
pub fn validate_frame_length(len: usize) -> Result<(), ProtoError> {
    if len > MAX_FRAME_BYTES {
        return Err(ProtoError::FrameTooLarge(len));
    }
    if len == 0 {
        return Err(ProtoError::EmptyFrame);
    }
    Ok(())
}

/// 编码宿主请求为单行 JSON（含行尾换行）。
pub fn encode_request(request: &PluginRequest) -> String {
    let mut line = serde_json::to_string(request).expect("plugin request is serializable");
    line.push('\n');
    line
}

/// 解码宿主请求；主要用于协议测试与宿主自检。
pub fn decode_request(line: &[u8]) -> Result<PluginRequest, ProtoError> {
    validate_frame_length(line.len())?;
    let text = std::str::from_utf8(line).map_err(|_| ProtoError::NonUtf8)?;
    serde_json::from_str(text.trim_end_matches(['\r', '\n']))
        .map_err(|error| ProtoError::Json(error.to_string()))
}

/// 解码插件响应；超长帧在 UTF-8 与 JSON 解析前即被拒绝。
pub fn decode_response(line: &[u8]) -> Result<PluginResponse, ProtoError> {
    validate_frame_length(line.len())?;
    let text = std::str::from_utf8(line).map_err(|_| ProtoError::NonUtf8)?;
    serde_json::from_str(text.trim_end_matches(['\r', '\n']))
        .map_err(|error| ProtoError::Json(error.to_string()))
}

/// 解码并校验响应，返回带 `<plugin-id>.` 前缀的快照键值。
pub fn decode_response_for(
    line: &[u8],
    plugin_id: &str,
) -> Result<BTreeMap<String, String>, ProtoError> {
    decode_response(line)?.validated_values(plugin_id)
}

impl PluginResponse {
    /// 校验 items 结构；值集合可为空（表示本周期暂无读数）。
    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.items.is_empty() {
            return Err(ProtoError::EmptyItems);
        }
        let mut seen = HashSet::new();
        for item in &self.items {
            if !valid_id(&item.id) {
                return Err(ProtoError::InvalidId(item.id.clone()));
            }
            if !seen.insert(item.id.as_str()) {
                return Err(ProtoError::DuplicateId(item.id.clone()));
            }
        }
        Ok(())
    }

    /// 校验后生成快照键：`<plugin-id>.<item-id>`。
    pub fn validated_values(
        &self,
        plugin_id: &str,
    ) -> Result<BTreeMap<String, String>, ProtoError> {
        self.validate()?;
        if !valid_id(plugin_id) {
            return Err(ProtoError::InvalidId(plugin_id.to_owned()));
        }
        let declared: HashSet<&str> = self.items.iter().map(|item| item.id.as_str()).collect();
        let mut values = BTreeMap::new();
        for (id, value) in &self.values {
            if !declared.contains(id.as_str()) {
                return Err(ProtoError::UndeclaredValue(id.clone()));
            }
            if value.contains('\n') || value.contains('\r') {
                return Err(ProtoError::ValueHasNewline(id.clone()));
            }
            values.insert(format!("{plugin_id}.{id}"), value.clone());
        }
        Ok(values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str) -> PluginItem {
        PluginItem {
            id: id.into(),
            name: "FAN".into(),
            unit: "rpm".into(),
            format: "{value} rpm".into(),
        }
    }

    #[test]
    fn plugin_values_become_first_class_snapshot_items() {
        let response = PluginResponse {
            items: vec![PluginItem {
                id: "fan_rpm".into(),
                name: "FAN".into(),
                unit: "rpm".into(),
                format: "{value} rpm".into(),
            }],
            values: BTreeMap::from([("fan_rpm".into(), "1420".into())]),
        };
        let values = response.validated_values("demo").unwrap();
        assert_eq!(values.get("demo.fan_rpm").map(String::as_str), Some("1420"));
    }

    #[test]
    fn response_over_one_mebibyte_is_rejected_before_json_parsing() {
        assert!(validate_frame_length(1_048_577).is_err());
        assert!(validate_frame_length(MAX_FRAME_BYTES).is_ok());
        assert!(validate_frame_length(0).is_err());
    }

    #[test]
    fn empty_items_are_rejected() {
        let response = PluginResponse {
            items: Vec::new(),
            values: BTreeMap::new(),
        };
        assert_eq!(response.validate(), Err(ProtoError::EmptyItems));
    }

    #[test]
    fn duplicate_and_invalid_item_ids_are_rejected() {
        let duplicate = PluginResponse {
            items: vec![item("fan"), item("fan")],
            values: BTreeMap::new(),
        };
        assert_eq!(
            duplicate.validate(),
            Err(ProtoError::DuplicateId("fan".into()))
        );
        for invalid in ["", "fan rpm", "fan.rpm", "风扇"] {
            let response = PluginResponse {
                items: vec![item(invalid)],
                values: BTreeMap::new(),
            };
            assert_eq!(
                response.validate(),
                Err(ProtoError::InvalidId(invalid.into()))
            );
        }
        assert!(!valid_id("demo.plugin"));
        assert!(!valid_id(&"a".repeat(MAX_ID_LEN + 1)));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let line = br#"{"items":[{"id":"fan","name":"FAN","unit":"rpm","format":"{value}","extra":1}],"values":{}}"#;
        assert!(matches!(decode_response(line), Err(ProtoError::Json(_))));
        let request = br#"{"t":1.0,"interval_ms":1000,"extra":true}"#;
        assert!(matches!(decode_request(request), Err(ProtoError::Json(_))));
    }

    #[test]
    fn non_utf8_and_empty_frames_are_rejected() {
        assert_eq!(decode_response(&[]), Err(ProtoError::EmptyFrame));
        assert_eq!(decode_response(&[0xff, 0xfe]), Err(ProtoError::NonUtf8));
    }

    #[test]
    fn newlines_in_values_are_rejected() {
        let response = PluginResponse {
            items: vec![item("fan")],
            values: BTreeMap::from([("fan".into(), "1420\n1500".into())]),
        };
        assert_eq!(
            response.validated_values("demo"),
            Err(ProtoError::ValueHasNewline("fan".into()))
        );
        let line = br#"{"items":[{"id":"fan","name":"FAN","unit":"rpm","format":"{value}"}],"values":{"fan":"1\n2"}}"#;
        assert_eq!(
            decode_response_for(line, "demo"),
            Err(ProtoError::ValueHasNewline("fan".into()))
        );
    }

    #[test]
    fn values_must_reference_declared_items() {
        let response = PluginResponse {
            items: vec![item("fan")],
            values: BTreeMap::from([("other".into(), "1".into())]),
        };
        assert_eq!(
            response.validated_values("demo"),
            Err(ProtoError::UndeclaredValue("other".into()))
        );
    }

    #[test]
    fn request_round_trip_keeps_t_and_interval() {
        let request = PluginRequest {
            t: 1_700_000_000.5,
            interval_ms: 1000,
        };
        let encoded = encode_request(&request);
        assert!(encoded.ends_with('\n'));
        assert_eq!(decode_request(encoded.as_bytes()).unwrap(), request);
    }

    #[test]
    fn valid_response_decodes_into_qualified_values() {
        let line = br#"{"items":[{"id":"fan","name":"FAN","unit":"rpm","format":"{value} rpm"}],"values":{"fan":"1420"}}"#;
        let values = decode_response_for(line, "demo").unwrap();
        assert_eq!(values.get("demo.fan").map(String::as_str), Some("1420"));
    }
}
