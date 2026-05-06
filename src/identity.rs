use serde_json::Value;
use sqlx::types::Uuid;

#[derive(Debug, Clone)]
pub(crate) struct ResolvedPerson {
    pub(crate) person_id: Uuid,
    pub(crate) external_id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct PersonProfile {
    pub(crate) external_id: String,
    pub(crate) email: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) phone: Option<String>,
    pub(crate) avatar_url: Option<String>,
    pub(crate) traits: Value,
}

pub(crate) async fn upsert_person_and_alias(
    pool: &sqlx::PgPool,
    project_id: Uuid,
    distinct_id: &str,
    profile: &PersonProfile,
) -> Result<Uuid, sqlx::Error> {
    let person_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO project_persons (
            project_id,
            external_id,
            email,
            name,
            phone,
            avatar_url,
            traits,
            identified_at,
            updated_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb, NOW(), NOW())
        ON CONFLICT (project_id, external_id)
        DO UPDATE SET
            email = EXCLUDED.email,
            name = EXCLUDED.name,
            phone = EXCLUDED.phone,
            avatar_url = EXCLUDED.avatar_url,
            traits = EXCLUDED.traits,
            updated_at = NOW()
        RETURNING id
        "#,
    )
    .bind(project_id)
    .bind(&profile.external_id)
    .bind(&profile.email)
    .bind(&profile.name)
    .bind(&profile.phone)
    .bind(&profile.avatar_url)
    .bind(&profile.traits)
    .fetch_one(pool)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO project_person_aliases (
            project_id,
            person_id,
            distinct_id,
            created_at,
            updated_at
        )
        VALUES ($1, $2, $3, NOW(), NOW())
        ON CONFLICT (project_id, distinct_id)
        DO UPDATE SET
            person_id = EXCLUDED.person_id,
            updated_at = NOW()
        "#,
    )
    .bind(project_id)
    .bind(person_id)
    .bind(distinct_id)
    .execute(pool)
    .await?;

    Ok(person_id)
}

pub(crate) async fn resolve_person_for_distinct_id(
    pool: &sqlx::PgPool,
    project_id: Uuid,
    distinct_id: &str,
) -> Result<Option<ResolvedPerson>, sqlx::Error> {
    sqlx::query_as::<_, (Uuid, String)>(
        r#"
        SELECT
            p.id,
            p.external_id
        FROM project_person_aliases a
        JOIN project_persons p ON p.id = a.person_id
        WHERE a.project_id = $1
          AND a.distinct_id = $2
        "#,
    )
    .bind(project_id)
    .bind(distinct_id)
    .fetch_optional(pool)
    .await
    .map(|row| {
        row.map(|(person_id, external_id)| ResolvedPerson {
            person_id,
            external_id,
        })
    })
}
