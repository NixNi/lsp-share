use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;
use std::borrow::Cow;

/// Custom deserializer ensuring `"result": null` or `"id": null` is deserialized as `Some("null")`
/// rather than `None`, preserving JSON-RPC 2.0 null fields verbatim.
fn deserialize_present_raw<'de, D>(deserializer: D) -> Result<Option<&'de RawValue>, D::Error>
where
    D: Deserializer<'de>,
{
    <&'de RawValue as Deserialize>::deserialize(deserializer).map(Some)
}

#[derive(Deserialize)]
pub struct Envelope<'a> {
    #[serde(default, borrow)]
    pub jsonrpc: Option<Cow<'a, str>>,
    #[serde(default, borrow, deserialize_with = "deserialize_present_raw")]
    pub id: Option<&'a RawValue>,
    #[serde(default, borrow)]
    pub method: Option<Cow<'a, str>>,
    #[serde(default, borrow, deserialize_with = "deserialize_present_raw")]
    pub params: Option<&'a RawValue>,
    #[serde(default, borrow, deserialize_with = "deserialize_present_raw")]
    pub result: Option<&'a RawValue>,
    #[serde(default, borrow, deserialize_with = "deserialize_present_raw")]
    pub error: Option<&'a RawValue>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Request,
    Notification,
    Response,
    Invalid,
}

impl<'a> Envelope<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, serde_json::Error> {
        let s = std::str::from_utf8(bytes)
            .map_err(|_| serde::de::Error::custom("LSP JSON-RPC payload is not valid UTF-8"))?;
        serde_json::from_str(s)
    }

    pub fn kind(&self) -> MessageKind {
        match (self.id, self.method.as_deref()) {
            (Some(_), Some(_)) => MessageKind::Request,
            (None, Some(_)) => MessageKind::Notification,
            (Some(_), None) if self.result.is_some() || self.error.is_some() => {
                MessageKind::Response
            }
            _ => MessageKind::Invalid,
        }
    }
}

/// Normalizes a JSON-RPC ID token to a HashMap lookup key without conflating `1` and `"1"`.
pub fn normalize_id_key(raw: &RawValue) -> String {
    raw.get().trim().to_string()
}

pub fn rewrite_request_id(env: &Envelope<'_>, new_id_json: &str) -> Vec<u8> {
    #[derive(Serialize)]
    struct Req<'a> {
        jsonrpc: &'a str,
        id: &'a RawValue,
        method: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        params: Option<&'a RawValue>,
    }
    let new_id = RawValue::from_string(new_id_json.to_string())
        .unwrap_or_else(|_| RawValue::from_string("0".to_string()).unwrap());
    let msg = Req {
        jsonrpc: env.jsonrpc.as_deref().unwrap_or("2.0"),
        id: &new_id,
        method: env.method.as_deref().unwrap_or(""),
        params: env.params,
    };
    serde_json::to_vec(&msg).unwrap_or_default()
}

pub fn rewrite_response_id(env: &Envelope<'_>, orig_id_json: &str) -> Vec<u8> {
    #[derive(Serialize)]
    struct Resp<'a> {
        jsonrpc: &'a str,
        id: &'a RawValue,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<&'a RawValue>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<&'a RawValue>,
    }
    let id = RawValue::from_string(orig_id_json.to_string())
        .unwrap_or_else(|_| RawValue::from_string("null".to_string()).unwrap());
    let msg = Resp {
        jsonrpc: env.jsonrpc.as_deref().unwrap_or("2.0"),
        id: &id,
        result: env.result,
        error: env.error,
    };
    serde_json::to_vec(&msg).unwrap_or_default()
}

pub fn build_ok_null_response(id_raw: &RawValue) -> Vec<u8> {
    #[derive(Serialize)]
    struct OkNull<'a> {
        jsonrpc: &'static str,
        id: &'a RawValue,
        result: serde_json::Value,
    }
    serde_json::to_vec(&OkNull {
        jsonrpc: "2.0",
        id: id_raw,
        result: serde_json::Value::Null,
    })
    .unwrap_or_default()
}

