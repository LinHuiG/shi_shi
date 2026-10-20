//! Share one bounded fail-closed output validator across providers.
use serde_json::Value;
pub fn validate(schema: &Value, value: &Value) -> Result<(), Vec<String>> {
    shishi_api::schema_validate::validate(schema, value).map_err(|e| vec![e.to_string()])
}
