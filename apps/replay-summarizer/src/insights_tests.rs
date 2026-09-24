use super::*;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::str::FromStr;

#[test]
fn batch_validation() {
    let ok = json!({"data":[{"index":1,"embedding":[0.,2.]},{"index":0,"embedding":[3.,4.]}]});
    assert_eq!(parse_batch(&ok, 2).unwrap()[0], vec![0.6, 0.8]);
    for invalid in [
        json!({"data":[{"index":0,"embedding":[1.]},{"index":0,"embedding":[1.]}]}),
        json!({"data":[{"index":0,"embedding":[1.]},{"index":1,"embedding":[1.,2.]}]}),
        json!({"data":[{"index":0,"embedding":[1.]},{"index":2,"embedding":[1.]}]}),
    ] {
        assert!(parse_batch(&invalid, 2).is_err());
    }
    assert!(parse_batch(&json!({"data":[{"index":0,"embedding":[null]}]}), 1).is_err());
    assert!(normalize(vec![0., 0.]).is_err());
}

#[test]
fn decisions_are_ordered_and_fail_closed() {
    assert_eq!(
        decision_scores(&json!({"answers":{"1":{"noul":0.2},"0":{"noul":0.9}}}), 2).unwrap(),
        vec![0.9, 0.2]
    );
    for body in [
        json!({"answers":{"0":{"noul":0.9}}}),
        json!({"answers":{"0":{"noul":0.9},"1":{"noul":1.1}}}),
        json!({"answers":{"0":{"noul":0.9},"1":{"noul":"0.9"}}}),
    ] {
        assert!(decision_scores(&body, 2).is_err());
    }
}

#[test]
fn matching_requires_a_decision_not_just_similar_vectors() {
    let candidates = vec![
        Candidate {
            group: Uuid::from_u128(1),
            description: String::new(),
            similarity: 0.99,
        },
        Candidate {
            group: Uuid::from_u128(2),
            description: String::new(),
            similarity: 0.7,
        },
    ];
    assert_eq!(
        best_match(&candidates, &[0.79, 0.8]),
        Some((Uuid::from_u128(2), 0.8))
    );
    assert_eq!(
        best_match(&candidates, &[0.9, 0.9]),
        Some((Uuid::from_u128(1), 0.9))
    );
    assert_eq!(best_match(&candidates, &[0.79, 0.79]), None);
    assert_eq!(best_match(&candidates, &[]), None);
}

#[test]
fn canonical_trims_and_normalizes_fields() {
    let point = PainPoint {
        timestamp_ms: 0,
        confidence: 0.8,
        description: " Button BROKE ".into(),
        evidence: String::new(),
        surface: Some(" Checkout ".into()),
        action: None,
        failure: None,
        consequence: None,
    };
    assert_eq!(
        canonical(&point),
        "surface: checkout problem: button broke evidence:"
    );
}

#[tokio::test]
#[ignore = "local PostgreSQL and paid Jev calls; temporary tables only"]
async fn evaluate_local_grouping() -> Result<()> {
    let url = std::env::var("REPLAY_TEST_DATABASE_URL")?;
    let options = PgConnectOptions::from_str(&url)?;
    ensure!(
        matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
        "local database required"
    );
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;
    let mut tx = pool.begin().await?;
    let rows = sqlx::query(
        r#"
        SELECT e.project_id, e.model_version, e.pain_point_id,
               e.embedding::real[]::double precision[], e.input_text, pp.description
        FROM replay_insight_embeddings e
        JOIN replay_summary_pain_points pp ON pp.id = e.pain_point_id ORDER BY pp.id
    "#,
    )
    .fetch_all(&mut *tx)
    .await?;
    ensure!(!rows.is_empty(), "no local embeddings");
    sqlx::raw_sql(r#"
        CREATE TEMP TABLE replay_insights (LIKE public.replay_insights INCLUDING DEFAULTS INCLUDING INDEXES) ON COMMIT DROP;
        CREATE TEMP TABLE replay_insight_embeddings (LIKE public.replay_insight_embeddings INCLUDING DEFAULTS INCLUDING INDEXES) ON COMMIT DROP;
        CREATE TEMP TABLE replay_insight_memberships (LIKE public.replay_insight_memberships INCLUDING DEFAULTS INCLUDING INDEXES) ON COMMIT DROP;
    "#).execute(&mut *tx).await?;
    let mut samples = std::collections::BTreeMap::<(Uuid, String), Vec<PreparedPoint>>::new();
    for row in &rows {
        samples
            .entry((row.get(0), row.get(1)))
            .or_default()
            .push(PreparedPoint {
                id: row.get(2),
                vector: normalize(row.get(3))?,
                text: row.get(4),
                description: row.get(5),
            });
    }
    for ((project_id, model_version), points) in samples {
        for batch in points.chunks(3) {
            let prepared = Prepared {
                project_id,
                model_version: model_version.clone(),
                points: batch.to_vec(),
            };
            save(&mut tx, &prepared).await?;
            save(&mut tx, &prepared).await?;
        }
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM replay_insight_memberships")
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(count as usize, rows.len());
    let groups: i64 = sqlx::query_scalar("SELECT count(*) FROM replay_insights")
        .fetch_one(&mut *tx)
        .await?;
    let joins: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM replay_insight_memberships WHERE match_reason LIKE 'typesafe/%'",
    )
    .fetch_one(&mut *tx)
    .await?;
    if std::env::var("OPENROUTER_API_KEY").is_ok() {
        ensure!(joins > 0, "no Jev matches; check provider configuration");
    } else {
        assert_eq!(
            joins, 0,
            "missing provider must not permit vector-only joins"
        );
        assert_eq!(
            groups, count,
            "all observations must be retained separately"
        );
    }
    eprintln!("{count} observations, {groups} groups, {joins} Jev matches; rolled back");
    tx.rollback().await?;
    Ok(())
}
