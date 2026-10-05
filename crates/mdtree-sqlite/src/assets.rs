//! Image asset storage: named, content-addressed images inside the workspace.

use std::str::FromStr;

use mdtree_core::{
    hash_asset, inspect_image, AssetName, AssetRecord, MediaType, NodeHash, NodeId, SnapshotAsset,
};
use rusqlite::{params, OptionalExtension, Row};

use crate::store::{integer, nonnegative, IntegrityFinding, SqliteStore, StoreError};

/// What to do when an asset with the requested name already exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssetWriteMode {
    /// Fail with [`StoreError::AssetExists`].
    Create,
    /// Replace the existing image under the same name.
    Replace,
    /// Keep the existing image and store under the first free `name-N`;
    /// identical bytes already stored under the name are reused instead.
    Unique,
}

const ASSET_COLUMNS: &str = "name,hash,media_type,byte_size,width,height,created_at,updated_at";

impl SqliteStore {
    /// Stores image `bytes` under `name`, returning the stored record (whose
    /// name differs from `name` only in [`AssetWriteMode::Unique`]).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Asset`] for unsupported or oversized images,
    /// [`StoreError::AssetExists`] in [`AssetWriteMode::Create`], or a storage error.
    pub fn put_asset(
        &mut self,
        name: &AssetName,
        bytes: &[u8],
        mode: AssetWriteMode,
        now_ms: u64,
    ) -> Result<AssetRecord, StoreError> {
        let info = inspect_image(bytes)?;
        let hash = hash_asset(bytes);
        let transaction = self.connection_mut().transaction()?;
        let existing = |name: &AssetName| -> Result<Option<AssetRecord>, StoreError> {
            transaction
                .query_row(
                    &format!("SELECT {ASSET_COLUMNS} FROM assets WHERE name=?1"),
                    [name.as_str()],
                    asset_record,
                )
                .optional()?
                .transpose()
        };
        let mut target = name.clone();
        let mut created_at = now_ms;
        if let Some(current) = existing(&target)? {
            match mode {
                AssetWriteMode::Create => return Err(StoreError::AssetExists(target.to_string())),
                AssetWriteMode::Replace => created_at = current.created_at,
                AssetWriteMode::Unique if current.hash == hash => return Ok(current),
                AssetWriteMode::Unique => {
                    let mut number = 2;
                    loop {
                        target = name.numbered(number);
                        match existing(&target)? {
                            None => break,
                            Some(current) if current.hash == hash => return Ok(current),
                            Some(_) => number += 1,
                        }
                    }
                }
            }
        }
        transaction.execute(
            "INSERT OR IGNORE INTO asset_blobs(hash,data) VALUES(?1,?2)",
            params![hash.as_bytes().as_slice(), bytes],
        )?;
        let byte_size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        transaction.execute(
            "INSERT INTO assets(name,hash,media_type,byte_size,width,height,created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(name) DO UPDATE SET hash=excluded.hash, media_type=excluded.media_type,
               byte_size=excluded.byte_size, width=excluded.width, height=excluded.height,
               updated_at=excluded.updated_at",
            params![
                target.as_str(),
                hash.as_bytes().as_slice(),
                info.media_type.as_str(),
                integer(byte_size)?,
                info.width,
                info.height,
                integer(created_at)?,
                integer(now_ms)?
            ],
        )?;
        delete_unused_blobs(&transaction)?;
        let record = transaction.query_row(
            &format!("SELECT {ASSET_COLUMNS} FROM assets WHERE name=?1"),
            [target.as_str()],
            asset_record,
        )??;
        transaction.commit()?;
        Ok(record)
    }

    /// One asset's metadata.
    ///
    /// # Errors
    ///
    /// Returns a storage error.
    pub fn asset(&self, name: &AssetName) -> Result<Option<AssetRecord>, StoreError> {
        self.connection()
            .query_row(
                &format!("SELECT {ASSET_COLUMNS} FROM assets WHERE name=?1"),
                [name.as_str()],
                asset_record,
            )
            .optional()?
            .transpose()
    }

    /// One asset's metadata and image bytes.
    ///
    /// # Errors
    ///
    /// Returns a storage error.
    pub fn asset_bytes(
        &self,
        name: &AssetName,
    ) -> Result<Option<(AssetRecord, Vec<u8>)>, StoreError> {
        let Some(record) = self.asset(name)? else {
            return Ok(None);
        };
        let data: Vec<u8> = self.connection().query_row(
            "SELECT data FROM asset_blobs WHERE hash=?1",
            [record.hash.as_bytes().as_slice()],
            |row| row.get(0),
        )?;
        Ok(Some((record, data)))
    }

