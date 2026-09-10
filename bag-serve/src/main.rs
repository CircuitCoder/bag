use std::{ffi::OsString, future::IntoFuture, net::IpAddr, path::PathBuf};

use clap::{Parser, Subcommand};
use tracing_indicatif::IndicatifLayer;
use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};

use crate::db::Database;

mod db;
mod filter;
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

        /// Number of concurrent thumbnail generation tasks. Default is 4. 0 = unlimited.
        #[clap(long, default_value_t = 4)]
        thumb_gen_concurrency: usize,

        /// Rescan and watch this directory, relative to --root, while serving.
        #[clap(short, long)]
        watch: Option<OsString>,
    },
    Upgrade,
    Rescan {
        #[clap(default_value = "")]
        base: OsString, // OsString opt-out non-empty check

        #[clap(short, long)]
        watch: bool,
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
        Command::Serve {
            bind,
            port,
            thumb_gen_concurrency,
            watch,
        } => {
            let app = serve::build(db.clone(), args.root.clone(), thumb_gen_concurrency);
            let binder = tokio::net::TcpListener::bind((bind, port)).await?;
            let server = axum::serve(binder, app);

            if let Some(base) = watch {
                let (server_shutdown_tx, server_shutdown_rx) = tokio::sync::oneshot::channel();
                let (watch_shutdown_tx, watch_shutdown_rx) = tokio::sync::oneshot::channel();
                let mut server_shutdown_tx = Some(server_shutdown_tx);
                let mut watch_shutdown_tx = Some(watch_shutdown_tx);
                let server = server
                    .with_graceful_shutdown(async move {
                        let _ = server_shutdown_rx.await;
                    })
                    .into_future();
                let watcher = scan::watch(&args.root, &base, &db, watch_shutdown_rx);
                tokio::pin!(server);
                tokio::pin!(watcher);

                tokio::select! {
                    result = &mut server => {
                        let _ = watch_shutdown_tx.take().unwrap().send(());
                        watcher.await?;
                        result?;
                    }
                    result = &mut watcher => {
                        let _ = server_shutdown_tx.take().unwrap().send(());
                        server.await?;
                        result?;
                        anyhow::bail!("Filesystem watcher stopped unexpectedly");
                    }
                    result = tokio::signal::ctrl_c() => {
                        result?;
                        let _ = watch_shutdown_tx.take().unwrap().send(());
                        let _ = server_shutdown_tx.take().unwrap().send(());
                        let (server_result, watcher_result) = tokio::join!(&mut server, &mut watcher);
                        server_result?;
                        watcher_result?;
                    }
                }
            } else {
                server.await?;
            }
        }
        Command::Rescan { base, watch } => {
            if watch {
                let (tx, rx) = tokio::sync::oneshot::channel();
                tokio::spawn(async move {
                    tokio::signal::ctrl_c().await.unwrap();
                    tx.send(()).unwrap();
                });
                scan::watch(&args.root, &base, &db, rx).await?;
            } else {
                scan::rescan(&args.root, &base, &db).await?;
            }
        }
        _ => unreachable!(),
    }

    Ok(())
}
