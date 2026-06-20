use std::{net::IpAddr, path::PathBuf};

use axum::{Router, http::Uri};
use bag_fs::fs::FsHandler;
use clap::{Parser, Subcommand};

use crate::db::Database;

mod scan;
mod db;

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
        #[clap(default_value = "/")]
        base: PathBuf
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();
    let db_uri = if let Some(uri) = args.db {
        uri
    } else {
        std::env::var("DATABASE_URL").map_err(|e| {
            anyhow::anyhow!("DATABASE_URL environment variable not set and no --db argument provided: {e}")
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
            let fs = FsHandler::new(args.root.clone());
            let raw_handler = Router::new().fallback(async move |uri: Uri| { fs.handle(uri).await });
            let app = Router::new().nest("/v1/raw", raw_handler);
            let binder = tokio::net::TcpListener::bind((bind, port)).await?;
            axum::serve(binder, app).await?;
        }
        Command::Rescan { base } => {
            scan::rescan(
                &args.root,
                &base,
                &db,
            ).await?;
        }
        _ => unreachable!()
    }

    Ok(())
}
