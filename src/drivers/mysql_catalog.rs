//! Prepared, level-specific MySQL catalog browsing and pure retained-state bounds.

use std::collections::{HashMap, HashSet};

use base64::Engine as _;
use chrono::{SecondsFormat, Utc};
use futures_util::TryStreamExt as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use sqlx::{Either, Executor as _, Row as _, SqlSafeStr as _, Statement as _};

use crate::drivers::DriverError;
use crate::model::{
    CatalogLevel, CatalogNode, CatalogNodeIdentity, CatalogNodeKind, CatalogPage, CatalogPageToken,
    CatalogRequest, CatalogRetainedCounts, MAX_CATALOG_COLUMNS, MAX_CATALOG_COLUMNS_PER_RELATION,
    MAX_CATALOG_RELATIONS, MAX_CATALOG_SCHEMAS, MAX_CATALOG_UTF8_BYTES, RequestValidationError,
};

const TOKEN_VERSION: u8 = 1;
const TOKEN_PREFIX: &str = "v1";
const TOKEN_DIGEST_DOMAIN: &[u8] = b"dbotter:mysql-catalog-token:v1\0";

const SCHEMA_QUERY: &str = r#"
SELECT SCHEMA_NAME
FROM information_schema.SCHEMATA
WHERE (? IS NULL OR CAST(SCHEMA_NAME AS BINARY) = CAST(? AS BINARY))
  AND (? = '' OR LEFT(CAST(SCHEMA_NAME AS BINARY), OCTET_LENGTH(CAST(? AS BINARY))) = CAST(? AS BINARY))
  AND (? = '' OR CAST(SCHEMA_NAME AS BINARY) > CAST(? AS BINARY))
ORDER BY CAST(SCHEMA_NAME AS BINARY)
LIMIT ?
"#;

const RELATION_QUERY: &str = r#"
SELECT TABLE_NAME, TABLE_TYPE
FROM information_schema.TABLES
WHERE CAST(TABLE_SCHEMA AS BINARY) = CAST(? AS BINARY)
  AND (? = '' OR LEFT(CAST(TABLE_NAME AS BINARY), OCTET_LENGTH(CAST(? AS BINARY))) = CAST(? AS BINARY))
  AND (? = '' OR CAST(TABLE_NAME AS BINARY) > CAST(? AS BINARY))
ORDER BY CAST(TABLE_NAME AS BINARY)
LIMIT ?
"#;

const COLUMN_QUERY: &str = r#"
SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, ORDINAL_POSITION
FROM information_schema.COLUMNS
WHERE CAST(TABLE_SCHEMA AS BINARY) = CAST(? AS BINARY)
  AND CAST(TABLE_NAME AS BINARY) = CAST(? AS BINARY)
  AND (? = '' OR LEFT(CAST(COLUMN_NAME AS BINARY), OCTET_LENGTH(CAST(? AS BINARY))) = CAST(? AS BINARY))
  AND (ORDINAL_POSITION > ? OR (ORDINAL_POSITION = ? AND CAST(COLUMN_NAME AS BINARY) > CAST(? AS BINARY)))
ORDER BY ORDINAL_POSITION, CAST(COLUMN_NAME AS BINARY)
LIMIT ?
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogRetentionOutcome {
    pub nodes: Vec<CatalogNode>,
    pub retained_counts: CatalogRetainedCounts,
    pub retained_utf8_bytes: usize,
    pub truncated: bool,
}

/// Pure per-profile retained catalog budget. The UI owns one instance for each
/// active profile generation; it never owns a driver session.
#[derive(Debug, Default)]
pub struct CatalogRetention {
    identities: HashSet<CatalogNodeIdentity>,
    retained_counts: CatalogRetainedCounts,
    retained_utf8_bytes: usize,
    columns_by_relation: HashMap<(String, String), usize>,
}

impl CatalogRetention {
    pub fn clear(&mut self) {
        self.identities.clear();
        self.retained_counts = CatalogRetainedCounts::default();
        self.retained_utf8_bytes = 0;
        self.columns_by_relation.clear();
    }

    pub fn retain(&mut self, nodes: Vec<CatalogNode>) -> CatalogRetentionOutcome {
        let mut accepted = Vec::with_capacity(nodes.len());
        let mut truncated = false;
        for node in nodes {
            match self.retain_one(node) {
                RetainDecision::Accepted(node) => accepted.push(node),
                RetainDecision::Duplicate => {}
                RetainDecision::CapReached => truncated = true,
            }
        }
        CatalogRetentionOutcome {
            nodes: accepted,
            retained_counts: self.counts(),
            retained_utf8_bytes: self.retained_utf8_bytes,
            truncated,
        }
    }