    /// Every asset's metadata, ordered by name.
    ///
    /// # Errors
    ///
    /// Returns a storage error.
    pub fn assets(&self) -> Result<Vec<AssetRecord>, StoreError> {
        let mut statement = self
            .connection()
            .prepare(&format!("SELECT {ASSET_COLUMNS} FROM assets ORDER BY name"))?;
        let rows = statement.query_map([], asset_record)?;
        rows.map(|row| row?).collect()
    }

    /// Deletes an asset (and its bytes when no other name shares them);
    /// `false` when it does not exist. Referencing nodes are not changed.
    ///
    /// # Errors
    ///
    /// Returns a storage error.
    pub fn remove_asset(&mut self, name: &AssetName) -> Result<bool, StoreError> {
        let transaction = self.connection_mut().transaction()?;
        let removed = transaction.execute("DELETE FROM assets WHERE name=?1", [name.as_str()])? > 0;
        delete_unused_blobs(&transaction)?;
        transaction.commit()?;
        Ok(removed)
    }

    /// Nodes whose Markdown references `name` as an `asset:` image, by ID.
    ///
    /// # Errors
    ///
    /// Returns a storage error.
    pub fn asset_usages(&self, name: &AssetName) -> Result<Vec<NodeId>, StoreError> {
        let mut usages = Vec::new();
        for (id, markdown) in self.nodes_mentioning(&format!("asset:{name}"))? {
            if mdtree_markdown::extract_asset_references(&markdown)
                .iter()
                .any(|reference| &reference.name == name)
            {
                usages.push(id);
            }
        }
        Ok(usages)
    }

    /// Every asset with its bytes, as carried by a snapshot.
    pub(crate) fn snapshot_assets(&self) -> Result<Vec<SnapshotAsset>, StoreError> {
        self.assets()?
            .into_iter()
            .map(|record| {
                let (record, bytes) = self
                    .asset_bytes(&record.name)?
                    .ok_or_else(|| StoreError::NotFound(record.name.to_string()))?;
                Ok(SnapshotAsset {
                    name: record.name,
                    media_type: record.media_type,
                    data: mdtree_core::encode_base64(&bytes),
                })
            })
            .collect()
    }

    /// `missing_asset` for each `asset:` image whose asset does not exist,
    /// and `asset_hash` for stored bytes that no longer match their hash.
    pub(crate) fn asset_integrity_findings(&self) -> Result<Vec<IntegrityFinding>, StoreError> {
        let mut findings = Vec::new();
        for (id, markdown) in self.nodes_mentioning("asset:")? {
            let mut reported = std::collections::BTreeSet::new();
            for reference in mdtree_markdown::extract_asset_references(&markdown) {
                if self.asset(&reference.name)?.is_none() && reported.insert(reference.name.clone())
                {
                    findings.push(IntegrityFinding {
                        code: "missing_asset",
                        node_id: Some(id),
                        detail: format!("image asset {} does not exist", reference.name),
                    });
                }
            }
        }
        let mut statement = self
            .connection()
            .prepare("SELECT hash,data FROM asset_blobs ORDER BY hash")?;
        let blobs = statement.query_map([], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        for blob in blobs {
            let (hash, data) = blob?;
            if hash_asset(&data).as_bytes().as_slice() != hash.as_slice() {
                findings.push(IntegrityFinding {
                    code: "asset_hash",
                    node_id: None,
                    detail: "stored image bytes do not match their content hash".into(),
                });
            }
        }
        Ok(findings)
    }

    /// `(id, markdown)` of every node whose Markdown contains `needle`, a
    /// cheap prefilter before exact Markdown parsing.
    fn nodes_mentioning(&self, needle: &str) -> Result<Vec<(NodeId, String)>, StoreError> {
        let mut statement = self.connection().prepare(
            "SELECT id,markdown_content FROM nodes WHERE instr(markdown_content,?1)>0 ORDER BY id",
        )?;
        let rows = statement.query_map([needle], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (id, markdown) = row?;
            let id = NodeId::from_str(&id)
                .map_err(|error| StoreError::InvalidData(error.to_string()))?;
            Ok((id, markdown))
        })
        .collect()
    }
}

