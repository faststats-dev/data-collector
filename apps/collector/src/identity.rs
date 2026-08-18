use serde_json::{Map, Value};
use sqlx::types::Uuid;

#[derive(Debug, Clone)]
pub(crate) struct ResolvedPerson {
    pub(crate) person_id: Uuid,
    pub(crate) external_id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct PersonPatch {
    pub(crate) external_id: String,
    pub(crate) email: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) phone: Option<String>,
    pub(crate) avatar_url: Option<String>,
    pub(crate) clear_fields: Vec<String>,
    pub(crate) traits: Map<String, Value>,
    pub(crate) replace_traits: bool,
    pub(crate) unset_traits: Vec<String>,
    pub(crate) aliases: Vec<String>,
}

pub(crate) async fn upsert_person_and_alias(
    pool: &sqlx::PgPool,
    project_id: Uuid,
    distinct_id: &str,
    patch: &PersonPatch,
) -> Result<Uuid, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    let clear_email = patch.clear_fields.iter().any(|field| field == "email");
    let clear_name = patch.clear_fields.iter().any(|field| field == "name");
    let clear_phone = patch.clear_fields.iter().any(|field| field == "phone");
    let clear_avatar = patch.clear_fields.iter().any(|field| field == "avatarUrl");
    let traits = Value::Object(patch.traits.clone());

    let person_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO project_persons (
            project_id, external_id, email, name, phone, avatar_url, traits,
            identified_at, updated_at
        )
        VALUES (
            $1, $2,
            CASE WHEN $8 THEN NULL ELSE $3 END,
            CASE WHEN $9 THEN NULL ELSE $4 END,
            CASE WHEN $10 THEN NULL ELSE $5 END,
            CASE WHEN $11 THEN NULL ELSE $6 END,
            ($7::jsonb - $13::text[]),
            NOW(), NOW()
        )
        ON CONFLICT (project_id, external_id)
        DO UPDATE SET
            email = CASE WHEN $8 THEN NULL ELSE COALESCE(EXCLUDED.email, project_persons.email) END,
            name = CASE WHEN $9 THEN NULL ELSE COALESCE(EXCLUDED.name, project_persons.name) END,
            phone = CASE WHEN $10 THEN NULL ELSE COALESCE(EXCLUDED.phone, project_persons.phone) END,
            avatar_url = CASE WHEN $11 THEN NULL ELSE COALESCE(EXCLUDED.avatar_url, project_persons.avatar_url) END,
            traits = CASE
                WHEN $12 THEN EXCLUDED.traits
                ELSE (project_persons.traits || EXCLUDED.traits) - $13::text[]
            END,
            updated_at = NOW()
        RETURNING id
        "#,
    )
    .bind(project_id)
    .bind(&patch.external_id)
    .bind(&patch.email)
    .bind(&patch.name)
    .bind(&patch.phone)
    .bind(&patch.avatar_url)
    .bind(&traits)
    .bind(clear_email)
    .bind(clear_name)
    .bind(clear_phone)
    .bind(clear_avatar)
    .bind(patch.replace_traits)
    .bind(&patch.unset_traits)
    .fetch_one(&mut *transaction)
    .await?;

    let mut aliases = patch
        .aliases
        .iter()
        .map(|alias| alias.trim())
        .filter(|alias| !alias.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    aliases.push(distinct_id.to_owned());
    aliases.sort();
    aliases.dedup();

    for alias in &aliases {
        sqlx::query(
            r#"
            INSERT INTO project_person_aliases (
                project_id, person_id, distinct_id, created_at, updated_at
            )
            VALUES ($1, $2, $3, NOW(), NOW())
            ON CONFLICT (project_id, distinct_id)
            DO UPDATE SET person_id = EXCLUDED.person_id, updated_at = NOW()
            "#,
        )
        .bind(project_id)
        .bind(person_id)
        .bind(alias)
        .execute(&mut *transaction)
        .await?;
    }

    transaction.commit().await?;
    Ok(person_id)
}

pub(crate) async fn resolve_person_for_distinct_id(
    pool: &sqlx::PgPool,
    project_id: Uuid,
    distinct_id: &str,
) -> Result<Option<ResolvedPerson>, sqlx::Error> {
    Ok(sqlx::query_as::<_, (Uuid, String)>(
        r#"
        SELECT p.id, p.external_id
        FROM project_person_aliases a
        JOIN project_persons p ON p.id = a.person_id
        WHERE a.project_id = $1 AND a.distinct_id = $2
        "#,
    )
    .bind(project_id)
    .bind(distinct_id)
    .fetch_optional(pool)
    .await?
    .map(|(person_id, external_id)| ResolvedPerson {
        person_id,
        external_id,
    }))
}