    pub fn remove(&mut self, nodes: &[CatalogNode]) {
        for node in nodes {
            if !self.identities.remove(&node.identity) {
                continue;
            }
            self.retained_utf8_bytes = self
                .retained_utf8_bytes
                .saturating_sub(catalog_node_utf8_bytes(node));
            match &node.identity {
                CatalogNodeIdentity::Schema { .. } => {
                    self.retained_counts.schemas = self.retained_counts.schemas.saturating_sub(1);
                }
                CatalogNodeIdentity::Relation { .. } => {
                    self.retained_counts.relations =
                        self.retained_counts.relations.saturating_sub(1);
                }
                CatalogNodeIdentity::Column {
                    schema, relation, ..
                } => {
                    self.retained_counts.columns = self.retained_counts.columns.saturating_sub(1);
                    let key = (schema.clone(), relation.clone());
                    if let Some(count) = self.columns_by_relation.get_mut(&key) {
                        *count = count.saturating_sub(1);
                        if *count == 0 {
                            self.columns_by_relation.remove(&key);
                        }
                    }
                }
            }
        }
    }

    pub fn counts(&self) -> CatalogRetainedCounts {
        CatalogRetainedCounts {
            columns_in_relation: self
                .columns_by_relation
                .values()
                .copied()
                .max()
                .unwrap_or(0),
            ..self.retained_counts
        }
    }

    pub const fn retained_utf8_bytes(&self) -> usize {
        self.retained_utf8_bytes
    }

    fn from_checkpoint(
        counts: CatalogRetainedCounts,
        retained_utf8_bytes: usize,
        relation: Option<(String, String)>,
    ) -> Result<Self, DriverError> {
        if counts.schemas > MAX_CATALOG_SCHEMAS
            || counts.relations > MAX_CATALOG_RELATIONS
            || counts.columns > MAX_CATALOG_COLUMNS
            || counts.columns_in_relation > MAX_CATALOG_COLUMNS_PER_RELATION
            || retained_utf8_bytes > MAX_CATALOG_UTF8_BYTES
        {
            return Err(DriverError::InvalidCatalogRequest);
        }
        let mut columns_by_relation = HashMap::new();
        if counts.columns_in_relation > 0 {
            let relation = relation.ok_or(DriverError::InvalidCatalogRequest)?;
            columns_by_relation.insert(relation, counts.columns_in_relation);
        }
        Ok(Self {
            identities: HashSet::new(),
            retained_counts: counts,
            retained_utf8_bytes,
            columns_by_relation,
        })
    }

    fn can_retain(&self, node: &CatalogNode) -> bool {
        if self.identities.contains(&node.identity) {
            return true;
        }
        if self
            .retained_utf8_bytes
            .saturating_add(catalog_node_utf8_bytes(node))
            > MAX_CATALOG_UTF8_BYTES
        {
            return false;
        }
        match &node.identity {
            CatalogNodeIdentity::Schema { .. } => {
                self.retained_counts.schemas < MAX_CATALOG_SCHEMAS
            }
            CatalogNodeIdentity::Relation { .. } => {
                self.retained_counts.relations < MAX_CATALOG_RELATIONS
            }
            CatalogNodeIdentity::Column {
                schema, relation, ..
            } => {
                self.retained_counts.columns < MAX_CATALOG_COLUMNS
                    && self
                        .columns_by_relation
                        .get(&(schema.clone(), relation.clone()))
                        .copied()
                        .unwrap_or(0)
                        < MAX_CATALOG_COLUMNS_PER_RELATION
            }
        }
    }

