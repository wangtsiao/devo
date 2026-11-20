use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

fn catalog() -> ProviderConfigFile {
    convert_models_dev_api(&json!({"deepseek": {
        "npm": "@ai-sdk/deepseek", "api": "https://api.deepseek.com",
        "models": {"deepseek-chat": {"tool_call": true}}
    }})).unwrap().0
}

fn projection(routes: Value) -> Value {
    json!({"deepseek": {"id": "deepseek", "models": {
        "deepseek-chat": {"id": "deepseek-chat", "type": "chat",
            "capabilities": {"tools": {"supported": true}}, "api": routes}
    }}})
}

#[test]
fn preference_selects_a_coherent_tuple_and_normalizes_deepseek_messages() {
    for (routes, wire, endpoint) in [
        (json!({"messages": {"base_url": "https://api.deepseek.com/anthropic/v1/"},
            "responses": {"base_url": "https://responses.example/v1"},
            "chat-completions": {"base_url": "https://chat.example/v1"}}),
            ProviderWireApi::AnthropicMessages, "https://api.deepseek.com/anthropic"),
        (json!({"responses": {"base_url": "https://responses.example/v1"},
            "chat-completions": {"base_url": "https://chat.example/v1"}}),
            ProviderWireApi::OpenAIResponses, "https://responses.example/v1"),
        (json!({"chat-completions": {"base_url": "https://chat.example/v1"}}),
            ProviderWireApi::OpenAIChatCompletions, "https://chat.example/v1"),
    ] {
        let mut actual = catalog();
        let mut expected = actual.clone();
        let model = expected.providers.get_mut("deepseek").unwrap().models.get_mut("deepseek-chat").unwrap();
        model.base_url = Some(endpoint.into());
        model.wire_api = Some(wire);
        model.request_model = Some("deepseek-chat".into());
        enrich(&mut actual, &projection(routes)).unwrap();
        assert_eq!(serde_json::to_value(actual).unwrap(), serde_json::to_value(expected).unwrap());
    }
}

#[test]
fn unrepresentable_or_unsupported_messages_uses_next_declared_route() {
    for messages in [
        json!({"base_url": "https://messages.example/v1", "path": "/custom"}),
        json!({"base_url": "${API_HOST}/v1"}),
        json!({"base_url": "https://messages.example/v1", "compatibility": {"reasoning_field": "reasoning_content"}}),
        json!({"base_url": "https://user:secret@messages.example/v1"}),
        json!({"base_url": "https://messages.example/v2"}),
        json!({"base_url": "https://messages.example/v1?token=x"}),
        json!({"base_url": "https://messages.example/v1", "capabilities": {"tools": {"supported": false}}}),
    ] {
        let mut actual = catalog();
        let mut expected = catalog();
        let chat = json!({"base_url": "https://api.deepseek.com/v1"});
        enrich(&mut expected, &projection(json!({"chat-completions": chat}))).unwrap();
        enrich(&mut actual, &projection(json!({"messages": messages, "chat-completions": chat}))).unwrap();
        assert_eq!(serde_json::to_value(actual).unwrap(), serde_json::to_value(expected).unwrap());
    }
}

#[test]
fn absent_metadata_and_invalid_projection_preserve_v1() {
    for api in [json!({}), projection(json!({})), projection(json!({"messages": {}})),
        json!({"deepseek": {"id": "wrong", "models": {}}}), json!([])] {
        let mut actual = catalog();
        let expected = serde_json::to_value(&actual).unwrap();
        let _ = enrich(&mut actual, &api);
        assert_eq!(serde_json::to_value(actual).unwrap(), expected);
    }
}

