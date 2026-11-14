use super::ProtocolVersion;
use crate::{http_server::AppState, state::ItemState, wbxml::eas::SyncCollectionRequest};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Options {
    filter_type: Option<u8>,
    body_type: Option<u8>,
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
            body_type: collection.body_pref_type,
            truncation_size: collection.body_pref_truncation_size,
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
        collection.body_pref_type = options.body_type;
        collection.body_pref_truncation_size = options.truncation_size;
    } else {
        collection.filter_type = Some(collection.filter_type.unwrap_or(legacy_filter));
    }
    Ok(())
}
