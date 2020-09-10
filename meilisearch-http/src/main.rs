use std::path::PathBuf;
use std::sync::Arc;
use std::{env, thread};

use actix_cors::Cors;
use actix_web::{middleware, HttpServer};
use log::info;
use main_error::MainError;
use meilisearch_http::helpers::NormalizePath;
use meilisearch_http::raft::{init_raft, RaftConfig};
use meilisearch_http::{create_app, create_app_raft, index_update_callback, snapshot, Data, Opt};
use structopt::StructOpt;
use tokio::fs::File;
use tokio::prelude::*;

mod analytics;

#[cfg(target_os = "linux")]
#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

#[actix_rt::main]
async fn main() -> Result<(), MainError> {
    let opt = Opt::from_args();

    #[cfg(all(not(debug_assertions), feature = "sentry"))]
    let _sentry = sentry::init((
        if !opt.no_sentry {
            Some(opt.sentry_dsn.clone())
        } else {
            None
        },
        sentry::ClientOptions {
            release: sentry::release_name!(),
            ..Default::default()
        },
    ));

    match opt.env.as_ref() {
        "production" => {
            if opt.master_key.is_none() {
                return Err(
                    "In production mode, the environment variable MEILI_MASTER_KEY is mandatory"
                        .into(),
                );
            }

            #[cfg(all(not(debug_assertions), feature = "sentry"))]
            if !opt.no_sentry && _sentry.is_enabled() {
                sentry::integrations::panic::register_panic_handler(); // TODO: This shouldn't be needed when upgrading to sentry 0.19.0. These integrations are turned on by default when using `sentry::init`.
                sentry::integrations::env_logger::init(None, Default::default());
            }
        }
        "development" => {
            env_logger::from_env(env_logger::Env::default().default_filter_or("info")).init();
        }
        _ => unreachable!(),
    }

    match opt.raft_config {
        Some(ref path) => run_raft(&opt, path).await,
        None => run(&opt).await,
    }
}

async fn run_raft(opt: &Opt, raft_config_path: &PathBuf) -> Result<(), MainError> {
    info!("running raft");
    let data = Data::new(opt.clone())?;

    // create subscriber for tracing
    //let subscriber = tracing_subscriber::fmt()
    //// filter spans/events with level TRACE or higher.
    //.with_max_level(tracing::Level::TRACE)
    //// build but do not install the subscriber.
    //.finish();
    //tracing::subscriber::set_global_default(subscriber)?;
    let mut file = File::open(raft_config_path).await?;
    let mut config = String::new();
    file.read_to_string(&mut config).await?;
    let config: RaftConfig = toml::from_str(&config)?;

    let raft = init_raft(config, data.clone()).await?;
    let raft = Arc::new(raft);

    if !opt.no_analytics {
        let analytics_data = data.clone();
        let analytics_opt = opt.clone();
        thread::spawn(move || analytics::analytics_sender(analytics_data, analytics_opt));
    }

    let data_cloned = data.clone();
    data.db.set_update_callback(Box::new(move |name, status| {
        index_update_callback(name, &data_cloned, status);
    }));

    print_launch_resume(&opt, &data);

    let http_server = HttpServer::new(move || {
        create_app_raft(raft.clone())
            .wrap(
                Cors::new()
                    .send_wildcard()
                    .allowed_headers(vec!["content-type", "x-meili-api-key"])
                    .max_age(86_400) // 24h
                    .finish(),
            )
            .wrap(middleware::Logger::default())
            .wrap(middleware::Compress::default())
            .wrap(NormalizePath)
    });

    let server_handle = if let Some(config) = opt.get_ssl_config()? {
        tokio::spawn(
            http_server
                .bind_rustls(opt.http_addr.clone(), config)?
                .run(),
        )
    } else {
        tokio::spawn(http_server.bind(opt.http_addr.clone())?.run())
    };

    tokio::try_join!(server_handle)?.0?;

    Ok(())
}

