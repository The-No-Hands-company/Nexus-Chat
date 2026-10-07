//! Full-text search — supports two backends selected at runtime:
//!
//! * **MeiliSearch** — used in full / production mode (`NEXUS_SEARCH_URL` is set).
//! * **Tantivy**     — embedded Rust search engine used in lite mode (no external process).
//! * **Disabled**    — no-op fallback (returns empty results).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── Public result types ───────────────────────────────────────────────────────

/// Message document indexed for full-text search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageDocument {
    pub id: String,
    pub channel_id: String,
    pub server_id: Option<String>,
    pub author_id: String,
    pub author_username: String,
    pub content: String,
    pub has_attachments: bool,
    pub has_embeds: bool,
    /// Unix timestamp (seconds) for range filtering / sorting.
    pub created_at: i64,
}

/// Server document indexed for discovery search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerDocument {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub member_count: u32,
}

/// Unified search result returned by all backends.
#[derive(Debug, Default)]
pub struct NexusSearchResults {
    pub hits: Vec<MessageDocument>,
    /// Estimated total matches (may be `None` for large tantivy indexes).
    pub total_hits: Option<usize>,
}

// ── Backend internals ─────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct TantivyFields {
    id: tantivy::schema::Field,
    channel_id: tantivy::schema::Field,
    server_id: tantivy::schema::Field,
    author_id: tantivy::schema::Field,
    author_username: tantivy::schema::Field,
    content: tantivy::schema::Field,
    created_at: tantivy::schema::Field,
}

struct TantivyState {
    index: tantivy::Index,
    writer: std::sync::Mutex<tantivy::IndexWriter>,
    reader: tantivy::IndexReader,
    fields: TantivyFields,
}

// SAFETY: Index, IndexReader are Send+Sync; Mutex<IndexWriter> is Send+Sync.
unsafe impl Send for TantivyState {}
unsafe impl Sync for TantivyState {}

enum SearchBackend {
    Meilisearch(meilisearch_sdk::client::Client),
    Tantivy(std::sync::Arc<TantivyState>),
    Disabled,
}

// ── SearchClient ──────────────────────────────────────────────────────────────

pub struct SearchClient {
    backend: SearchBackend,
}

impl Clone for SearchClient {
    fn clone(&self) -> Self {
        match &self.backend {
            SearchBackend::Meilisearch(c) => Self {
                backend: SearchBackend::Meilisearch(c.clone()),
            },
            SearchBackend::Tantivy(s) => Self {
                backend: SearchBackend::Tantivy(s.clone()),
            },
            SearchBackend::Disabled => Self {
                backend: SearchBackend::Disabled,
            },
        }
    }
}

impl SearchClient {
    // ── Constructors ──────────────────────────────────────────────────────────

    /// Connect to a MeiliSearch instance.
    pub fn new(url: &str, api_key: &str) -> Self {
        Self {
            backend: SearchBackend::Meilisearch(
                meilisearch_sdk::client::Client::new(url, Some(api_key))
                    .expect("Failed to create MeiliSearch client"),
            ),
        }
    }