/// Inserts snapshot assets during import (bytes already validated).
pub(crate) fn insert_snapshot_asset(
    transaction: &rusqlite::Transaction<'_>,
    asset: &SnapshotAsset,
    now_ms: u64,
) -> Result<(), StoreError> {
    let bytes = mdtree_core::decode_base64(&asset.data).ok_or_else(|| {
        StoreError::InvalidData(format!("asset {} is not valid base64", asset.name))
    })?;
    let info = inspect_image(&bytes)?;
    let hash = hash_asset(&bytes);
    transaction.execute(
        "INSERT OR IGNORE INTO asset_blobs(hash,data) VALUES(?1,?2)",
        params![hash.as_bytes().as_slice(), bytes],
    )?;
    transaction.execute(
        "INSERT INTO assets(name,hash,media_type,byte_size,width,height,created_at,updated_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?7)",
        params![
            asset.name.as_str(),
            hash.as_bytes().as_slice(),
            info.media_type.as_str(),
            integer(u64::try_from(bytes.len()).unwrap_or(u64::MAX))?,
            info.width,
            info.height,
            integer(now_ms)?
        ],
    )?;
    Ok(())
}

fn delete_unused_blobs(transaction: &rusqlite::Transaction<'_>) -> Result<(), StoreError> {
    transaction.execute(
        "DELETE FROM asset_blobs WHERE hash NOT IN (SELECT hash FROM assets)",
        [],
    )?;
    Ok(())
}

