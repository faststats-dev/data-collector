use crate::{
    input::Input,
    model::{Model, VERSION},
};
use anyhow::{Context, Result, ensure};
use futures_util::{StreamExt, TryStreamExt, stream};
use reqwest::Url;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};
use std::{sync::Arc, time::Duration};
use uuid::Uuid;

#[derive(Serialize, Deserialize)]
pub struct Row {
    project_id: Uuid,
    input_hash: String,
    canonical_hash: String,
    model_version: String,
    group_id: String,
    root_type: String,
    signature: String,
    origin: String,
    generic: bool,
    isolated: bool,
    embedding: Vec<f32>,
}

#[derive(Deserialize)]
struct Group {
    group_id: String,
}

pub struct Store {
    client: reqwest::Client,
    url: Url,
    username: String,
    password: String,
    pool: PgPool,
}

impl Store {
    pub async fn connect() -> Result<Self> {
        let mut url =
            Url::parse(&std::env::var("CLICKHOUSE_URL").context("CLICKHOUSE_URL is required")?)?;
        let username = percent_encoding::percent_decode_str(url.username())
            .decode_utf8()?
            .into_owned();
        let password = percent_encoding::percent_decode_str(url.password().unwrap_or_default())
            .decode_utf8()?
            .into_owned();
        let database = url.path().trim_matches('/').to_owned();
        ensure!(
            !database.is_empty(),
            "CLICKHOUSE_URL must include a database path"
        );
        url.set_username("")
            .map_err(|_| anyhow::anyhow!("Invalid ClickHouse URL"))?;
        url.set_password(None)
            .map_err(|_| anyhow::anyhow!("Invalid ClickHouse URL"))?;
        url.set_path("/");
        url.query_pairs_mut().append_pair("database", &database);
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(120))
            .connect(
                &std::env::var("DATABASE_URL")
                    .context("DATABASE_URL is required for project locks")?,
            )
            .await?;
        let mut headers = reqwest::header::HeaderMap::new();
        for (header, name) in [
            ("cf-access-client-id", "CF_ACCESS_CLIENT_ID"),
            ("cf-access-client-secret", "CF_ACCESS_CLIENT_SECRET"),
        ] {
            if let Ok(value) = std::env::var(name) {
                headers.insert(
                    reqwest::header::HeaderName::from_static(header),
                    value.parse()?,
                );
            }
        }
        Ok(Self {
            client: reqwest::Client::builder()
                .default_headers(headers)
                .timeout(Duration::from_secs(120))
                .build()?,
            url,
            username,
            password,
            pool,
        })
    }

    pub async fn lock(&self, project: Uuid) -> Result<Transaction<'_, Postgres>> {
        let mut tx = self.pool.begin().await?;
        // Transaction-scoped locks release on cancellation or process death.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("error-embedder-v1:{project}"))
            .execute(&mut *tx)
            .await?;
        Ok(tx)
    }

    async fn request(&self, sql: String, params: &[(&str, String)]) -> Result<String> {
        let response = self
            .client
            .post(self.url.clone())
            .basic_auth(&self.username, Some(&self.password))
            .query(params)
            .body(sql)
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        ensure!(
            status.is_success(),
            "ClickHouse query failed ({status}): {body}"
        );
        Ok(body)
    }

    async fn query<T: DeserializeOwned>(
        &self,
        sql: &str,
        params: &[(&str, String)],
    ) -> Result<Vec<T>> {
        self.request(format!("{sql} FORMAT JSONEachRow"), params)
            .await?
            .lines()
            .filter(|l| !l.is_empty())
            .map(|line| serde_json::from_str(line).map_err(Into::into))
            .collect()
    }

    pub async fn contains(&self, project: Uuid, hash: &str) -> Result<bool> {
        let rows=self.query::<Group>("SELECT group_id FROM error_embeddings FINAL WHERE project_id={project:UUID} AND model_version={version:String} AND input_hash={hash:String} LIMIT 1",
            &[("param_project",project.to_string()),("param_version",VERSION.into()),("param_hash",hash.into())]).await?;
        Ok(!rows.is_empty())
    }

    pub async fn prepare(&self, input: &Input, hash: &str, model: Arc<Model>) -> Result<Row> {
        let prepared = input.prepare();
        let canonical_hash = hex::encode(Sha256::digest(prepared.text.as_bytes()));
        // Canonical text may omit the application frame used by the origin guard.
        let cached = self.query::<Row>(
            "SELECT * FROM error_embeddings FINAL WHERE project_id={project:UUID} AND model_version={version:String} AND canonical_hash={canonical:String} AND origin={origin:String} LIMIT 1",
            &[("param_project",input.project_id.to_string()),("param_version",VERSION.into()),("param_canonical",canonical_hash.clone()),("param_origin",prepared.origin.clone())]).await?;
        if let Some(mut row) = cached.into_iter().next() {
            row.input_hash = hash.into();
            if row.isolated || prepared.isolated {
                row.isolated = true;
                row.group_id = format!("emb1:{}", row.input_hash);
            }
            return Ok(row);
        }
        let text = prepared.text;
        let (embedding, truncated) =
            tokio::task::spawn_blocking(move || model.embed(&text)).await??;
        let isolated = prepared.isolated || truncated;
        let mut group_id = format!("emb1:{hash}");
        if !isolated {
            // Complete-link compatibility against every member, not just an
            // anchor. Exact project filtering precedes the small vector scan.
            let matches=self.query::<Group>(
                "SELECT group_id FROM error_embeddings FINAL
                 WHERE project_id={project:UUID} AND model_version={version:String}
                 GROUP BY group_id
                 HAVING max(isolated)=0
                   AND countIf(root_type != {root:String})=0
                   AND countIf((signature != '' OR {signature:String} != '') AND signature != {signature:String})=0
                   AND countIf((generic OR {generic:Bool}) AND origin != {origin:String})=0
                   AND min(1-cosineDistance(embedding,{vector:Array(Float32)})) >= 0.986472
                 ORDER BY min(1-cosineDistance(embedding,{vector:Array(Float32)})) DESC, group_id
                 LIMIT 1",
                &[("param_project",input.project_id.to_string()),("param_version",VERSION.into()),
                  ("param_root",prepared.root_type.clone()),("param_signature",prepared.signature.clone()),
                  ("param_generic",u8::from(prepared.generic).to_string()),("param_origin",prepared.origin.clone()),
                  ("param_vector",serde_json::to_string(&embedding)?)]).await?;
            if let Some(found) = matches.first() {
                group_id = found.group_id.clone();
            }
        }
        Ok(Row {
            project_id: input.project_id,
            input_hash: hash.into(),
            canonical_hash,
            model_version: VERSION.into(),
            group_id,
            root_type: prepared.root_type,
            signature: prepared.signature,
            origin: prepared.origin,
            generic: prepared.generic,
            isolated,
            embedding,
        })
    }

    async fn insert(&self, row: &Row) -> Result<()> {
        self.request(
            format!(
                "INSERT INTO error_embeddings FORMAT JSONEachRow\n{}",
                serde_json::to_string(row)?
            ),
            &[],
        )
        .await?;
        Ok(())
    }

    pub async fn backfill(&self, model: Arc<Model>) -> Result<()> {
        #[derive(Deserialize)]
        struct Project {
            project_id: Uuid,
        }
        let projects = self.query::<Project>(
            "SELECT project_id FROM error_tracking GROUP BY project_id ORDER BY uniqExact(embedding_input_hash), project_id", &[]).await?;
        stream::iter(projects)
            .map(Ok)
            .try_for_each_concurrent(4, |project| {
                self.backfill_project(project.project_id, model.clone())
            })
            .await?;
        tracing::info!("Backfill complete for all projects");
        Ok(())
    }

    async fn backfill_project(&self, project: Uuid, model: Arc<Model>) -> Result<()> {
        // Database visibility, not optional Redis markers, determines missing inputs.
        let mut count = 0;
        loop {
            let batch=self.query::<Input>(
                "SELECT DISTINCT project_id, language, error_type, error_message, stacktrace
                 FROM error_tracking
                 WHERE project_id={project:UUID} AND (project_id, embedding_input_hash) NOT IN
                   (SELECT project_id,input_hash FROM error_embeddings FINAL WHERE project_id={project:UUID} AND model_version={version:String})
                 ORDER BY toString(project_id), embedding_input_hash LIMIT 128",
                &[("param_version",VERSION.into()), ("param_project",project.to_string())]).await?;
            if batch.is_empty() {
                break;
            }
            for input in batch {
                let tx = self.lock(input.project_id).await?;
                let hash = input.hash();
                if !self.contains(project, &hash).await? {
                    let row = self.prepare(&input, &hash, model.clone()).await?;
                    self.insert(&row).await?;
                    count += 1;
                }
                tx.commit().await?;
            }
            tracing::info!(%project, count, "Backfilled unique embedding inputs");
        }
        tracing::info!(
            %project, count,
            "Backfill complete; every stored error input has an embedding"
        );
        Ok(())
    }
}
