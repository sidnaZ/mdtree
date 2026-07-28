//! Model-aware semantic chunk and embedding persistence.

#![allow(clippy::missing_errors_doc)]

use std::collections::BTreeMap;
use std::str::FromStr;

use mdtree_core::{
    EmbeddingMetric, EmbeddingProfile, NodeHash, NodeId, Section, SemanticChunk, SemanticChunkWork,
    SemanticIndexCoverage, SemanticIndexStatus, SemanticSource, SemanticWriteOutcome,
};
use rusqlite::{params, OptionalExtension, Transaction};

use crate::{SqliteStore, StoreError};

impl SqliteStore {
    /// Returns every canonical node and current section in stable tree order.
    pub fn semantic_sources(&self) -> Result<Vec<SemanticSource>, StoreError> {
        let root = self.root()?;
        let nodes = self.subtree(root.id())?;
        let mut sections_by_node = BTreeMap::<NodeId, Vec<Section>>::new();
        let mut statement = self.connection().prepare(
            "SELECT id,node_id,parent_section_id,heading,heading_level,anchor,
                    start_byte,end_byte,content,content_hash,position
             FROM sections ORDER BY node_id,position",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<u8>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, Vec<u8>>(9)?,
                row.get::<_, u32>(10)?,
            ))
        })?;
        for row in rows {
            let (
                id,
                node_id,
                parent_section_id,
                heading,
                heading_level,
                anchor,
                start_byte,
                end_byte,
                content,
                content_hash,
                position,
            ) = row?;
            let node_id = parse_id(&node_id)?;
            sections_by_node.entry(node_id).or_default().push(Section {
                id: parse_id(&id)?,
                node_id,
                parent_section_id: parent_section_id.as_deref().map(parse_id).transpose()?,
                heading,
                heading_level,
                anchor,
                start_byte: nonnegative(start_byte, "section start_byte")?,
                end_byte: nonnegative(end_byte, "section end_byte")?,
                content,
                content_hash: parse_hash(&content_hash)?,
                position,
            });
        }
        nodes
            .into_iter()
            .map(|depth| {
                let node = depth.node;
                let sections = sections_by_node.remove(&node.id()).unwrap_or_default();
                Ok(SemanticSource { node, sections })
            })
            .collect()
    }

    /// Returns the active profile, exact lifecycle counts, and semantic revision.
    pub fn semantic_index_status(&self) -> Result<SemanticIndexStatus, StoreError> {
        let (profile_id, revision): (Option<i64>, i64) = self.connection().query_row(
            "SELECT active_profile_id,revision FROM semantic_index WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let Some(profile_id) = profile_id else {
            return Ok(SemanticIndexStatus {
                profile: None,
                coverage: SemanticIndexCoverage {
                    total: 0,
                    pending: 0,
                    processing: 0,
                    ready: 0,
                    failed: 0,
                    revision: nonnegative(revision, "semantic revision")?,
                },
            });
        };
        let profile = query_profile(self.connection(), profile_id)?;
        let mut coverage = SemanticIndexCoverage {
            total: 0,
            pending: 0,
            processing: 0,
            ready: 0,
            failed: 0,
            revision: nonnegative(revision, "semantic revision")?,
        };
        let mut statement = self.connection().prepare(
            "SELECT state,COUNT(*) FROM semantic_chunks
             WHERE profile_id=?1 GROUP BY state",
        )?;
        let rows = statement.query_map([profile_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (state, count) = row?;
            let count = nonnegative(count, "semantic coverage count")?;
            coverage.total = coverage.total.saturating_add(count);
            match state.as_str() {
                "pending" => coverage.pending = count,
                "processing" => coverage.processing = count,
                "ready" => coverage.ready = count,
                "failed" => coverage.failed = count,
                _ => return Err(StoreError::InvalidData(format!("semantic state {state}"))),
            }
        }
        Ok(SemanticIndexStatus {
            profile: Some(profile),
            coverage,
        })
    }

    /// Activates a compatible profile without generating or deleting chunks.
    pub fn activate_semantic_profile(
        &mut self,
        profile: &EmbeddingProfile,
    ) -> Result<(), StoreError> {
        validate_profile(profile)?;
        let transaction = self.connection_mut().transaction()?;
        let profile_id = ensure_profile(&transaction, profile)?;
        set_active_profile(&transaction, profile_id)?;
        transaction.commit()?;
        Ok(())
    }

    /// Atomically replaces one node's chunks for a profile.
    ///
    /// A ready vector with an identical complete input hash and compatible
    /// profile is reused; every other chunk becomes pending.
    pub fn replace_node_semantic_chunks(
        &mut self,
        node_id: NodeId,
        profile: &EmbeddingProfile,
        chunks: &[SemanticChunk],
        updated_at: u64,
    ) -> Result<(), StoreError> {
        validate_profile(profile)?;
        if chunks.iter().any(|chunk| chunk.node_id != node_id) {
            return Err(StoreError::InvalidData(
                "semantic chunk belongs to another node".into(),
            ));
        }
        let updated_at = sqlite_integer(updated_at, "semantic updated_at")?;
        let expected_bytes = embedding_bytes(profile.dimensions)?;
        let transaction = self.connection_mut().transaction()?;
        let node_exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM nodes WHERE id=?1)",
            [node_id.to_string()],
            |row| row.get(0),
        )?;
        if !node_exists {
            return Err(StoreError::NotFound(node_id.to_string()));
        }
        let profile_id = ensure_profile(&transaction, profile)?;
        set_active_profile(&transaction, profile_id)?;

        let node_id_text = node_id.to_string();
        let mut prepared = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            let section_node: Option<String> = transaction
                .query_row(
                    "SELECT node_id FROM sections WHERE id=?1",
                    [chunk.section_id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            if section_node.as_deref() != Some(node_id_text.as_str()) {
                return Err(StoreError::InvalidData(format!(
                    "semantic section {} does not belong to node {node_id}",
                    chunk.section_id
                )));
            }
            let reused = transaction
                .query_row(
                    "SELECT embedding FROM semantic_chunks
                     WHERE profile_id=?1 AND input_hash=?2 AND state='ready'
                     LIMIT 1",
                    params![profile_id, chunk.input_hash.as_bytes().as_slice()],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()?
                .filter(|bytes| bytes.len() == expected_bytes);
            prepared.push((chunk, reused));
        }

        transaction.execute(
            "DELETE FROM semantic_chunks WHERE node_id=?1 AND profile_id=?2",
            params![node_id.to_string(), profile_id],
        )?;
        for (chunk, reused) in prepared {
            let position = i64::from(chunk.position);
            let start_byte = sqlite_integer(chunk.start_byte, "semantic start_byte")?;
            let end_byte = sqlite_integer(chunk.end_byte, "semantic end_byte")?;
            match reused {
                Some(embedding) => {
                    transaction.execute(
                        "INSERT INTO semantic_chunks(
                            profile_id,node_id,section_id,position,start_byte,end_byte,input,
                            input_hash,state,embedding,attempts,last_error,updated_at
                         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'ready',?9,0,NULL,?10)",
                        params![
                            profile_id,
                            node_id.to_string(),
                            chunk.section_id.to_string(),
                            position,
                            start_byte,
                            end_byte,
                            &chunk.input,
                            chunk.input_hash.as_bytes().as_slice(),
                            embedding,
                            updated_at
                        ],
                    )?;
                }
                None => {
                    transaction.execute(
                        "INSERT INTO semantic_chunks(
                            profile_id,node_id,section_id,position,start_byte,end_byte,input,
                            input_hash,state,embedding,attempts,last_error,updated_at
                         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'pending',NULL,0,NULL,?9)",
                        params![
                            profile_id,
                            node_id.to_string(),
                            chunk.section_id.to_string(),
                            position,
                            start_byte,
                            end_byte,
                            &chunk.input,
                            chunk.input_hash.as_bytes().as_slice(),
                            updated_at
                        ],
                    )?;
                }
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Claims at most `limit` pending chunks in stable row order.
    pub fn claim_semantic_chunks(
        &mut self,
        limit: u32,
        updated_at: u64,
    ) -> Result<Vec<SemanticChunkWork>, StoreError> {
        if limit == 0 {
            return Err(StoreError::InvalidData(
                "semantic claim limit must be positive".into(),
            ));
        }
        let updated_at = sqlite_integer(updated_at, "semantic updated_at")?;
        let transaction = self.connection_mut().transaction()?;
        let Some(profile_id) = active_profile_id(&transaction)? else {
            return Err(StoreError::InvalidData(
                "semantic profile is not configured".into(),
            ));
        };
        let rows = {
            let mut statement = transaction.prepare(
                "SELECT id,node_id,section_id,position,input,input_hash,attempts
                 FROM semantic_chunks
                 WHERE profile_id=?1 AND state='pending'
                 ORDER BY id LIMIT ?2",
            )?;
            let collected = statement
                .query_map(params![profile_id, i64::from(limit)], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, u32>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Vec<u8>>(5)?,
                        row.get::<_, u32>(6)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            collected
        };
        let mut work = Vec::with_capacity(rows.len());
        for (id, node, section, position, input, hash, attempts) in rows {
            transaction.execute(
                "UPDATE semantic_chunks
                 SET state='processing',attempts=attempts+1,updated_at=?2
                 WHERE id=?1 AND state='pending'",
                params![id, updated_at],
            )?;
            work.push(SemanticChunkWork {
                node_id: parse_id(&node)?,
                section_id: parse_id(&section)?,
                position,
                input,
                input_hash: parse_hash(&hash)?,
                attempt: attempts.saturating_add(1),
            });
        }
        transaction.commit()?;
        Ok(work)
    }

    /// Stores a finite vector only when the claimed input is still current.
    pub fn store_semantic_embedding(
        &mut self,
        work: &SemanticChunkWork,
        embedding: &[f32],
        updated_at: u64,
    ) -> Result<SemanticWriteOutcome, StoreError> {
        if embedding.iter().any(|value| !value.is_finite()) {
            return Err(StoreError::InvalidData(
                "semantic embedding contains a non-finite value".into(),
            ));
        }
        let updated_at = sqlite_integer(updated_at, "semantic updated_at")?;
        let transaction = self.connection_mut().transaction()?;
        let Some(profile_id) = active_profile_id(&transaction)? else {
            return Ok(SemanticWriteOutcome::Stale);
        };
        let dimensions: u32 = transaction.query_row(
            "SELECT dimensions FROM semantic_profiles WHERE id=?1",
            [profile_id],
            |row| row.get(0),
        )?;
        if usize::try_from(dimensions).ok() != Some(embedding.len()) {
            return Err(StoreError::InvalidData(format!(
                "semantic embedding has {} dimensions, expected {dimensions}",
                embedding.len()
            )));
        }
        let bytes = encode_embedding(embedding);
        let changed = transaction.execute(
            "UPDATE semantic_chunks
             SET state='ready',embedding=?6,last_error=NULL,updated_at=?7
             WHERE profile_id=?1 AND section_id=?2 AND position=?3
               AND input_hash=?4 AND state='processing' AND node_id=?5",
            params![
                profile_id,
                work.section_id.to_string(),
                i64::from(work.position),
                work.input_hash.as_bytes().as_slice(),
                work.node_id.to_string(),
                bytes,
                updated_at
            ],
        )?;
        transaction.commit()?;
        Ok(if changed == 1 {
            SemanticWriteOutcome::Stored
        } else {
            SemanticWriteOutcome::Stale
        })
    }

    /// Records a redacted non-empty failure only when claimed work is current.
    pub fn fail_semantic_chunk(
        &mut self,
        work: &SemanticChunkWork,
        error: &str,
        updated_at: u64,
    ) -> Result<SemanticWriteOutcome, StoreError> {
        if error.trim().is_empty() {
            return Err(StoreError::InvalidData(
                "semantic failure detail must not be blank".into(),
            ));
        }
        let updated_at = sqlite_integer(updated_at, "semantic updated_at")?;
        let transaction = self.connection_mut().transaction()?;
        let Some(profile_id) = active_profile_id(&transaction)? else {
            return Ok(SemanticWriteOutcome::Stale);
        };
        let changed = transaction.execute(
            "UPDATE semantic_chunks
             SET state='failed',embedding=NULL,last_error=?6,updated_at=?7
             WHERE profile_id=?1 AND section_id=?2 AND position=?3
               AND input_hash=?4 AND state='processing' AND node_id=?5",
            params![
                profile_id,
                work.section_id.to_string(),
                i64::from(work.position),
                work.input_hash.as_bytes().as_slice(),
                work.node_id.to_string(),
                error,
                updated_at
            ],
        )?;
        transaction.commit()?;
        Ok(if changed == 1 {
            SemanticWriteOutcome::Stored
        } else {
            SemanticWriteOutcome::Stale
        })
    }

    /// Requeues every failed chunk in the active profile.
    pub fn retry_failed_semantic_chunks(&mut self, updated_at: u64) -> Result<u64, StoreError> {
        let updated_at = sqlite_integer(updated_at, "semantic updated_at")?;
        let transaction = self.connection_mut().transaction()?;
        let Some(profile_id) = active_profile_id(&transaction)? else {
            return Ok(0);
        };
        let changed = transaction.execute(
            "UPDATE semantic_chunks
             SET state='pending',embedding=NULL,last_error=NULL,updated_at=?2
             WHERE profile_id=?1 AND state='failed'",
            params![profile_id, updated_at],
        )?;
        transaction.commit()?;
        u64::try_from(changed)
            .map_err(|_| StoreError::InvalidData("semantic retry count overflow".into()))
    }

    /// Requeues work left in processing state by an interrupted indexer.
    pub fn recover_processing_semantic_chunks(
        &mut self,
        updated_at: u64,
    ) -> Result<u64, StoreError> {
        let updated_at = sqlite_integer(updated_at, "semantic updated_at")?;
        let transaction = self.connection_mut().transaction()?;
        let Some(profile_id) = active_profile_id(&transaction)? else {
            return Ok(0);
        };
        let changed = transaction.execute(
            "UPDATE semantic_chunks
             SET state='pending',embedding=NULL,last_error=NULL,updated_at=?2
             WHERE profile_id=?1 AND state='processing'",
            params![profile_id, updated_at],
        )?;
        transaction.commit()?;
        u64::try_from(changed)
            .map_err(|_| StoreError::InvalidData("semantic recovery count overflow".into()))
    }

    /// Deletes all derived semantic chunks and deactivates the profile.
    pub fn clear_semantic_index(&mut self) -> Result<(), StoreError> {
        let transaction = self.connection_mut().transaction()?;
        transaction.execute("DELETE FROM semantic_chunks", [])?;
        transaction.execute(
            "UPDATE semantic_index
             SET active_profile_id=NULL,revision=revision+1
             WHERE singleton=1 AND active_profile_id IS NOT NULL",
            [],
        )?;
        transaction.commit()?;
        Ok(())
    }
}

fn validate_profile(profile: &EmbeddingProfile) -> Result<(), StoreError> {
    if profile.provider.trim().is_empty()
        || profile.model.trim().is_empty()
        || profile.dimensions == 0
        || profile.input_format_version == 0
    {
        return Err(StoreError::InvalidData(
            "semantic profile fields must be non-empty and positive".into(),
        ));
    }
    let _ = embedding_bytes(profile.dimensions)?;
    Ok(())
}

fn ensure_profile(
    transaction: &Transaction<'_>,
    profile: &EmbeddingProfile,
) -> Result<i64, StoreError> {
    transaction.execute(
        "INSERT OR IGNORE INTO semantic_profiles(
            provider,model,dimensions,metric,input_format_version
         ) VALUES (?1,?2,?3,?4,?5)",
        params![
            &profile.provider,
            &profile.model,
            i64::from(profile.dimensions),
            profile.metric.as_str(),
            i64::from(profile.input_format_version)
        ],
    )?;
    transaction
        .query_row(
            "SELECT id FROM semantic_profiles
             WHERE provider=?1 AND model=?2 AND dimensions=?3
               AND metric=?4 AND input_format_version=?5",
            params![
                &profile.provider,
                &profile.model,
                i64::from(profile.dimensions),
                profile.metric.as_str(),
                i64::from(profile.input_format_version)
            ],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn set_active_profile(transaction: &Transaction<'_>, profile_id: i64) -> Result<(), StoreError> {
    transaction.execute(
        "UPDATE semantic_index
         SET active_profile_id=?1,revision=revision+1
         WHERE singleton=1 AND active_profile_id IS NOT ?1",
        [profile_id],
    )?;
    Ok(())
}

fn active_profile_id(transaction: &Transaction<'_>) -> Result<Option<i64>, StoreError> {
    transaction
        .query_row(
            "SELECT active_profile_id FROM semantic_index WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn query_profile(
    connection: &rusqlite::Connection,
    profile_id: i64,
) -> Result<EmbeddingProfile, StoreError> {
    connection
        .query_row(
            "SELECT provider,model,dimensions,metric,input_format_version
             FROM semantic_profiles WHERE id=?1",
            [profile_id],
            |row| {
                let metric = match row.get::<_, String>(3)?.as_str() {
                    "cosine" => EmbeddingMetric::Cosine,
                    value => {
                        return Err(rusqlite::Error::FromSqlConversionFailure(
                            3,
                            rusqlite::types::Type::Text,
                            format!("unknown embedding metric {value}").into(),
                        ));
                    }
                };
                Ok(EmbeddingProfile {
                    provider: row.get(0)?,
                    model: row.get(1)?,
                    dimensions: row.get(2)?,
                    metric,
                    input_format_version: row.get(4)?,
                })
            },
        )
        .map_err(Into::into)
}

fn encode_embedding(values: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(values.len().saturating_mul(size_of::<f32>()));
    for value in values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

pub(crate) fn embedding_blob_is_valid(bytes: &[u8], dimensions: u32) -> bool {
    embedding_bytes(dimensions).is_ok_and(|expected| expected == bytes.len())
        && bytes.chunks_exact(size_of::<f32>()).all(|chunk| {
            f32::from_le_bytes(chunk.try_into().expect("exact float chunk")).is_finite()
        })
}

pub(crate) fn decode_embedding(bytes: &[u8], dimensions: u32) -> Option<Vec<f32>> {
    embedding_blob_is_valid(bytes, dimensions).then(|| {
        bytes
            .chunks_exact(size_of::<f32>())
            .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("exact float chunk")))
            .collect()
    })
}

fn embedding_bytes(dimensions: u32) -> Result<usize, StoreError> {
    usize::try_from(dimensions)
        .ok()
        .and_then(|value| value.checked_mul(size_of::<f32>()))
        .ok_or_else(|| StoreError::InvalidData("semantic dimensions exceed platform range".into()))
}

fn parse_hash(bytes: &[u8]) -> Result<NodeHash, StoreError> {
    let array: [u8; 32] = bytes
        .try_into()
        .map_err(|_| StoreError::InvalidData("semantic input hash length".into()))?;
    Ok(NodeHash::new(array))
}

fn parse_id(value: &str) -> Result<NodeId, StoreError> {
    NodeId::from_str(value).map_err(|error| StoreError::InvalidData(error.to_string()))
}

fn sqlite_integer(value: u64, field: &str) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::InvalidData(format!("{field} exceeds SQLite")))
}

fn nonnegative(value: i64, field: &str) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| StoreError::InvalidData(format!("negative {field}")))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::str::FromStr;

    use mdtree_core::{
        hash_content, hash_revision, EmbeddingMetric, EmbeddingProfile, Node, NodeFields, NodeId,
        NodeMetadata, RevisionHashInput, SemanticChunk, SemanticIndexState, SemanticWriteOutcome,
        Slug,
    };
    use tempfile::{tempdir, TempDir};

    use crate::{backup_workspace, create_workspace, SqliteStore};

    struct Fixture {
        _directory: TempDir,
        path: PathBuf,
        store: SqliteStore,
    }

    fn workspace() -> Fixture {
        let directory = tempdir().expect("directory");
        let path = directory.path().join("semantic.mdtree");
        let id = NodeId::from_str("01JZ8Q5CWPN8T7KPN5A1V9B6XM").expect("ID");
        let slug = Slug::from_str("project").expect("slug");
        let metadata = NodeMetadata::new("Project");
        let markdown_content = "# Project\nSemantic search.\n".to_owned();
        let content_hash = hash_content(&markdown_content);
        let revision_hash = hash_revision(RevisionHashInput {
            node_id: id,
            parent_id: None,
            slug: &slug,
            metadata: &metadata,
            markdown_content: &markdown_content,
            sibling_order: 0,
        })
        .expect("revision hash");
        let root = Node::new(
            NodeFields {
                id,
                slug,
                metadata,
                markdown_content,
                sibling_order: 0,
                version: 1,
                content_hash,
                revision_hash,
                created_at: 1,
                updated_at: 1,
            },
            None,
        )
        .expect("root");
        let connection = create_workspace(&path, "Semantic", &root).expect("workspace");
        Fixture {
            _directory: directory,
            path,
            store: SqliteStore::new(connection),
        }
    }

    fn profile(model: &str, dimensions: u32) -> EmbeddingProfile {
        EmbeddingProfile {
            provider: "ollama".into(),
            model: model.into(),
            dimensions,
            metric: EmbeddingMetric::Cosine,
            input_format_version: 1,
        }
    }

    fn root_section(store: &SqliteStore, root: NodeId) -> (NodeId, u64, u64) {
        let (id, start, end): (String, i64, i64) = store
            .connection()
            .query_row(
                "SELECT id,start_byte,end_byte FROM sections
                 WHERE node_id=?1 ORDER BY position LIMIT 1",
                [root.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("section");
        (
            NodeId::from_str(&id).expect("section ID"),
            u64::try_from(start).expect("start"),
            u64::try_from(end).expect("end"),
        )
    }

    #[test]
    fn lifecycle_rejects_stale_invalid_and_incompatible_vectors() {
        let mut fixture = workspace();
        let root = fixture.store.root().expect("root").id();
        let (section_id, start_byte, end_byte) = root_section(&fixture.store, root);
        let chunk = SemanticChunk {
            node_id: root,
            section_id,
            position: 0,
            start_byte,
            end_byte,
            input: "input".into(),
            input_hash: mdtree_core::hash_embedding_input(&profile("embed", 3), "input"),
        };
        fixture
            .store
            .replace_node_semantic_chunks(root, &profile("embed", 3), &[chunk], 1)
            .expect("replace");
        assert_eq!(
            fixture
                .store
                .semantic_index_status()
                .expect("status")
                .coverage
                .state(),
            SemanticIndexState::Pending
        );
        let work = fixture
            .store
            .claim_semantic_chunks(1, 2)
            .expect("claim")
            .pop()
            .expect("work");
        assert!(fixture
            .store
            .store_semantic_embedding(&work, &[f32::NAN, 0.0, 1.0], 3)
            .is_err());
        assert!(fixture
            .store
            .store_semantic_embedding(&work, &[0.0, 1.0], 3)
            .is_err());
        assert_eq!(
            fixture
                .store
                .store_semantic_embedding(&work, &[0.0, 1.0, 0.0], 3)
                .expect("store"),
            SemanticWriteOutcome::Stored
        );
        let stored: Vec<u8> = fixture
            .store
            .connection()
            .query_row(
                "SELECT embedding FROM semantic_chunks WHERE state='ready'",
                [],
                |row| row.get(0),
            )
            .expect("embedding BLOB");
        assert_eq!(
            stored,
            [0.0_f32, 1.0, 0.0]
                .into_iter()
                .flat_map(f32::to_le_bytes)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            fixture
                .store
                .store_semantic_embedding(&work, &[0.0, 1.0, 0.0], 4)
                .expect("repeat"),
            SemanticWriteOutcome::Stale
        );
        assert_eq!(
            fixture
                .store
                .semantic_index_status()
                .expect("status")
                .coverage
                .state(),
            SemanticIndexState::Ready
        );
    }

    #[test]
    fn failure_retry_clear_and_profile_switch_are_explicit() {
        let mut fixture = workspace();
        let root = fixture.store.root().expect("root").id();
        let (section_id, _, _) = root_section(&fixture.store, root);
        let original_profile = profile("embed", 2);
        let input = "input".to_owned();
        let chunk = SemanticChunk {
            node_id: root,
            section_id,
            position: 0,
            start_byte: 0,
            end_byte: 1,
            input_hash: mdtree_core::hash_embedding_input(&original_profile, &input),
            input,
        };
        fixture
            .store
            .replace_node_semantic_chunks(root, &original_profile, &[chunk], 1)
            .expect("replace");
        let work = fixture.store.claim_semantic_chunks(1, 2).expect("claim")[0].clone();
        assert_eq!(
            fixture
                .store
                .fail_semantic_chunk(&work, "provider unavailable", 3)
                .expect("fail"),
            SemanticWriteOutcome::Stored
        );
        assert_eq!(
            fixture
                .store
                .semantic_index_status()
                .expect("status")
                .coverage
                .state(),
            SemanticIndexState::Failed
        );
        assert_eq!(
            fixture
                .store
                .retry_failed_semantic_chunks(4)
                .expect("retry"),
            1
        );
        fixture
            .store
            .activate_semantic_profile(&profile("other", 4))
            .expect("switch");
        assert_eq!(
            fixture
                .store
                .semantic_index_status()
                .expect("status")
                .coverage
                .total,
            0
        );
        fixture.store.clear_semantic_index().expect("clear");
        assert!(fixture
            .store
            .semantic_index_status()
            .expect("status")
            .profile
            .is_none());
    }

    #[test]
    fn ready_vector_is_reused_only_for_an_identical_profile_and_input_hash() {
        let mut fixture = workspace();
        let root = fixture.store.root().expect("root").id();
        let (section_id, _, _) = root_section(&fixture.store, root);
        let profile = profile("embed", 2);
        let chunk = SemanticChunk {
            node_id: root,
            section_id,
            position: 0,
            start_byte: 0,
            end_byte: 1,
            input: "same".into(),
            input_hash: mdtree_core::hash_embedding_input(&profile, "same"),
        };
        fixture
            .store
            .replace_node_semantic_chunks(root, &profile, std::slice::from_ref(&chunk), 1)
            .expect("replace");
        let work = fixture.store.claim_semantic_chunks(1, 2).expect("claim")[0].clone();
        fixture
            .store
            .store_semantic_embedding(&work, &[1.0, 0.0], 3)
            .expect("store");
        fixture
            .store
            .replace_node_semantic_chunks(root, &profile, &[chunk], 4)
            .expect("replace identical");
        assert_eq!(
            fixture
                .store
                .semantic_index_status()
                .expect("status")
                .coverage
                .ready,
            1
        );

        fixture
            .store
            .connection()
            .execute(
                "UPDATE semantic_chunks SET embedding=?1 WHERE state='ready'",
                [[f32::NAN.to_le_bytes(), 0.0_f32.to_le_bytes()].concat()],
            )
            .expect("inject non-finite vector");
        assert!(fixture
            .store
            .validate_integrity()
            .expect("integrity")
            .findings
            .iter()
            .any(|finding| finding.code == "semantic_embedding"));
    }

    #[test]
    fn deleting_a_section_cascades_chunks_and_advances_semantic_revision() {
        let mut fixture = workspace();
        let root = fixture.store.root().expect("root").id();
        let (section_id, _, _) = root_section(&fixture.store, root);
        let profile = profile("embed", 2);
        let chunk = SemanticChunk {
            node_id: root,
            section_id,
            position: 0,
            start_byte: 0,
            end_byte: 1,
            input: "input".into(),
            input_hash: mdtree_core::hash_embedding_input(&profile, "input"),
        };
        fixture
            .store
            .replace_node_semantic_chunks(root, &profile, &[chunk], 1)
            .expect("replace");
        let before = fixture
            .store
            .semantic_index_status()
            .expect("status")
            .coverage
            .revision;
        fixture
            .store
            .connection()
            .execute("DELETE FROM sections WHERE id=?1", [section_id.to_string()])
            .expect("delete");
        let after = fixture
            .store
            .semantic_index_status()
            .expect("status")
            .coverage;
        assert_eq!(after.total, 0);
        assert!(after.revision > before);
    }

    #[test]
    fn derived_rebuild_explicitly_invalidates_profile_chunks() {
        let mut fixture = workspace();
        let root = fixture.store.root().expect("root").id();
        let (section_id, _, _) = root_section(&fixture.store, root);
        let profile = profile("embed", 2);
        let chunk = SemanticChunk {
            node_id: root,
            section_id,
            position: 0,
            start_byte: 0,
            end_byte: 1,
            input: "input".into(),
            input_hash: mdtree_core::hash_embedding_input(&profile, "input"),
        };
        fixture
            .store
            .replace_node_semantic_chunks(root, &profile, &[chunk], 1)
            .expect("replace");
        let before = fixture
            .store
            .semantic_index_status()
            .expect("status")
            .coverage
            .revision;
        fixture
            .store
            .rebuild_derived(&mdtree_core::SequentialUlidGenerator::new(500))
            .expect("rebuild");
        let status = fixture.store.semantic_index_status().expect("status");
        assert_eq!(status.coverage.total, 0);
        assert!(status.coverage.revision > before);
        assert_eq!(status.profile, Some(profile));
    }

    #[test]
    fn online_backup_preserves_profile_chunks_and_vectors() {
        let mut fixture = workspace();
        let root = fixture.store.root().expect("root").id();
        let (section_id, _, _) = root_section(&fixture.store, root);
        let profile = profile("embed", 2);
        let chunk = SemanticChunk {
            node_id: root,
            section_id,
            position: 0,
            start_byte: 0,
            end_byte: 1,
            input: "input".into(),
            input_hash: mdtree_core::hash_embedding_input(&profile, "input"),
        };
        fixture
            .store
            .replace_node_semantic_chunks(root, &profile, &[chunk], 1)
            .expect("replace");
        let work = fixture.store.claim_semantic_chunks(1, 2).expect("claim")[0].clone();
        fixture
            .store
            .store_semantic_embedding(&work, &[1.0, 0.0], 3)
            .expect("ready");

        let backup = fixture
            .path
            .parent()
            .expect("workspace parent")
            .join("semantic-backup.mdtree");
        backup_workspace(&fixture.store, &backup).expect("backup");
        let backed_up = SqliteStore::open(&backup).expect("open backup");
        let status = backed_up.semantic_index_status().expect("backup status");
        assert_eq!(status.profile, Some(profile));
        assert_eq!(status.coverage.ready, 1);
        let embedding: Vec<u8> = backed_up
            .connection()
            .query_row(
                "SELECT embedding FROM semantic_chunks WHERE state='ready'",
                [],
                |row| row.get(0),
            )
            .expect("backup embedding");
        assert_eq!(
            embedding,
            [1.0_f32, 0.0]
                .into_iter()
                .flat_map(f32::to_le_bytes)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn invalid_node_identity_and_zero_claim_limit_are_rejected() {
        let mut fixture = workspace();
        assert!(fixture.store.claim_semantic_chunks(0, 1).is_err());
        let unknown = NodeId::from_str("01JZ8Q5CWPN8T7KPN5A1V9B6XZ").expect("ID");
        assert!(fixture
            .store
            .replace_node_semantic_chunks(unknown, &profile("embed", 2), &[], 1)
            .is_err());
    }

    #[test]
    fn failed_chunk_replacement_rolls_back_the_previous_ready_index() {
        let mut fixture = workspace();
        let root = fixture.store.root().expect("root").id();
        let (section_id, _, _) = root_section(&fixture.store, root);
        let profile = profile("embed", 2);
        let valid = SemanticChunk {
            node_id: root,
            section_id,
            position: 0,
            start_byte: 0,
            end_byte: 1,
            input: "valid".into(),
            input_hash: mdtree_core::hash_embedding_input(&profile, "valid"),
        };
        fixture
            .store
            .replace_node_semantic_chunks(root, &profile, &[valid], 1)
            .expect("replace");
        let work = fixture.store.claim_semantic_chunks(1, 2).expect("claim")[0].clone();
        fixture
            .store
            .store_semantic_embedding(&work, &[1.0, 0.0], 3)
            .expect("ready");

        let invalid = SemanticChunk {
            node_id: root,
            section_id: NodeId::from_str("01JZ8Q5CWPN8T7KPN5A1V9B6XZ").expect("ID"),
            position: 0,
            start_byte: 0,
            end_byte: 1,
            input: "invalid".into(),
            input_hash: mdtree_core::hash_embedding_input(&profile, "invalid"),
        };
        assert!(fixture
            .store
            .replace_node_semantic_chunks(root, &profile, &[invalid], 4)
            .is_err());
        let status = fixture.store.semantic_index_status().expect("status");
        assert_eq!(status.coverage.ready, 1);
        assert_eq!(status.coverage.total, 1);
    }
}