/// Maps one `assets` row; the inner result reports corrupt persisted values.
fn asset_record(row: &Row<'_>) -> rusqlite::Result<Result<AssetRecord, StoreError>> {
    let name: String = row.get(0)?;
    let hash: Vec<u8> = row.get(1)?;
    let media_type: String = row.get(2)?;
    let values = (
        row.get::<_, i64>(3)?,
        row.get::<_, u32>(4)?,
        row.get::<_, u32>(5)?,
        row.get::<_, i64>(6)?,
        row.get::<_, i64>(7)?,
    );
    Ok((|| {
        let (byte_size, width, height, created_at, updated_at) = values;
        let hash: [u8; 32] = hash
            .try_into()
            .map_err(|_| StoreError::InvalidData("asset hash is not 32 bytes".into()))?;
        Ok(AssetRecord {
            name: AssetName::from_str(&name)?,
            hash: NodeHash::new(hash),
            media_type: MediaType::from_str(&media_type)?,
            byte_size: nonnegative(byte_size)?,
            width,
            height,
            created_at: nonnegative(created_at)?,
            updated_at: nonnegative(updated_at)?,
        })
    })())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use mdtree_core::{
        hash_content, hash_revision, AssetName, Node, NodeFields, NodeId, NodeMetadata,
        RevisionHashInput, Slug,
    };
    use tempfile::{tempdir, TempDir};

    use super::AssetWriteMode;
    use crate::snapshot::{
        export_markdown_snapshot, export_snapshot, import_markdown_snapshot_new,
        import_snapshot_new,
    };
    use crate::{create_workspace, SqliteStore, StoreError};

    /// A minimal valid PNG header; `inspect_image` only reads IHDR.
    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes
    }

    fn name(raw: &str) -> AssetName {
        raw.parse().expect("asset name")
    }

    fn workspace(markdown: &str) -> (TempDir, SqliteStore) {
        let directory = tempdir().expect("tempdir");
        let id = NodeId::from_str("01JZ8Q5CWPN8T7KPN5A1V9B6XM").expect("ID");
        let slug = Slug::from_str("root").expect("slug");
        let metadata = NodeMetadata::new("Root");
        let revision_hash = hash_revision(RevisionHashInput {
            node_id: id,
            parent_id: None,
            slug: &slug,
            metadata: &metadata,
            markdown_content: markdown,
            sibling_order: 0,
        })
        .expect("hash");
        let root = Node::new(
            NodeFields {
                id,
                slug,
                metadata,
                markdown_content: markdown.into(),
                sibling_order: 0,
                version: 1,
                content_hash: hash_content(markdown),
                revision_hash,
                created_at: 1,
                updated_at: 1,
            },
            None,
        )
        .expect("root");
        let connection =
            create_workspace(&directory.path().join("w.mdtree"), "W", &root).expect("workspace");
        (directory, SqliteStore::new(connection))
    }

    fn blob_count(store: &SqliteStore) -> u32 {
        store
            .connection()
            .query_row("SELECT COUNT(*) FROM asset_blobs", [], |row| row.get(0))
            .expect("count")
    }

    #[test]
    fn assets_are_stored_by_name_with_shared_bytes_and_cleaned_up() {
        let (_directory, mut store) = workspace("# Root\n");
        let first = store
            .put_asset(&name("a.png"), &png(3, 2), AssetWriteMode::Create, 10)
            .expect("put");
        assert_eq!((first.width, first.height, first.byte_size), (3, 2, 29));
        assert!(matches!(
            store.put_asset(&name("a.png"), &png(4, 4), AssetWriteMode::Create, 11),
            Err(StoreError::AssetExists(_))
        ));
        // Identical bytes under a second name share one blob.
        store
            .put_asset(&name("b.png"), &png(3, 2), AssetWriteMode::Create, 12)
            .expect("put");
        assert_eq!(blob_count(&store), 1);
        // Unique mode reuses identical bytes and numbers different ones.
        let same = store
            .put_asset(&name("a.png"), &png(3, 2), AssetWriteMode::Unique, 13)
            .expect("put");
        assert_eq!(same.name.as_str(), "a.png");
        let other = store
            .put_asset(&name("a.png"), &png(5, 5), AssetWriteMode::Unique, 14)
            .expect("put");
        assert_eq!(other.name.as_str(), "a-2.png");
        // Replacing keeps the creation time and drops no-longer-used bytes.
        let replaced = store
            .put_asset(&name("b.png"), &png(9, 9), AssetWriteMode::Replace, 15)
            .expect("replace");
        assert_eq!(
            (replaced.created_at, replaced.updated_at, replaced.width),
            (12, 15, 9)
        );
        let (_, bytes) = store
            .asset_bytes(&name("b.png"))
            .expect("read")
            .expect("asset");
        assert_eq!(bytes, png(9, 9));
        assert!(store.remove_asset(&name("a-2.png")).expect("remove"));
        assert!(!store.remove_asset(&name("a-2.png")).expect("remove again"));
        let names: Vec<_> = store
            .assets()
            .expect("list")
            .into_iter()
            .map(|asset| asset.name.to_string())
            .collect();
        assert_eq!(names, vec!["a.png", "b.png"]);
        assert_eq!(blob_count(&store), 2);
        assert!(matches!(
            store.put_asset(&name("c.png"), b"<svg/>", AssetWriteMode::Create, 16),
            Err(StoreError::Asset(_))
        ));
    }

    #[test]
    fn integrity_reports_missing_assets_and_usages_are_found() {
        let (_directory, mut store) =
            workspace("![a](asset:a.png) ![m](asset:missing.png)\n\n`![c](asset:code.png)`\n");
        store
            .put_asset(&name("a.png"), &png(1, 1), AssetWriteMode::Create, 1)
            .expect("put");
        let findings = store.validate_integrity().expect("validate").findings;
        let missing: Vec<_> = findings
            .iter()
            .filter(|finding| finding.code == "missing_asset")
            .map(|finding| finding.detail.as_str())
            .collect();
        assert_eq!(missing, vec!["image asset missing.png does not exist"]);
        assert_eq!(store.asset_usages(&name("a.png")).expect("usages").len(), 1);
        assert!(store
            .asset_usages(&name("code.png"))
            .expect("usages")
            .is_empty());
    }

    #[test]
    fn snapshots_carry_assets_in_json_and_markdown_and_import_them() {
        let (directory, mut store) = workspace("![a](asset:a.png)\n");
        let plain = export_snapshot(&store).expect("export");
        assert_eq!(
            plain.format_version, 1,
            "asset-free snapshots stay version 1"
        );
        store
            .put_asset(&name("a.png"), &png(3, 2), AssetWriteMode::Create, 1)
            .expect("put");
        let snapshot = export_snapshot(&store).expect("export");
        assert_eq!(snapshot.format_version, 2);
        assert_eq!(snapshot.assets.len(), 1);

        let imported_path = directory.path().join("imported.mdtree");
        import_snapshot_new(&imported_path, &snapshot).expect("JSON import");
        let imported = SqliteStore::open(&imported_path).expect("open");
        assert_eq!(
            imported
                .asset_bytes(&name("a.png"))
                .expect("read")
                .expect("asset")
                .1,
            png(3, 2)
        );

        let markdown_path = directory.path().join("markdown");
        export_markdown_snapshot(&store, &markdown_path).expect("Markdown export");
        assert_eq!(
            std::fs::read(markdown_path.join("_assets/a.png")).expect("asset file"),
            png(3, 2)
        );
        let markdown_import = directory.path().join("markdown.mdtree");
        import_markdown_snapshot_new(&markdown_path, &markdown_import).expect("Markdown import");
        let reimported = SqliteStore::open(&markdown_import).expect("open");
        assert_eq!(
            export_snapshot(&reimported).expect("export").assets,
            snapshot.assets
        );
    }
}
