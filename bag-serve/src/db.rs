use std::{collections::HashMap, str::FromStr};

use sqlx::{
    SqlitePool,
    migrate::{Migrate, Migrator},
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous},
};

static MIGRATOR: Migrator = sqlx::migrate!();
pub struct Database {
    db: SqlitePool,
}

impl Database {
    fn default_options(url: &str) -> anyhow::Result<SqliteConnectOptions> {
        let opts = SqliteConnectOptions::from_str(url)?
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .pragma("journal_size_limit", "67108864") // 64MB
            .pragma("mmap_size", "268435456"); // 256MB
        Ok(opts)
    }

    pub async fn load(url: &str) -> anyhow::Result<Database> {
        let db = SqlitePool::connect_with(Self::default_options(url)?).await?;

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
        let opts: SqliteConnectOptions = Self::default_options(url)?;
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