pub fn build_ok_response(id_raw: &RawValue, result: &serde_json::Value) -> Vec<u8> {
    #[derive(Serialize)]
    struct OkResp<'a> {
        jsonrpc: &'static str,
        id: &'a RawValue,
        result: &'a serde_json::Value,
    }
    serde_json::to_vec(&OkResp {
        jsonrpc: "2.0",
        id: id_raw,
        result,
    })
    .unwrap_or_default()
}

pub fn build_ok_raw_response(id_raw: &RawValue, result_raw: &RawValue) -> Vec<u8> {
    #[derive(Serialize)]
    struct OkRaw<'a> {
        jsonrpc: &'static str,
        id: &'a RawValue,
        result: &'a RawValue,
    }
    serde_json::to_vec(&OkRaw {
        jsonrpc: "2.0",
        id: id_raw,
        result: result_raw,
    })
    .unwrap_or_default()
}

pub fn build_error_response(id_raw: &RawValue, code: i64, message: &str) -> Vec<u8> {
    #[derive(Serialize)]
    struct ErrBody<'a> {
        code: i64,
        message: &'a str,
    }
    #[derive(Serialize)]
    struct ErrResp<'a> {
        jsonrpc: &'static str,
        id: &'a RawValue,
        error: ErrBody<'a>,
    }
    serde_json::to_vec(&ErrResp {
        jsonrpc: "2.0",
        id: id_raw,
        error: ErrBody { code, message },
    })
    .unwrap_or_default()
}

pub fn build_notification(method: &str, params: &serde_json::Value) -> Vec<u8> {
    #[derive(Serialize)]
    struct Notif<'a> {
        jsonrpc: &'static str,
        method: &'a str,
        params: &'a serde_json::Value,
    }
    serde_json::to_vec(&Notif {
        jsonrpc: "2.0",
        method,
        params,
    })
    .unwrap_or_default()
}

pub fn build_request(id_json: &str, method: &str, params: &serde_json::Value) -> Vec<u8> {
    #[derive(Serialize)]
    struct Req<'a> {
        jsonrpc: &'static str,
        id: &'a RawValue,
        method: &'a str,
        params: &'a serde_json::Value,
    }
    let id_rv = RawValue::from_string(id_json.to_string()).unwrap();
    serde_json::to_vec(&Req {
        jsonrpc: "2.0",
        id: &id_rv,
        method,
        params,
    })
    .unwrap_or_default()
}

pub fn build_cancel_notification(server_id: u64) -> Vec<u8> {
    build_notification("$/cancelRequest", &serde_json::json!({ "id": server_id }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_result_null_verbatim() {
        let raw = br#"{"jsonrpc":"2.0","id":42,"result":null}"#;
        let env = Envelope::parse(raw).unwrap();
        assert_eq!(env.kind(), MessageKind::Response);
        assert_eq!(env.result.unwrap().get(), "null");

        let rewritten = rewrite_response_id(&env, "1");
        let parsed: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(parsed["id"], 1);
        assert!(parsed.get("result").is_some());
        assert!(parsed["result"].is_null());
    }

    #[test]
    fn preserves_string_vs_int_ids_and_large_raw_params() {
        let raw = br#"{"jsonrpc":"2.0","id":"req-1","method":"textDocument/completion","params":{"big":[1,2,3]}}"#;
        let env = Envelope::parse(raw).unwrap();
        assert_eq!(env.kind(), MessageKind::Request);
        assert_eq!(normalize_id_key(env.id.unwrap()), "\"req-1\"");

        let rewritten = rewrite_request_id(&env, "99");
        let parsed: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(parsed["id"], 99);
        assert_eq!(parsed["method"], "textDocument/completion");
        assert_eq!(parsed["params"]["big"], serde_json::json!([1, 2, 3]));
    }
}
