use context::ApiContext;
use permissions::ApiPermission;
use rfd_model::{
    permissions::{Caller, Permissions},
    storage::postgres::PostgresStore,
    ApiKey, ApiUser,
};
use server::{server, ServerConfig};
use std::{
    error::Error,
    net::{SocketAddr, SocketAddrV4},
    sync::Arc,
};
use tracing_appender::non_blocking::NonBlocking;
use tracing_subscriber::EnvFilter;

use crate::{
    config::{AppConfig, ServerLogFormat},
    endpoints::login::oauth::{
        github::GitHubOAuthProvider, google::GoogleOAuthProvider, OAuthProviderName,
    },
    initial_data::InitialData,
};

mod authn;
mod config;
mod context;
mod endpoints;
mod error;
mod initial_data;
mod mapper;
mod permissions;
mod server;
mod util;

pub type ApiCaller = Caller<ApiPermission>;
pub type ApiPermissions = Permissions<ApiPermission>;
pub type User = ApiUser<ApiPermission>;
pub type UserToken = ApiKey<ApiPermission>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut args = std::env::args();
    let _ = args.next();
    let config_path = args.next();

    let config = AppConfig::new(config_path.map(|path| vec![path]))?;

    let (writer, _guard) = if let Some(log_directory) = config.log_directory {
        let file_appender = tracing_appender::rolling::daily(log_directory, "rfd-api.log");
        tracing_appender::non_blocking(file_appender)
    } else {
        NonBlocking::new(std::io::stdout())
    };

    let subscriber = tracing_subscriber::fmt()
        .with_file(false)
        .with_line_number(false)
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(writer);

    match config.log_format {
        ServerLogFormat::Json => subscriber.json().init(),
        ServerLogFormat::Pretty => subscriber.pretty().init(),
    };

    let mut context = ApiContext::new(
        config.public_url,
        Arc::new(
            PostgresStore::new(&config.database_url)
                .await
                .map_err(|err| {
                    format!("Failed to establish initial database connection: {:?}", err)
                })?,
        ),
        config.jwt,
        config.keys,
        config.search,
    )
    .await?;

    let init_data = InitialData::new(config.initial_mappers.map(|p| vec![p]))?;
    init_data.initialize(&context).await?;

    if let Some(github) = config.authn.oauth.github {
        context.insert_oauth_provider(
            OAuthProviderName::GitHub,
            Box::new(move || {
                Box::new(GitHubOAuthProvider::new(
                    github.device.client_id.clone(),
                    github.device.client_secret.clone(),
                    github.web.client_id.clone(),
                    github.web.client_secret.clone(),
                ))
            }),
        )
    }

    if let Some(google) = config.authn.oauth.google {
        context.insert_oauth_provider(
            OAuthProviderName::Google,
            Box::new(move || {
                Box::new(GoogleOAuthProvider::new(
                    google.device.client_id.clone(),
                    google.device.client_secret.clone(),
                    google.web.client_id.clone(),
                    google.web.client_secret.clone(),
                ))
            }),
        )
    }

    tracing::debug!(?config.spec, "Spec configuration");

    let config = ServerConfig {
        context,
        server_address: SocketAddr::V4(SocketAddrV4::new("0.0.0.0".parse()?, config.server_port)),
        spec_output: config.spec,
    };

    let server = server(config)?.start();

    server.await?;

    Ok(())
}