    fn retain_one(&mut self, node: CatalogNode) -> RetainDecision {
        if self.identities.contains(&node.identity) {
            return RetainDecision::Duplicate;
        }
        if !self.can_retain(&node) {
            return RetainDecision::CapReached;
        }
        self.retained_utf8_bytes = self
            .retained_utf8_bytes
            .saturating_add(catalog_node_utf8_bytes(&node));
        match &node.identity {
            CatalogNodeIdentity::Schema { .. } => {
                self.retained_counts.schemas = self.retained_counts.schemas.saturating_add(1);
            }
            CatalogNodeIdentity::Relation { .. } => {
                self.retained_counts.relations = self.retained_counts.relations.saturating_add(1);
            }
            CatalogNodeIdentity::Column {
                schema, relation, ..
            } => {
                self.retained_counts.columns = self.retained_counts.columns.saturating_add(1);
                let count = self
                    .columns_by_relation
                    .entry((schema.clone(), relation.clone()))
                    .or_default();
                *count = count.saturating_add(1);
            }
        }
        self.identities.insert(node.identity.clone());
        RetainDecision::Accepted(node)
    }
}

enum RetainDecision {
    Accepted(CatalogNode),
    Duplicate,
    CapReached,
}

pub fn catalog_node_utf8_bytes(node: &CatalogNode) -> usize {
    node.name
        .len()
        .saturating_add(node.type_name.as_deref().map_or(0, str::len))
}

pub fn quote_mysql_identifier(identifier: &str) -> String {
    let mut quoted = String::with_capacity(identifier.len().saturating_add(2));
    quoted.push('`');
    for character in identifier.chars() {
        if character == '`' {
            quoted.push('`');
        }
        quoted.push(character);
    }
    quoted.push('`');
    quoted
}

