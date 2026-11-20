//! Optional, credential-free route enrichment of the v1 discovery catalog.
//! The published v2 API is a provider map, not the authored TOML schema.
use super::*;

pub(super) const FETCH_BUDGET: Duration = Duration::from_secs(3);

pub(super) const SOURCE: &str = "https://models.dev/experimental/v2.0/api.json";

/// Validate the projection before installing any changes. User connection
/// configuration is deliberately not an input to this function.
pub(super) fn enrich(catalog: &mut ProviderConfigFile, api: &Value) -> Result<(), String> {
    let root = api.as_object().ok_or("v2 catalog root must be an object")?;
    let mut next = catalog.clone();
    for (provider_id, provider) in &mut next.providers {
        // This identity belongs exclusively to the retained explicit OAuth flow.
        if provider_id == "openai-codex" {
            continue;
        }
        let Some(remote) = root.get(provider_id) else { continue };
        if remote.get("id").and_then(Value::as_str) != Some(provider_id.as_str()) {
            return Err("v2 provider identity mismatch".into());
        }
        let models = remote.get("models").and_then(Value::as_object)
            .ok_or("v2 provider models must be an object")?;
        for (model_id, model) in &mut provider.models {
            if model.wire_api.or(provider.wire_api) == Some(ProviderWireApi::GoogleGenerativeAi) {
                continue;
            }
            let Some(remote) = models.get(model_id) else { continue };
            let request_model = remote.get("id").and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty()).ok_or("v2 model id missing")?;
            if request_model != model_id {
                return Err("v2 model identity mismatch".into());
            }
            // v1 may have a provider-specific request alias. Without an explicit
            // v2 alias contract, keep that entire route rather than mix tuples.
            if model.request_model.as_deref().is_some_and(|id| id != request_model) {
                continue;
            }
            if remote.get("type").and_then(Value::as_str) != Some("chat")
                || remote.pointer("/capabilities/tools/supported") != Some(&Value::Bool(true))
                || remote.get("status").and_then(Value::as_str) == Some("deprecated")
            {
                continue;
            }
            let routes = remote.get("api").and_then(Value::as_object)
                .ok_or("v2 model api must be an object")?;
            for (name, wire, path) in [
                ("messages", ProviderWireApi::AnthropicMessages, "/messages"),
                ("responses", ProviderWireApi::OpenAIResponses, "/responses"),
                ("chat-completions", ProviderWireApi::OpenAIChatCompletions, "/chat/completions"),
            ] {
                let Some(route) = routes.get(name) else { continue };
                if route.pointer("/capabilities/tools/supported") == Some(&Value::Bool(false)) {
                    continue;
                }
                // v2 can declare protocol-specific body conventions that the
                // retained v1 adapter metadata does not express. Do not change
                // route while silently losing those requirements.
                if route.get("compatibility").is_some_and(|value| {
                    value.as_object().is_none_or(|fields| !fields.is_empty())
                }) {
                    continue;
                }
                // Custom paths are not expressible by the current adapters.
                if route.get("path").is_some_and(|value| value.as_str() != Some(path)) {
                    continue;
                }
                let Some(base) = route.get("base_url").and_then(Value::as_str) else { continue };
                let Ok(url) = reqwest::Url::parse(base) else { continue };
                if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none()
                    || !url.username().is_empty() || url.password().is_some()
                    || url.query().is_some() || url.fragment().is_some() || base.contains("${")
                {
                    continue;
                }
                let base = base.trim_end_matches('/');
                let endpoint = if wire == ProviderWireApi::AnthropicMessages {
                    // Published Messages bases include the API version; Devo
                    // appends /v1/messages. Other versions cannot be represented.
                    let Some(base) = base.strip_suffix("/v1") else { continue };
                    base
                } else {
                    // Server normalize_openai_base_url injects /v1 for a bare
                    // host. v2 declares base + protocol suffix literally, so
                    // such a route cannot retain its target through the adapter.
                    if url.path().trim_matches('/').is_empty() {
                        continue;
                    }
                    base
                };
                model.base_url = Some(endpoint.to_owned());
                model.wire_api = Some(wire);
                model.request_model = Some(request_model.to_owned());
                break;
            }
        }
    }
    *catalog = next;
    Ok(())
}

#[cfg(test)]
#[path = "transport_v2_tests.rs"]
mod tests;
