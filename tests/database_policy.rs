//! Real PostgreSQL policy tests, confined to sqlx's random disposable databases.
//! All cluster role DDL lives inside a rolled-back transaction (also on panic).
use endless_drawing_server::db::{self, DatabasePolicyError};
use sqlx::{Acquire, PgPool};

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires disposable PostgreSQL and DATABASE_URL"]
async fn read_only_manifest_rejects_missing_dirty_changed_and_future_versions(pool: PgPool) {
    db::verify_schema(&pool).await.unwrap();
    assert!(matches!(
        db::verify_runtime_role(&pool).await,
        Err(DatabasePolicyError::UnsafeRuntimeRole)
    ));
    sqlx::query(
        "UPDATE _sqlx_migrations SET checksum=decode(repeat('00',48),'hex') WHERE version=1",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert!(matches!(
        db::verify_schema(&pool).await,
        Err(DatabasePolicyError::SchemaMismatch)
    ));
    sqlx::query("UPDATE _sqlx_migrations SET checksum=$1 WHERE version=1")
        .bind(db::MIGRATOR.iter().next().unwrap().checksum.as_ref())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE _sqlx_migrations SET success=false WHERE version=1")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        db::verify_schema(&pool).await,
        Err(DatabasePolicyError::SchemaMismatch)
    ));
    sqlx::query("UPDATE _sqlx_migrations SET success=true WHERE version=1")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO _sqlx_migrations(version,description,success,checksum,execution_time) VALUES(999,'future',true,decode('00','hex'),0)").execute(&pool).await.unwrap();
    assert!(matches!(
        db::verify_schema(&pool).await,
        Err(DatabasePolicyError::SchemaMismatch)
    ));
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version=999")
        .execute(&pool)
        .await
        .unwrap();
    db::verify_schema(&pool).await.unwrap();
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version=4")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        db::verify_schema(&pool).await,
        Err(DatabasePolicyError::SchemaMismatch)
    ));
    sqlx::query("DROP TABLE _sqlx_migrations")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        db::verify_schema(&pool).await,
        Err(DatabasePolicyError::Database(_))
    ));
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires disposable PostgreSQL and DATABASE_URL"]
async fn nonsuperuser_migrator_initializes_schema_and_leaves_runtime_read_only_metadata(
    pool: PgPool,
) {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let migration = format!("ed_migrate_{suffix}");
    let app = format!("ed_app_{suffix}");
    let mut tx = pool.begin().await.unwrap();
    for role in [&migration, &app] {
        sqlx::query(&format!("CREATE ROLE \"{role}\" NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS")).execute(&mut *tx).await.unwrap();
    }
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let database = database.replace('"', "\"\"");
    sqlx::query(&format!(
        "GRANT CREATE ON DATABASE \"{database}\" TO \"{migration}\""
    ))
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("DROP SCHEMA public CASCADE")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(&format!(
        "CREATE SCHEMA public AUTHORIZATION \"{migration}\""
    ))
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(&format!("SET LOCAL ROLE \"{migration}\""))
        .execute(&mut *tx)
        .await
        .unwrap();
    db::MIGRATOR.run_direct(&mut *tx).await.unwrap();
    db::verify_schema(&mut *tx).await.unwrap();
    assert!(matches!(
        db::verify_runtime_role(&mut *tx).await,
        Err(DatabasePolicyError::UnsafeRuntimeRole)
    ));
    db::grant_runtime_access(&mut tx, &app).await.unwrap();
    sqlx::query("RESET ROLE").execute(&mut *tx).await.unwrap();
    sqlx::query(&format!("SET LOCAL ROLE \"{app}\""))
        .execute(&mut *tx)
        .await
        .unwrap();
    db::verify_schema(&mut *tx).await.unwrap();
    db::verify_runtime_role(&mut *tx).await.unwrap();
    tx.rollback().await.unwrap();
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires disposable PostgreSQL and DATABASE_URL"]
async fn runtime_has_dml_without_schema_cluster_file_or_migration_metadata_write(pool: PgPool) {
    let role = format!("ed_policy_{}", uuid::Uuid::new_v4().simple());
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(&format!("CREATE ROLE \"{role}\" NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS NOINHERIT")).execute(&mut *tx).await.unwrap();
    sqlx::query("REVOKE CREATE ON SCHEMA public FROM PUBLIC")
        .execute(&mut *tx)
        .await
        .unwrap();
    db::grant_runtime_access(&mut tx, &role).await.unwrap();
    assert!(
        db::grant_runtime_access(&mut tx, "invalid\";DROP SCHEMA public CASCADE")
            .await
            .is_err()
    );
    sqlx::query(&format!("SET LOCAL ROLE \"{role}\""))
        .execute(&mut *tx)
        .await
        .unwrap();
    db::verify_schema(&mut *tx).await.unwrap();
    db::verify_runtime_role(&mut *tx).await.unwrap();
    let all_dml:bool=sqlx::query_scalar("SELECT bool_and(has_table_privilege(current_user,c.oid,'SELECT') AND has_table_privilege(current_user,c.oid,'INSERT') AND has_table_privilege(current_user,c.oid,'UPDATE') AND has_table_privilege(current_user,c.oid,'DELETE')) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND c.relkind='r' AND c.relname<>'_sqlx_migrations'").fetch_one(&mut *tx).await.unwrap();
    assert!(all_dml);
    let id:uuid::Uuid=sqlx::query_scalar("INSERT INTO strokes(id,owner_digest,points,brush,width,color,opacity,bounds) VALUES(gen_random_uuid(),decode(repeat('00',32),'hex'),'[[0,0],[1,1]]','round',4,'#123456',1,box(point(-3,-3),point(4,4))) RETURNING id").fetch_one(&mut *tx).await.unwrap();
    assert_eq!(
        sqlx::query("UPDATE strokes SET deleted_at=now() WHERE id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .unwrap()
            .rows_affected(),
        1
    );
    assert_eq!(
        sqlx::query("DELETE FROM strokes WHERE id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .unwrap()
            .rows_affected(),
        1
    );
    for forbidden in [
        "CREATE TABLE public.runtime_forbidden(id integer)",
        "ALTER TABLE public.strokes ADD COLUMN runtime_forbidden integer",
        "DROP TABLE public.strokes",
        "UPDATE public._sqlx_migrations SET description='tampered'",
        "CREATE ROLE runtime_forbidden",
        "SELECT pg_read_file('/etc/passwd')",
        "COPY(SELECT 1) TO PROGRAM 'true'",
    ] {
        let mut savepoint = tx.begin().await.unwrap();
        let error = sqlx::query(forbidden)
            .execute(&mut *savepoint)
            .await
            .unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("42501"),
            "{forbidden}"
        );
        savepoint.rollback().await.unwrap();
    }
    // NOINHERIT membership still permits SET ROLE; policy must reject that escape hatch.
    sqlx::query("RESET ROLE").execute(&mut *tx).await.unwrap();
    sqlx::query(&format!("GRANT pg_read_all_data TO \"{role}\""))
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(&format!("SET LOCAL ROLE \"{role}\""))
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(matches!(
        db::verify_runtime_role(&mut *tx).await,
        Err(DatabasePolicyError::UnsafeRuntimeRole)
    ));
    tx.rollback().await.unwrap(); // Role + grants + membership vanish; never leave global roles.
}
