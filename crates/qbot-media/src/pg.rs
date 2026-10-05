use async_trait::async_trait;
use qbot_core::marker::fill;
use qbot_core::{GroupId, MessageId};
use qbot_store::{PgArchive, PgMediaCache};

use crate::item::MediaRef;
use crate::open::{MediaRefs, RefsError};
use crate::ports::{CacheError, DescriptionCache, EditError, LineEditor};
use qbot_core::MediaKind;

#[async_trait]
impl DescriptionCache for PgMediaCache {
    async fn get(&self, key: &str) -> Result<Option<String>, CacheError> {
        PgMediaCache::get(self, key)
            .await
            .map_err(|e| CacheError(e.to_string()))
    }

    async fn put(&self, key: &str, description: &str) -> Result<(), CacheError> {
        PgMediaCache::put(self, key, description)
            .await
            .map_err(|e| CacheError(e.to_string()))
    }
}

/// Fills markers in lines stored in Postgres.
#[derive(Debug, Clone)]
pub struct PgLineEditor(pub PgArchive);

#[async_trait]
impl LineEditor for PgLineEditor {
    async fn fill(
        &self,
        group: GroupId,
        message: MessageId,
        kind: MediaKind,
        index: usize,
        replacement: &str,
    ) -> Result<bool, EditError> {
        self.0
            .rewrite_text(group, message, |text| {
                fill(text, kind.marker(), index, replacement)
            })
            .await
            .map_err(|e| EditError(e.to_string()))
    }
}

#[async_trait]
impl MediaRefs for PgArchive {
    async fn media_ref(
        &self,
        group: GroupId,
        message: MessageId,
        kind: MediaKind,
        index: u32,
    ) -> Result<Option<MediaRef>, RefsError> {
        let row = PgArchive::media_ref(self, group, message, kind, index)
            .await
            .map_err(|e| RefsError(e.to_string()))?;
        Ok(row.map(|r| MediaRef {
            key: r.key,
            url: r.url,
            file: r.file,
            size: r.size,
        }))
    }
}
