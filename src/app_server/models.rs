use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashSet, future::Future};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningEffort {
    pub reasoning_effort: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub model: String,
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub supported_reasoning_efforts: Vec<ReasoningEffort>,
    pub default_reasoning_effort: Option<String>,
}

impl Model {
    pub fn default_effort(&self) -> Option<&str> {
        self.default_reasoning_effort
            .as_deref()
            .filter(|default| {
                self.supported_reasoning_efforts
                    .iter()
                    .any(|e| e.reasoning_effort == *default)
            })
            .or_else(|| {
                self.supported_reasoning_efforts
                    .first()
                    .map(|e| e.reasoning_effort.as_str())
            })
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ModelSelection {
    pub id: String,
    pub model: String,
    pub display_name: String,
    pub effort: Option<String>,
}
impl ModelSelection {
    pub fn from_model(model: &Model) -> Self {
        Self {
            id: model.id.clone(),
            model: model.model.clone(),
            display_name: model.display_name.clone(),
            effort: model.default_effort().map(str::to_owned),
        }
    }
    pub fn initial(models: &[Model]) -> Option<Self> {
        let visible = models.iter().filter(|m| !m.hidden).collect::<Vec<_>>();
        let defaults = visible.iter().filter(|m| m.is_default).collect::<Vec<_>>();
        let model = if defaults.len() == 1 {
            *defaults[0]
        } else {
            *visible.first()?
        };
        Some(Self::from_model(model))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelPage {
    data: Vec<Model>,
    next_cursor: Option<String>,
}

// The injected request is also used by local tests; no subprocess or inference is needed.
pub(super) async fn collect_models<F, Fut>(mut request: F) -> Result<Vec<Model>>
where
    F: FnMut(Value) -> Fut,
    Fut: Future<Output = Result<Value>>,
{
    let mut cursor = None;
    let mut cursors = HashSet::new();
    let mut ids = HashSet::new();
    let mut models = Vec::new();
    for _ in 0..100 {
        let mut params = json!({"includeHidden":false,"limit":100});
        if let Some(current) = &cursor {
            params["cursor"] = json!(current);
        }
        let page: ModelPage = serde_json::from_value(request(params).await?)?;
        models.extend(
            page.data
                .into_iter()
                .filter(|m| !m.hidden && !m.model.is_empty() && ids.insert(m.id.clone())),
        );
        match page.next_cursor {
            None => return Ok(models),
            Some(next) if !next.is_empty() && cursors.insert(next.clone()) => cursor = Some(next),
            Some(_) => bail!("model/list returned a repeated or empty pagination cursor"),
        }
    }
    bail!("model/list exceeded the pagination limit")
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(crate) fn sample(id: &str, is_default: bool) -> Model {
        serde_json::from_value(json!({"id":id,"model":format!("slug-{id}"),"displayName":format!("Model {id}"),"description":"Test model","hidden":false,"isDefault":is_default,"defaultReasoningEffort":"deep","supportedReasoningEfforts":[{"reasoningEffort":"light","description":"Fast"},{"reasoningEffort":"deep","description":"Thorough"}]})).unwrap()
    }
    #[test]
    fn parses_catalog_defaults_and_model_specific_efforts() {
        let a = sample("a", false);
        let b = sample("b", true);
        let chosen = ModelSelection::initial(&[a.clone(), b.clone()]).unwrap();
        assert_eq!(chosen.model, "slug-b");
        assert_eq!(chosen.effort.as_deref(), Some("deep"));
        assert_eq!(b.supported_reasoning_efforts.len(), 2);
        assert_eq!(ModelSelection::initial(&[a.clone()]).unwrap().id, "a");
        let mut duplicate = a.clone();
        duplicate.is_default = true;
        assert_eq!(ModelSelection::initial(&[duplicate, b]).unwrap().id, "a");
        let mut hidden = a;
        hidden.hidden = true;
        assert!(ModelSelection::initial(&[hidden]).is_none());
        assert!(ModelSelection::initial(&[]).is_none());
        let mut unsupported_default = sample("new", true);
        unsupported_default.default_reasoning_effort = Some("not-supported".into());
        assert_eq!(unsupported_default.default_effort(), Some("light"));
        unsupported_default.supported_reasoning_efforts.clear();
        assert!(unsupported_default.default_effort().is_none());
    }
    #[tokio::test]
    async fn paginates_all_pages_and_filters_hidden_models() {
        let mut page = 0;
        let models = collect_models(|params| {
            assert_eq!(params["includeHidden"], false);
            let response = match page {
                0 => {
                    assert!(params.get("cursor").is_none());
                    json!({"data":[sample("a",false)],"nextCursor":"page2"})
                }
                1 => {
                    assert_eq!(params["cursor"], "page2");
                    json!({"data":[sample("b",true)],"nextCursor":"page3"})
                }
                _ => {
                    assert_eq!(params["cursor"], "page3");
                    let mut hidden = sample("hidden", false);
                    hidden.hidden = true;
                    json!({"data":[hidden,sample("c",false)],"nextCursor":null})
                }
            };
            page += 1;
            std::future::ready(Ok(response))
        })
        .await
        .unwrap();
        assert_eq!(page, 3);
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
    }
    #[tokio::test]
    async fn catalog_failure_and_repeated_cursor_return_errors() {
        let failed =
            collect_models(|_| std::future::ready(Err(anyhow::anyhow!("unavailable")))).await;
        assert!(failed.is_err());
        assert!(
            collect_models(|_| std::future::ready(Ok(json!({"data":[],"nextCursor":"same"}))))
                .await
                .is_err()
        );
    }
}