#[test]
fn aliases_google_and_oauth_identity_are_not_reinterpreted() {
    for mode in ["alias", "google", "oauth"] {
        let mut actual = catalog();
        let mut api = projection(json!({"messages": {"base_url": "https://messages.example/v1"}}));
        match mode {
            "alias" => actual.providers.get_mut("deepseek").unwrap().models.get_mut("deepseek-chat").unwrap().request_model = Some("provider-alias".into()),
            "google" => actual.providers.get_mut("deepseek").unwrap().models.get_mut("deepseek-chat").unwrap().wire_api = Some(ProviderWireApi::GoogleGenerativeAi),
            "oauth" => {
                let entry = actual.providers.remove("deepseek").unwrap();
                actual.providers.insert("openai-codex".into(), entry);
                let mut entry = api.as_object_mut().unwrap().remove("deepseek").unwrap();
                entry["id"] = json!("openai-codex");
                api["openai-codex"] = entry;
            }
            _ => unreachable!(),
        }
        let expected = serde_json::to_value(&actual).unwrap();
        enrich(&mut actual, &api).unwrap();
        assert_eq!(serde_json::to_value(actual).unwrap(), expected);
    }
}

#[test]
fn configured_connections_strip_enrichment_even_when_partial() {
    let home = tempfile::tempdir().unwrap();
    let mut remote = catalog();
    enrich(&mut remote, &projection(json!({"messages": {"base_url": "https://api.deepseek.com/anthropic/v1"}}))).unwrap();
    LIVE_CATALOGS.get_or_init(Default::default).lock().unwrap().insert(home.path().to_owned(), LiveCatalog {
        source: SOURCE.into(), fetched_at: Instant::now(), catalog: remote,
    });
    for route in [json!({"base_url": "https://manual.example", "wire_api": "openai_responses"}),
        json!({"base_url": "https://manual.example"}), json!({"credential": "fixture-id"})] {
        let overlay: ProviderConfigFile = serde_json::from_value(json!({"provider": {"deepseek": route}})).unwrap();
        let effective = crate::effective_provider_catalog_with_home(&overlay, Some(home.path())).unwrap();
        let provider = &effective.providers["deepseek"];
        let model = &provider.models["deepseek-chat"];
        let expected = &overlay.providers["deepseek"];
        assert_eq!((provider.base_url.clone(), provider.wire_api, model.base_url.clone(), model.wire_api, model.request_model.clone()),
            (expected.base_url.clone(), expected.wire_api, None, None, None));
    }
    LIVE_CATALOGS.get().unwrap().lock().unwrap().remove(home.path());
}

#[test]
fn root_openai_routes_do_not_change_the_declared_target() {
    for name in ["responses", "chat-completions"] {
        for base in ["https://gateway.example", "https://gateway.example/", "https://gateway.example///"] {
            let mut actual = catalog();
            let expected = serde_json::to_value(&actual).unwrap();
            let mut routes = serde_json::Map::new();
            routes.insert(name.into(), json!({"base_url": base}));
            enrich(&mut actual, &projection(Value::Object(routes))).unwrap();
            assert_eq!(serde_json::to_value(actual).unwrap(), expected);
        }
    }
}

#[test]
fn unrepresentable_responses_falls_back_to_declared_explicit_chat_path() {
    let mut actual = catalog();
    let mut expected = catalog();
    let chat = json!({"base_url": "https://gateway.example/api"});
    enrich(&mut expected, &projection(json!({"chat-completions": chat}))).unwrap();
    enrich(&mut actual, &projection(json!({
        "responses": {"base_url": "https://gateway.example"},
        "chat-completions": chat
    }))).unwrap();
    assert_eq!(serde_json::to_value(actual).unwrap(), serde_json::to_value(expected).unwrap());
}

#[tokio::test(start_paused = true)]
async fn optional_fetch_has_a_separate_three_second_budget() {
    let start = tokio::time::Instant::now();
    let outcome = tokio::time::timeout(FETCH_BUDGET, std::future::pending::<()>()).await;
    assert!(outcome.is_err());
    assert_eq!(start.elapsed(), Duration::from_secs(3));
}