    /// Open (or create) an embedded Tantivy index in `<data_dir>/search/`.
    pub fn new_tantivy(data_dir: &str) -> Result<Self> {
        use tantivy::schema::{FAST, STORED, STRING, TEXT};
        use tantivy::{Index, ReloadPolicy};

        let search_dir = std::path::Path::new(data_dir).join("search");
        std::fs::create_dir_all(&search_dir)
            .with_context(|| format!("Cannot create tantivy dir: {}", search_dir.display()))?;

        let mut b = tantivy::schema::Schema::builder();
        let id = b.add_text_field("id", STORED);
        let channel_id = b.add_text_field("channel_id", STORED | STRING);
        let server_id = b.add_text_field("server_id", STORED | STRING);
        let author_id = b.add_text_field("author_id", STORED | STRING);
        let author_username = b.add_text_field("author_username", TEXT | STORED);
        let content = b.add_text_field("content", TEXT | STORED);
        let created_at = b.add_i64_field("created_at", FAST | STORED);
        let schema = b.build();

        let fields = TantivyFields {
            id,
            channel_id,
            server_id,
            author_id,
            author_username,
            content,
            created_at,
        };

        let dir = tantivy::directory::MmapDirectory::open(&search_dir)
            .with_context(|| format!("Cannot open tantivy dir: {}", search_dir.display()))?;
        let index =
            Index::open_or_create(dir, schema).context("Failed to open/create tantivy index")?;

        let writer = index
            .writer(50 * 1024 * 1024)
            .context("Failed to create tantivy IndexWriter")?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .context("Failed to create tantivy IndexReader")?;

        tracing::info!("🔍 Tantivy search index at {}", search_dir.display());

        Ok(Self {
            backend: SearchBackend::Tantivy(std::sync::Arc::new(TantivyState {
                index,
                writer: std::sync::Mutex::new(writer),
                reader,
                fields,
            })),
        })
    }

    /// No-op client — all operations are silent no-ops.
    pub fn disabled() -> Self {
        Self {
            backend: SearchBackend::Disabled,
        }
    }

    pub fn is_enabled(&self) -> bool {
        !matches!(self.backend, SearchBackend::Disabled)
    }

    pub fn uses_search_sync_queue(&self) -> bool {
        matches!(self.backend, SearchBackend::Meilisearch(_))
    }

    // ── Bootstrap ─────────────────────────────────────────────────────────────

    pub async fn bootstrap_indexes(&self) -> Result<()> {
        if let SearchBackend::Meilisearch(client) = &self.backend {
            Self::meili_setup_messages(client).await?;
            Self::meili_setup_servers(client).await?;
        }
        Ok(())
    }

    // ── Indexing ──────────────────────────────────────────────────────────────

    pub async fn index_message(&self, doc: MessageDocument) -> Result<()> {
        match &self.backend {
            SearchBackend::Meilisearch(client) => {
                client
                    .index("messages")
                    .add_or_update(&[doc], Some("id"))
                    .await
                    .context("MeiliSearch: index_message failed")?;
            }
            SearchBackend::Tantivy(state) => {
                let state = state.clone();
                tokio::task::spawn_blocking(move || Self::tantivy_add_document(&state, &doc))
                    .await??;
            }
            SearchBackend::Disabled => {}
        }
        Ok(())
    }

    pub async fn sync_message_index(
        &self,
        pool: &sqlx::AnyPool,
        message_id: Uuid,
        doc: MessageDocument,
    ) -> Result<()> {
        if self.uses_search_sync_queue() {
            Self::enqueue_message_index(pool, message_id, &doc).await
        } else {
            self.index_message(doc).await
        }
    }

    pub async fn index_messages_batch(&self, docs: Vec<MessageDocument>) -> Result<()> {
        if docs.is_empty() {
            return Ok(());
        }
        match &self.backend {
            SearchBackend::Meilisearch(client) => {
                client
                    .index("messages")
                    .add_or_update(&docs, Some("id"))
                    .await
                    .context("MeiliSearch: batch index failed")?;
            }
            SearchBackend::Tantivy(state) => {
                let state = state.clone();
                tokio::task::spawn_blocking(move || {
                    let mut writer = state
                        .writer
                        .lock()
                        .map_err(|_| anyhow::anyhow!("tantivy writer lock poisoned"))?;
                    for doc in &docs {
                        writer
                            .add_document(Self::tantivy_build_doc(&state.fields, doc))
                            .context("tantivy add_document failed")?;
                    }
                    writer.commit().context("tantivy commit failed").map(|_| ())
                })
                .await??;
            }
            SearchBackend::Disabled => {}
        }
        Ok(())
    }