pub fn bounded_select_template(schema: &str, relation: &str) -> String {
    let quoted_schema = quote_mysql_identifier(schema);
    let quoted_relation = quote_mysql_identifier(relation);
    let mut template = String::with_capacity(
        quoted_schema
            .len()
            .saturating_add(quoted_relation.len())
            .saturating_add(32),
    );
    template.push_str("SELECT * FROM ");
    template.push_str(&quoted_schema);
    template.push('.');
    template.push_str(&quoted_relation);
    template.push_str(" LIMIT 500");
    template
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenPayload {
    version: u8,
    profile_fingerprint: String,
    profile_generation: u64,
    level: u8,
    parent_fingerprint: String,
    prefix_fingerprint: String,
    last_name: String,
    last_ordinal: Option<u32>,
    schemas: u64,
    relations: u64,
    columns: u64,
    columns_in_relation: u64,
    retained_utf8_bytes: u64,
}

struct DecodedToken {
    last_name: String,
    last_ordinal: Option<u32>,
    counts: CatalogRetainedCounts,
    retained_utf8_bytes: usize,
}

pub async fn load_page(
    pool: &sqlx::MySqlPool,
    configured_database: Option<&str>,
    request: &CatalogRequest,
) -> Result<CatalogPage, DriverError> {
    request
        .validate()
        .map_err(|_error: RequestValidationError| DriverError::InvalidCatalogRequest)?;
    let decoded = match request.page_token() {
        Some(token) => decode_page_token(request, token)?,
        None => DecodedToken {
            last_name: String::new(),
            last_ordinal: None,
            counts: CatalogRetainedCounts::default(),
            retained_utf8_bytes: 0,
        },
    };
    let page_size = u32::from(request.page_size());
    let server_limit = page_size.saturating_add(1);
    let timeout = request.timeout();
    tokio::time::timeout(
        timeout,
        load_page_inner(pool, configured_database, request, decoded, server_limit),
    )
    .await
    .map_err(|_| DriverError::Timeout {
        driver: crate::model::DriverKind::MySql,
        seconds: timeout.as_secs(),
    })?
}

async fn load_page_inner(
    pool: &sqlx::MySqlPool,
    configured_database: Option<&str>,
    request: &CatalogRequest,
    decoded: DecodedToken,
    server_limit: u32,
) -> Result<CatalogPage, DriverError> {
    let prefix = request.prefix().unwrap_or_default().to_owned();
    let nodes = match request {
        CatalogRequest::Schemas { .. } => {
            load_schemas(
                pool,
                configured_database.map(str::to_owned),
                &prefix,
                &decoded.last_name,
                server_limit,
            )
            .await?
        }
        CatalogRequest::Relations { schema, .. } => {
            load_relations(pool, schema, &prefix, &decoded.last_name, server_limit).await?
        }
        CatalogRequest::Columns {
            schema, relation, ..
        } => {
            load_columns(
                pool,
                schema,
                relation,
                &prefix,
                decoded.last_ordinal.unwrap_or(0),
                &decoded.last_name,
                server_limit,
            )
            .await?
        }
    };
    finish_page(request, decoded, nodes)
}

async fn load_schemas(
    pool: &sqlx::MySqlPool,
    configured_database: Option<String>,
    prefix: &str,
    last_name: &str,
    server_limit: u32,
) -> Result<Vec<CatalogNode>, DriverError> {
    let mut connection = pool.acquire().await.map_err(DriverError::from)?;
    let statement = (&mut *connection)
        .prepare(sqlx::AssertSqlSafe(SCHEMA_QUERY.to_owned()).into_sql_str())
        .await
        .map_err(DriverError::from)?;
    let query = statement
        .query()
        .bind(configured_database.clone())
        .bind(configured_database)
        .bind(prefix.to_owned())
        .bind(prefix.to_owned())
        .bind(prefix.to_owned())
        .bind(last_name.to_owned())
        .bind(last_name.to_owned())
        .bind(server_limit);
    let mut stream = (&mut *connection).fetch_many(query);
    let mut nodes = Vec::new();
    while let Some(step) = stream.try_next().await.map_err(DriverError::from)? {
        if let Either::Right(row) = step {
            let name = row.try_get::<String, _>(0).map_err(DriverError::from)?;
            nodes.push(CatalogNode {
                identity: CatalogNodeIdentity::Schema {
                    schema: name.clone(),
                },
                kind: CatalogNodeKind::Schema,
                name,
                type_name: None,
                nullable: None,
                ordinal: None,
            });
        }
    }
    Ok(nodes)
}

async fn load_relations(
    pool: &sqlx::MySqlPool,
    schema: &str,
    prefix: &str,
    last_name: &str,
    server_limit: u32,
) -> Result<Vec<CatalogNode>, DriverError> {
    let mut connection = pool.acquire().await.map_err(DriverError::from)?;
    let statement = (&mut *connection)
        .prepare(sqlx::AssertSqlSafe(RELATION_QUERY.to_owned()).into_sql_str())
        .await
        .map_err(DriverError::from)?;
    let query = statement
        .query()
        .bind(schema.to_owned())
        .bind(prefix.to_owned())
        .bind(prefix.to_owned())
        .bind(prefix.to_owned())
        .bind(last_name.to_owned())
        .bind(last_name.to_owned())
        .bind(server_limit);
    let mut stream = (&mut *connection).fetch_many(query);
    let mut nodes = Vec::new();
    while let Some(step) = stream.try_next().await.map_err(DriverError::from)? {
        if let Either::Right(row) = step {
            let name = row.try_get::<String, _>(0).map_err(DriverError::from)?;
            let relation_type = row.try_get::<String, _>(1).map_err(DriverError::from)?;
            let kind = if relation_type.eq_ignore_ascii_case("VIEW")
                || relation_type.eq_ignore_ascii_case("SYSTEM VIEW")
            {
                CatalogNodeKind::View
            } else {
                CatalogNodeKind::Table
            };
            nodes.push(CatalogNode {
                identity: CatalogNodeIdentity::Relation {
                    schema: schema.to_owned(),
                    relation: name.clone(),
                },
                kind,
                name,
                type_name: Some(relation_type),
                nullable: None,
                ordinal: None,
            });
        }
    }
    Ok(nodes)
}

async fn load_columns(
    pool: &sqlx::MySqlPool,
    schema: &str,
    relation: &str,
    prefix: &str,
    last_ordinal: u32,
    last_name: &str,
    server_limit: u32,
) -> Result<Vec<CatalogNode>, DriverError> {
    let mut connection = pool.acquire().await.map_err(DriverError::from)?;
    let statement = (&mut *connection)
        .prepare(sqlx::AssertSqlSafe(COLUMN_QUERY.to_owned()).into_sql_str())
        .await
        .map_err(DriverError::from)?;
    let query = statement
        .query()
        .bind(schema.to_owned())
        .bind(relation.to_owned())
        .bind(prefix.to_owned())
        .bind(prefix.to_owned())
        .bind(prefix.to_owned())
        .bind(last_ordinal)
        .bind(last_ordinal)
        .bind(last_name.to_owned())
        .bind(server_limit);
    let mut stream = (&mut *connection).fetch_many(query);
    let mut nodes = Vec::new();
    while let Some(step) = stream.try_next().await.map_err(DriverError::from)? {
        if let Either::Right(row) = step {
            let name = row.try_get::<String, _>(0).map_err(DriverError::from)?;
            let type_name = row.try_get::<String, _>(1).map_err(DriverError::from)?;
            let nullable = row
                .try_get::<String, _>(2)
                .map_err(DriverError::from)?
                .eq_ignore_ascii_case("YES");
            let ordinal = row.try_get::<u32, _>(3).map_err(DriverError::from)?;
            nodes.push(CatalogNode {
                identity: CatalogNodeIdentity::Column {
                    schema: schema.to_owned(),
                    relation: relation.to_owned(),
                    ordinal,
                },
                kind: CatalogNodeKind::Column,
                name,
                type_name: Some(type_name),
                nullable: Some(nullable),
                ordinal: Some(ordinal),
            });
        }
    }
    Ok(nodes)
}

fn finish_page(
    request: &CatalogRequest,
    decoded: DecodedToken,
    mut fetched: Vec<CatalogNode>,
) -> Result<CatalogPage, DriverError> {
    let page_size = usize::from(request.page_size());
    let has_extra = fetched.len() > page_size;
    let extra = fetched.get(page_size).cloned();
    fetched.truncate(page_size);
    let relation = match request {
        CatalogRequest::Columns {
            schema, relation, ..
        } => Some((schema.clone(), relation.clone())),
        CatalogRequest::Schemas { .. } | CatalogRequest::Relations { .. } => None,
    };
    let mut retention =
        CatalogRetention::from_checkpoint(decoded.counts, decoded.retained_utf8_bytes, relation)?;
    let mut outcome = retention.retain(fetched);
    let extra_fits = extra
        .as_ref()
        .is_none_or(|candidate| retention.can_retain(candidate));
    if has_extra && !extra_fits {
        outcome.truncated = true;
    }
    let next_token =
        if has_extra && extra_fits && !outcome.truncated && outcome.nodes.len() == page_size {
            outcome
                .nodes
                .last()
                .map(|last| encode_page_token(request, last, &outcome))
                .transpose()?
        } else {
            None
        };
    let parent = match request {
        CatalogRequest::Schemas { .. } => None,
        CatalogRequest::Relations { schema, .. } => Some(CatalogNodeIdentity::Schema {
            schema: schema.clone(),
        }),
        CatalogRequest::Columns {
            schema, relation, ..
        } => Some(CatalogNodeIdentity::Relation {
            schema: schema.clone(),
            relation: relation.clone(),
        }),
    };
    Ok(CatalogPage {
        identity: request.identity().clone(),
        level: request.level(),
        parent,
        nodes: outcome.nodes,
        next_token,
        retained_counts: outcome.retained_counts,
        retained_utf8_bytes: outcome.retained_utf8_bytes,
        truncated: outcome.truncated,
        stale: false,
        loaded_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
    })
}

fn encode_page_token(
    request: &CatalogRequest,
    last: &CatalogNode,
    outcome: &CatalogRetentionOutcome,
) -> Result<CatalogPageToken, DriverError> {
    let (last_name, last_ordinal) = match last {
        CatalogNode {
            identity: CatalogNodeIdentity::Column { ordinal, .. },
            name,
            ..
        } => (name.clone(), Some(*ordinal)),
        CatalogNode { name, .. } => (name.clone(), None),
    };
    let payload = TokenPayload {
        version: TOKEN_VERSION,
        profile_fingerprint: profile_fingerprint(request),
        profile_generation: request.profile_generation().0,
        level: level_tag(request.level()),
        parent_fingerprint: parent_fingerprint(request),
        prefix_fingerprint: prefix_fingerprint(request.prefix()),
        last_name,
        last_ordinal,
        schemas: usize_to_u64(outcome.retained_counts.schemas)?,
        relations: usize_to_u64(outcome.retained_counts.relations)?,
        columns: usize_to_u64(outcome.retained_counts.columns)?,
        columns_in_relation: usize_to_u64(outcome.retained_counts.columns_in_relation)?,
        retained_utf8_bytes: usize_to_u64(outcome.retained_utf8_bytes)?,
    };
    let payload_bytes =
        serde_json::to_vec(&payload).map_err(|_| DriverError::InvalidCatalogRequest)?;
    let digest = token_digest(&payload_bytes);
    let payload_encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload_bytes);
    let digest_encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    Ok(CatalogPageToken(format!(
        "{TOKEN_PREFIX}.{payload_encoded}.{digest_encoded}"
    )))
}

