use super::ProtocolVersion;
use crate::{http_server::AppState, state::ItemState, wbxml::eas::SyncCollectionRequest};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Options {
    filter_type: Option<u8>,
    #[serde(default)]
    mime_support: u8,
    #[serde(default)]
    body_preferences: Vec<crate::wbxml::eas::RequestedBodyPreference>,
    #[serde(default)]
    body_part_preferences: Vec<crate::wbxml::eas::RequestedBodyPreference>,
    #[serde(default, skip_serializing)]
    body_type: Option<u8>,
    #[serde(default, skip_serializing)]
    truncation_size: Option<usize>,
}

pub(super) async fn apply(
    state: &AppState,
    user: &str,
    device: &str,
    version: ProtocolVersion,
    collection: &mut SyncCollectionRequest,
    legacy_filter: u8,
) -> anyhow::Result<()> {
    let key = format!(
        "__options__:{}:{}",
        version.as_str(),
        collection.collection_id
    );
    let options = if collection.options_count > 0 {
        let options = Options {
            filter_type: collection.filter_type,
            mime_support: collection.mime_support,
            body_preferences: collection.body_preferences.clone(),
            body_part_preferences: collection.body_part_preferences.clone(),
            body_type: None,
            truncation_size: None,
        };
        state
            .state
            .put_item_states(
                user,
                device,
                &key,
                vec![ItemState {
                    item_id: "options".to_owned(),
                    hash: serde_json::to_string(&options)?,
                }],
            )
            .await?;
        Some(options)
    } else {
        state
            .state
            .item_states(user, device, &key)
            .await?
            .iter()
            .find(|item| item.item_id == "options")
            .map(|item| serde_json::from_str::<Options>(&item.hash))
            .transpose()?
    };
    if let Some(options) = options {
        collection.filter_type = Some(options.filter_type.unwrap_or(0));
        collection.mime_support = options.mime_support;
        collection.body_preferences = options.body_preferences;
        collection.body_part_preferences = options.body_part_preferences;
        if collection.body_preferences.is_empty() {
            if let Some(body_type) = options.body_type {
                collection
                    .body_preferences
                    .push(crate::wbxml::eas::RequestedBodyPreference {
                        body_type,
                        truncation_size: options.truncation_size,
                        ..Default::default()
                    });
            }
        }
    } else {
        collection.filter_type = Some(collection.filter_type.unwrap_or(legacy_filter));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Options;
    use crate::wbxml::eas::RequestedBodyPreference;

    #[test]
    fn options_persist_body_part_preferences_independently() {
        let options = Options {
            filter_type: Some(0),
            mime_support: 0,
            body_preferences: vec![RequestedBodyPreference {
                body_type: 1,
                ..Default::default()
            }],
            body_part_preferences: vec![RequestedBodyPreference {
                body_type: 4,
                truncation_size: Some(4096),
                ..Default::default()
            }],
            body_type: None,
            truncation_size: None,
        };
        let decoded: Options =
            serde_json::from_str(&serde_json::to_string(&options).unwrap()).unwrap();
        assert_eq!(decoded.body_preferences[0].body_type, 1);
        assert_eq!(decoded.body_part_preferences[0].body_type, 4);
        assert_eq!(decoded.body_part_preferences[0].truncation_size, Some(4096));
    }

    #[test]
    fn old_options_without_body_part_preferences_still_load() {
        let options: Options =
            serde_json::from_str(r#"{"filter_type":0,"mime_support":0,"body_preferences":[]}"#)
                .unwrap();
        assert!(options.body_part_preferences.is_empty());
    }
}