    pub async fn delete_message(&self, message_id: Uuid) -> Result<()> {
        let id_str = message_id.to_string();
        match &self.backend {
            SearchBackend::Meilisearch(client) => {
                client
                    .index("messages")
                    .delete_document(&id_str)
                    .await
                    .context("MeiliSearch: delete_message failed")?;
            }
            SearchBackend::Tantivy(state) => {
                let state = state.clone();
                tokio::task::spawn_blocking(move || {
                    let term = tantivy::Term::from_field_text(state.fields.id, &id_str);
                    let mut writer = state
                        .writer
                        .lock()
                        .map_err(|_| anyhow::anyhow!("tantivy writer lock poisoned"))?;
                    writer.delete_term(term);
                    writer
                        .commit()
                        .context("tantivy commit (delete) failed")
                        .map(|_| ())
                })
                .await??;
            }
            SearchBackend::Disabled => {}
        }
        Ok(())
    }

    pub async fn sync_message_delete(&self, pool: &sqlx::AnyPool, message_id: Uuid) -> Result<()> {
        if self.uses_search_sync_queue() {
            Self::enqueue_message_delete(pool, message_id).await
        } else {
            self.delete_message(message_id).await
        }
    }

    // ── Search ────────────────────────────────────────────────────────────────

    pub async fn search_messages(
        &self,
        query: &str,
        server_id: Option<Uuid>,
        channel_id: Option<Uuid>,
        author_id: Option<Uuid>,
        limit: usize,
        offset: usize,
    ) -> Result<NexusSearchResults> {
        match &self.backend {
            SearchBackend::Meilisearch(client) => {
                Self::meili_search(
                    client, query, server_id, channel_id, author_id, limit, offset,
                )
                .await
            }
            SearchBackend::Tantivy(state) => {
                Self::tantivy_search(
                    state.clone(),
                    query.to_string(),
                    server_id,
                    channel_id,
                    author_id,
                    limit,
                    offset,
                )
                .await
            }
            SearchBackend::Disabled => {
                anyhow::bail!("Full-text search is disabled (no search backend configured)")
            }
        }
    }

    // ── Sync queue ────────────────────────────────────────────────────────────

    pub async fn process_sync_queue(&self, pool: &sqlx::AnyPool) -> Result<()> {
        let client = match &self.backend {
            SearchBackend::Meilisearch(c) => c,
            _ => return Ok(()),
        };

        #[derive(sqlx::FromRow)]
        struct QueueRow {
            id: i64,
            operation: String,
            index_name: String,
            document_id: String,
            payload: Option<String>,
        }

        let rows = sqlx::query_as::<_, QueueRow>(
            "SELECT id, operation, index_name, document_id, payload \
             FROM search_sync_queue WHERE processed = false ORDER BY created_at LIMIT 100",
        )
        .fetch_all(pool)
        .await
        .context("Failed to fetch search sync queue")?;

        for row in rows {
            let result: Result<()> = async {
                match row.operation.as_str() {
                    "index" | "update" => {
                        if row.index_name == "messages"
                            && let Some(payload) = row.payload
                        {
                            let doc: MessageDocument = serde_json::from_str(&payload)
                                .context("Failed to deserialise MessageDocument")?;
                            client
                                .index("messages")
                                .add_or_update(&[doc], Some("id"))
                                .await
                                .context("MeiliSearch: queue index failed")?;
                        }
                    }
                    "delete" => {
                        if row.index_name == "messages" {
                            let id = Uuid::parse_str(&row.document_id).context("Invalid UUID")?;
                            client
                                .index("messages")
                                .delete_document(id.to_string())
                                .await
                                .context("MeiliSearch: queue delete failed")?;
                        }
                    }
                    other => tracing::warn!("Unknown search sync operation: {other}"),
                }
                Ok(())
            }
            .await;

            if let Err(e) = result {
                tracing::error!(queue_id = row.id, error_kind = %nexus_common::logsafe::anyhow_kind(&e), "Search sync queue error");
            } else {
                let _ = sqlx::query("UPDATE search_sync_queue SET processed = true WHERE id = $1")
                    .bind(row.id)
                    .execute(pool)
                    .await;
            }
        }
        Ok(())
    }

