use std::collections::HashMap;

use sqlx::{
    SqlitePool,
    migrate::{Migrate, Migrator},
    sqlite::SqliteConnectOptions,
};

static MIGRATOR: Migrator = sqlx::migrate!();
pub struct Database {
    db: SqlitePool,
}

impl Database {
    pub async fn load(url: &str) -> anyhow::Result<Database> {
        let db = SqlitePool::connect(&url).await?;

        let mut conn = db.acquire().await?;
        conn.ensure_migrations_table().await?;
        let applied_migrations: HashMap<_, _> = conn
            .list_applied_migrations()
            .await?
            .into_iter()
            .map(|e| (e.version, e.checksum))
            .collect();
        for migration in MIGRATOR.iter() {
            if migration.migration_type.is_down_migration() {
                continue;
            }
            match applied_migrations.get(&migration.version) {
                None => {
                    return Err(anyhow::anyhow!(
                        "Database migration pending, please run `pixivdwn database setup`"
                    ));
                }
                Some(checksum) if checksum != &migration.checksum => {
                    return Err(anyhow::anyhow!(
                        "Database migration version {} checksum mismatch, possible corruption",
                        migration.version
                    ));
                }
                _ => {}
            }
        }

        Ok(Database { db })
    }

    pub async fn setup(url: &str) -> anyhow::Result<Database> {
        let opts: SqliteConnectOptions = url.parse()?;
        let opts = opts.create_if_missing(true);
        let db = SqlitePool::connect_with(opts).await?;
        MIGRATOR.run(&db).await?;
        Ok(Database { db })
    }
}

impl AsRef<SqlitePool> for Database {
    fn as_ref(&self) -> &SqlitePool {
        &self.db
    }
}