fn decode_page_token(
    request: &CatalogRequest,
    token: &CatalogPageToken,
) -> Result<DecodedToken, DriverError> {
    let mut parts = token.0.split('.');
    let prefix = parts.next().ok_or(DriverError::InvalidCatalogRequest)?;
    let payload_encoded = parts.next().ok_or(DriverError::InvalidCatalogRequest)?;
    let digest_encoded = parts.next().ok_or(DriverError::InvalidCatalogRequest)?;
    if prefix != TOKEN_PREFIX || parts.next().is_some() {
        return Err(DriverError::InvalidCatalogRequest);
    }
    let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_encoded)
        .map_err(|_| DriverError::InvalidCatalogRequest)?;
    let supplied_digest = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(digest_encoded)
        .map_err(|_| DriverError::InvalidCatalogRequest)?;
    if supplied_digest.as_slice() != token_digest(&payload_bytes) {
        return Err(DriverError::InvalidCatalogRequest);
    }
    let payload: TokenPayload =
        serde_json::from_slice(&payload_bytes).map_err(|_| DriverError::InvalidCatalogRequest)?;
    if payload.version != TOKEN_VERSION
        || payload.profile_fingerprint != profile_fingerprint(request)
        || payload.profile_generation != request.profile_generation().0
        || payload.level != level_tag(request.level())
        || payload.parent_fingerprint != parent_fingerprint(request)
        || payload.prefix_fingerprint != prefix_fingerprint(request.prefix())
        || (request.level() == CatalogLevel::Columns) != payload.last_ordinal.is_some()
    {
        return Err(DriverError::InvalidCatalogRequest);
    }
    let counts = CatalogRetainedCounts {
        schemas: u64_to_usize(payload.schemas)?,
        relations: u64_to_usize(payload.relations)?,
        columns: u64_to_usize(payload.columns)?,
        columns_in_relation: u64_to_usize(payload.columns_in_relation)?,
    };
    let retained_utf8_bytes = u64_to_usize(payload.retained_utf8_bytes)?;
    CatalogRetention::from_checkpoint(
        counts,
        retained_utf8_bytes,
        match request {
            CatalogRequest::Columns {
                schema, relation, ..
            } => Some((schema.clone(), relation.clone())),
            CatalogRequest::Schemas { .. } | CatalogRequest::Relations { .. } => None,
        },
    )?;
    Ok(DecodedToken {
        last_name: payload.last_name,
        last_ordinal: payload.last_ordinal,
        counts,
        retained_utf8_bytes,
    })
}