    pub async fn enqueue_message_index(
        pool: &sqlx::AnyPool,
        message_id: Uuid,
        doc: &MessageDocument,
    ) -> Result<()> {
        let payload = serde_json::to_string(doc).context("Failed to serialise MessageDocument")?;
        sqlx::query(
            "INSERT INTO search_sync_queue (operation, index_name, document_id, payload) \
             VALUES ('index', 'messages', $1, $2)",
        )
        .bind(message_id.to_string())
        .bind(payload)
        .execute(pool)
        .await
        .context("Failed to enqueue message for search indexing")?;
        Ok(())
    }

    pub async fn enqueue_message_delete(pool: &sqlx::AnyPool, message_id: Uuid) -> Result<()> {
        sqlx::query(
            "INSERT INTO search_sync_queue (operation, index_name, document_id) \
             VALUES ('delete', 'messages', $1)",
        )
        .bind(message_id.to_string())
        .execute(pool)
        .await
        .context("Failed to enqueue message delete for search")?;
        Ok(())
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn tantivy_build_doc(f: &TantivyFields, doc: &MessageDocument) -> tantivy::TantivyDocument {
        let mut d = tantivy::TantivyDocument::default();
        d.add_text(f.id, &doc.id);
        d.add_text(f.channel_id, &doc.channel_id);
        if let Some(sid) = &doc.server_id {
            d.add_text(f.server_id, sid);
        }
        d.add_text(f.author_id, &doc.author_id);
        d.add_text(f.author_username, &doc.author_username);
        d.add_text(f.content, &doc.content);
        d.add_i64(f.created_at, doc.created_at);
        d
    }

    fn tantivy_add_document(state: &TantivyState, doc: &MessageDocument) -> Result<()> {
        let tantivy_doc = Self::tantivy_build_doc(&state.fields, doc);
        let mut writer = state
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("tantivy writer lock poisoned"))?;
        writer
            .add_document(tantivy_doc)
            .context("tantivy add_document failed")?;
        writer.commit().context("tantivy commit failed").map(|_| ())
    }