async fn run(opt: &Opt) -> Result<(), MainError> {
    info!("running normal");
    if let Some(path) = &opt.load_from_snapshot {
        snapshot::load_snapshot(
            &opt.db_path,
            path,
            opt.ignore_snapshot_if_db_exists,
            opt.ignore_missing_snapshot,
        )?;
    }

    let data = Data::new(opt.clone())?;

    if !opt.no_analytics {
        let analytics_data = data.clone();
        let analytics_opt = opt.clone();
        thread::spawn(move || analytics::analytics_sender(analytics_data, analytics_opt));
    }

    let data_cloned = data.clone();
    data.db.set_update_callback(Box::new(move |name, status| {
        index_update_callback(name, &data_cloned, status);
    }));

    if let Some(path) = &opt.snapshot_path {
        snapshot::schedule_snapshot(
            data.clone(),
            &path,
            opt.snapshot_interval_sec.unwrap_or(86400),
        )?;
    }

    print_launch_resume(&opt, &data);

    let http_server = HttpServer::new(move || {
        create_app(&data)
            .wrap(
                Cors::new()
                    .send_wildcard()
                    .allowed_headers(vec!["content-type", "x-meili-api-key"])
                    .max_age(86_400) // 24h
                    .finish(),
            )
            .wrap(middleware::Logger::default())
            .wrap(middleware::Compress::default())
            .wrap(NormalizePath)
    });

    if let Some(config) = opt.get_ssl_config()? {
        http_server
            .bind_rustls(opt.http_addr.clone(), config)?
            .run()
            .await?;
    } else {
        http_server.bind(opt.http_addr.clone())?.run().await?;
    }

    Ok(())
}

pub fn print_launch_resume(opt: &Opt, data: &Data) {
    let ascii_name = r#"
888b     d888          d8b 888 d8b  .d8888b.                                    888
8888b   d8888          Y8P 888 Y8P d88P  Y88b                                   888
88888b.d88888              888     Y88b.                                        888
888Y88888P888  .d88b.  888 888 888  "Y888b.    .d88b.   8888b.  888d888 .d8888b 88888b.
888 Y888P 888 d8P  Y8b 888 888 888     "Y88b. d8P  Y8b     "88b 888P"  d88P"    888 "88b
888  Y8P  888 88888888 888 888 888       "888 88888888 .d888888 888    888      888  888
888   "   888 Y8b.     888 888 888 Y88b  d88P Y8b.     888  888 888    Y88b.    888  888
888       888  "Y8888  888 888 888  "Y8888P"   "Y8888  "Y888888 888     "Y8888P 888  888
"#;

    eprintln!("{}", ascii_name);

    eprintln!("Database path:\t\t{:?}", opt.db_path);
    eprintln!("Server listening on:\t{:?}", opt.http_addr);
    eprintln!("Environment:\t\t{:?}", opt.env);
    eprintln!("Commit SHA:\t\t{:?}", env!("VERGEN_SHA").to_string());
    eprintln!(
        "Build date:\t\t{:?}",
        env!("VERGEN_BUILD_TIMESTAMP").to_string()
    );
    eprintln!(
        "Package version:\t{:?}",
        env!("CARGO_PKG_VERSION").to_string()
    );

    #[cfg(all(not(debug_assertions), feature = "sentry"))]
    eprintln!(
        "Sentry DSN:\t\t{:?}",
        if !opt.no_sentry {
            &opt.sentry_dsn
        } else {
            "Disabled"
        }
    );

    eprintln!(
        "Amplitude Analytics:\t{:?}",
        if !opt.no_analytics {
            "Enabled"
        } else {
            "Disabled"
        }
    );

    eprintln!();

    if data.api_keys.master.is_some() {
        eprintln!("A Master Key has been set. Requests to MeiliSearch won't be authorized unless you provide an authentication key.");
    } else {
        eprintln!("No master key found; The server will accept unidentified requests. \
            If you need some protection in development mode, please export a key: export MEILI_MASTER_KEY=xxx");
    }

    eprintln!();
    eprintln!("Documentation:\t\thttps://docs.meilisearch.com");
    eprintln!("Source code:\t\thttps://github.com/meilisearch/meilisearch");
    eprintln!("Contact:\t\thttps://docs.meilisearch.com/resources/contact.html or bonjour@meilisearch.com");
    eprintln!();
}