fn token_digest(payload: &[u8]) -> Vec<u8> {
    let mut digest = Sha256::new();
    digest.update(TOKEN_DIGEST_DOMAIN);
    digest.update(payload);
    digest.finalize().to_vec()
}

fn parent_fingerprint(request: &CatalogRequest) -> String {
    let mut digest = Sha256::new();
    digest.update(b"dbotter:mysql-catalog-parent:v1\0");
    match request {
        CatalogRequest::Schemas { .. } => digest.update([0]),
        CatalogRequest::Relations { schema, .. } => {
            digest.update([1]);
            update_fingerprint_part(&mut digest, schema);
        }
        CatalogRequest::Columns {
            schema, relation, ..
        } => {
            digest.update([2]);
            update_fingerprint_part(&mut digest, schema);
            update_fingerprint_part(&mut digest, relation);
        }
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest.finalize())
}

fn profile_fingerprint(request: &CatalogRequest) -> String {
    let mut digest = Sha256::new();
    digest.update(b"dbotter:mysql-catalog-profile:v1\0");
    update_fingerprint_part(&mut digest, &request.profile_id().0);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest.finalize())
}

fn prefix_fingerprint(prefix: Option<&str>) -> String {
    let mut digest = Sha256::new();
    digest.update(b"dbotter:mysql-catalog-prefix:v1\0");
    match prefix {
        Some(prefix) => {
            digest.update([1]);
            update_fingerprint_part(&mut digest, prefix);
        }
        None => digest.update([0]),
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest.finalize())
}

fn update_fingerprint_part(digest: &mut Sha256, value: &str) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value.as_bytes());
}

const fn level_tag(level: CatalogLevel) -> u8 {
    match level {
        CatalogLevel::Schemas => 1,
        CatalogLevel::Relations => 2,
        CatalogLevel::Columns => 3,
    }
}