    async fn tantivy_search(
        state: std::sync::Arc<TantivyState>,
        query: String,
        server_id: Option<Uuid>,
        channel_id: Option<Uuid>,
        author_id: Option<Uuid>,
        limit: usize,
        offset: usize,
    ) -> Result<NexusSearchResults> {
        tokio::task::spawn_blocking(move || {
            use tantivy::collector::TopDocs;
            use tantivy::query::QueryParser;
            use tantivy::schema::OwnedValue;

            let f = &state.fields;
            let searcher = state.reader.searcher();

            let qp = QueryParser::for_index(&state.index, vec![f.content, f.author_username]);
            let parsed = qp
                .parse_query(&query)
                .map_err(|e| anyhow::anyhow!("Tantivy parse error: {:?}", e))?;

            let top_docs = searcher
                .search(&parsed, &TopDocs::with_limit((limit + offset).max(1)))
                .context("Tantivy search failed")?;

            let mut hits = Vec::<MessageDocument>::new();
            for (_score, addr) in top_docs.into_iter().skip(offset) {
                let doc = searcher
                    .doc::<tantivy::TantivyDocument>(addr)
                    .context("Tantivy: failed to retrieve doc")?;

                let str_val = |field| -> String {
                    doc.get_first(field)
                        .and_then(|v| {
                            if let OwnedValue::Str(s) = v {
                                Some(s.as_str())
                            } else {
                                None
                            }
                        })
                        .unwrap_or("")
                        .to_string()
                };

                let msg_server_id = {
                    let s = str_val(f.server_id);
                    if s.is_empty() { None } else { Some(s) }
                };
                let msg_channel_id = str_val(f.channel_id);
                let msg_author_id = str_val(f.author_id);

                // Post-retrieval filter (avoids full collector complexity)
                if let Some(sid) = &server_id
                    && msg_server_id.as_deref() != Some(sid.to_string().as_str())
                {
                    continue;
                }
                if let Some(cid) = &channel_id
                    && msg_channel_id != cid.to_string()
                {
                    continue;
                }
                if let Some(aid) = &author_id
                    && msg_author_id != aid.to_string()
                {
                    continue;
                }

                hits.push(MessageDocument {
                    id: str_val(f.id),
                    channel_id: msg_channel_id,
                    server_id: msg_server_id,
                    author_id: msg_author_id,
                    author_username: str_val(f.author_username),
                    content: str_val(f.content),
                    has_attachments: false,
                    has_embeds: false,
                    created_at: doc
                        .get_first(f.created_at)
                        .and_then(|v| {
                            if let OwnedValue::I64(n) = v {
                                Some(*n)
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0),
                });
            }

            let total = hits.len();
            Ok::<_, anyhow::Error>(NexusSearchResults {
                hits,
                total_hits: Some(total),
            })
        })
        .await?
    }

    async fn meili_search(
        client: &meilisearch_sdk::client::Client,
        query: &str,
        server_id: Option<Uuid>,
        channel_id: Option<Uuid>,
        author_id: Option<Uuid>,
        limit: usize,
        offset: usize,
    ) -> Result<NexusSearchResults> {
        let mut filters = Vec::<String>::new();
        if let Some(s) = server_id {
            filters.push(format!("server_id = \"{s}\""));
        }
        if let Some(c) = channel_id {
            filters.push(format!("channel_id = \"{c}\""));
        }
        if let Some(a) = author_id {
            filters.push(format!("author_id = \"{a}\""));
        }
        let filter_str = filters.join(" AND ");

        let index = client.index("messages");
        let mut req = index.search();
        req.with_query(query)
            .with_limit(limit)
            .with_offset(offset)
            .with_sort(&["created_at:desc"]);
        if !filter_str.is_empty() {
            req.with_filter(&filter_str);
        }

        let results = req
            .execute::<MessageDocument>()
            .await
            .context("MeiliSearch query failed")?;
        Ok(NexusSearchResults {
            hits: results.hits.into_iter().map(|h| h.result).collect(),
            total_hits: results.estimated_total_hits,
        })
    }

    async fn meili_setup_messages(client: &meilisearch_sdk::client::Client) -> Result<()> {
        if let Ok(t) = client.create_index("messages", Some("id")).await {
            let _ = client.wait_for_task(t, None, None).await;
        }
        let idx = client.index("messages");
        idx.set_searchable_attributes(["content", "author_username"])
            .await
            .context("meili searchable")?;
        idx.set_filterable_attributes([
            "channel_id",
            "server_id",
            "author_id",
            "has_attachments",
            "has_embeds",
            "created_at",
        ])
        .await
        .context("meili filterable")?;
        idx.set_sortable_attributes(["created_at"])
            .await
            .context("meili sortable")?;
        Ok(())
    }

    async fn meili_setup_servers(client: &meilisearch_sdk::client::Client) -> Result<()> {
        if let Ok(t) = client.create_index("servers", Some("id")).await {
            let _ = client.wait_for_task(t, None, None).await;
        }
        client
            .index("servers")
            .set_searchable_attributes(["name", "description"])
            .await
            .context("meili server searchable")?;
        Ok(())
    }

    // ── Health check ──────────────────────────────────────────────────────────

    /// Probe the search backend.
    ///
    /// Returns `(is_ok, backend_name, optional_error_message)`.
    pub async fn health_check(&self) -> (bool, &'static str, Option<String>) {
        match &self.backend {
            SearchBackend::Meilisearch(client) => {
                if client.health().await.is_ok() {
                    (true, "meilisearch", None)
                } else {
                    (false, "meilisearch", Some("MeiliSearch unreachable".into()))
                }
            }
            // Tantivy is an in-process library — always reachable if the process is running.
            SearchBackend::Tantivy(_) => (true, "tantivy", None),
            // Disabled returns ok so it doesn't mark the server as degraded.
            SearchBackend::Disabled => (true, "disabled", None),
        }
    }
}
