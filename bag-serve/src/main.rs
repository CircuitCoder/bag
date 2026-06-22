use std::{ffi::OsString, net::IpAddr, path::PathBuf};

use clap::{Parser, Subcommand};
use tracing_indicatif::IndicatifLayer;
use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};

use crate::db::Database;

mod db;
mod scan;
mod serve;

#[derive(Parser)]
struct Args {
    #[clap(short, long)]
    db: Option<String>,

    #[clap(short, long)]
    root: PathBuf,

    #[clap(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Serve {
        #[clap(short, long, default_value = "0.0.0.0")]
        bind: IpAddr,

        #[clap(short, long, default_value = "6102")]
        port: u16,
    },
    Upgrade,
    Rescan {
        #[clap(default_value = "")]
        base: OsString, // OsString opt-out non-empty check
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let indicatif_layer = IndicatifLayer::new();

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(indicatif_layer.get_stderr_writer())
                .with_filter(tracing_subscriber::filter::EnvFilter::from_default_env()),
        )
        .with(indicatif_layer)
        .init();

    let args = Args::parse();
    let db_uri = if let Some(uri) = args.db {
        uri
    } else {
        std::env::var("DATABASE_URL").map_err(|e| {
            anyhow::anyhow!(
                "DATABASE_URL environment variable not set and no --db argument provided: {e}"
            )
        })?
    };

    if let Command::Upgrade = args.command {
        Database::setup(&db_uri).await?;
        println!("Database setup complete");
        return Ok(());
    }

    let db = Database::load(&db_uri).await?;

    match args.command {
        Command::Serve { bind, port } => {
            let app = serve::build(db, args.root);
            let binder = tokio::net::TcpListener::bind((bind, port)).await?;
            axum::serve(binder, app).await?;
        }
        Command::Rescan { base } => {
            scan::rescan(&args.root, &base, &db).await?;
        }
        _ => unreachable!(),
    }

    Ok(())
}