fn usize_to_u64(value: usize) -> Result<u64, DriverError> {
    u64::try_from(value).map_err(|_| DriverError::InvalidCatalogRequest)
}

fn u64_to_usize(value: u64) -> Result<usize, DriverError> {
    usize::try_from(value).map_err(|_| DriverError::InvalidCatalogRequest)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::model::{OperationId, ProfileGeneration, ProfileId, RequestIdentity};

    fn identity(generation: u64) -> RequestIdentity {
        RequestIdentity::new(
            ProfileId("mysql-live".to_owned()),
            ProfileGeneration(generation),
            OperationId(7),
        )
    }

    fn schema_request(generation: u64, token: Option<CatalogPageToken>) -> CatalogRequest {
        CatalogRequest::Schemas {
            identity: identity(generation),
            prefix: Some("app_".to_owned()),
            page_token: token,
            page_size: 2,
            timeout: Duration::from_secs(5),
        }
    }

    fn schema(name: &str) -> CatalogNode {
        CatalogNode {
            identity: CatalogNodeIdentity::Schema {
                schema: name.to_owned(),
            },
            kind: CatalogNodeKind::Schema,
            name: name.to_owned(),
            type_name: None,
            nullable: None,
            ordinal: None,
        }
    }

    #[test]
    fn page_size_plus_one_proves_more_and_token_uses_last_retained_key() {
        let request = schema_request(4, None);
        let page = finish_page(
            &request,
            DecodedToken {
                last_name: String::new(),
                last_ordinal: None,
                counts: CatalogRetainedCounts::default(),
                retained_utf8_bytes: 0,
            },
            vec![schema("app_a"), schema("app_b"), schema("app_c")],
        )
        .expect("valid synthetic catalog page");
        assert_eq!(page.nodes.len(), 2);
        assert!(page.next_token.is_some());
        let token = page.next_token.as_ref().expect("next page token");
        let decoded = decode_page_token(&request, token).expect("decode generated token");
        assert_eq!(decoded.last_name, "app_b");
        assert_eq!(decoded.counts.schemas, 2);
    }

    #[test]
    fn token_tampering_generation_parent_and_prefix_mismatch_fail_closed() {
        let request = schema_request(4, None);
        let outcome = CatalogRetentionOutcome {
            nodes: vec![schema("app_b")],
            retained_counts: CatalogRetainedCounts {
                schemas: 1,
                ..CatalogRetainedCounts::default()
            },
            retained_utf8_bytes: 5,
            truncated: false,
        };
        let mut token =
            encode_page_token(&request, &outcome.nodes[0], &outcome).expect("encode valid token");
        token.0.push('x');
        assert!(decode_page_token(&request, &token).is_err());

        let valid =
            encode_page_token(&request, &outcome.nodes[0], &outcome).expect("encode valid token");
        assert!(decode_page_token(&schema_request(5, None), &valid).is_err());
        let different_profile = CatalogRequest::Schemas {
            identity: RequestIdentity::new(
                ProfileId("other-mysql".to_owned()),
                ProfileGeneration(4),
                OperationId(8),
            ),
            prefix: Some("app_".to_owned()),
            page_token: None,
            page_size: 2,
            timeout: Duration::from_secs(5),
        };
        assert!(decode_page_token(&different_profile, &valid).is_err());
        let different_prefix = CatalogRequest::Schemas {
            identity: identity(4),
            prefix: Some("other_".to_owned()),
            page_token: None,
            page_size: 2,
            timeout: Duration::from_secs(5),
        };
        assert!(decode_page_token(&different_prefix, &valid).is_err());
    }

    #[test]
    fn no_extra_row_does_not_fabricate_truncation_at_an_exact_cap() {
        let request = CatalogRequest::Schemas {
            identity: identity(4),
            prefix: None,
            page_token: None,
            page_size: MAX_CATALOG_SCHEMAS as u16,
            timeout: Duration::from_secs(5),
        };
        let nodes = (0..MAX_CATALOG_SCHEMAS)
            .map(|index| schema(&format!("schema-{index:04}")))
            .collect();
        let page = finish_page(
            &request,
            DecodedToken {
                last_name: String::new(),
                last_ordinal: None,
                counts: CatalogRetainedCounts::default(),
                retained_utf8_bytes: 0,
            },
            nodes,
        )
        .expect("valid exact-cap page");
        assert!(!page.truncated);
        assert!(page.next_token.is_none());
    }
}
