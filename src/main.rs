mod completion;
mod context_extraction;
mod gcra;
mod invocation_builder;
mod rate_limit_config;

use std::time::Duration;

use async_openai::{
	config::OpenAIConfig,
	Client,
};
use envconfig::Envconfig;
use lazy_static::lazy_static;
use miette::{
	IntoDiagnostic,
	Report,
	Result,
	WrapErr,
};
use migration::{
	Migrator,
	MigratorTrait,
};
use poise::{
	serenity_prelude::{
		ClientBuilder,
		CreateAllowedMentions,
		FullEvent,
		GatewayIntents,
	},
	Framework,
	FrameworkError,
	FrameworkOptions,
};
use sea_orm::{
	ConnectOptions,
	Database,
	DatabaseConnection,
};
use tera::Tera;
use tokio::sync::Mutex;
use tracing::{
	debug,
	error,
	info,
	info_span,
	trace,
	Instrument,
};

use crate::{
	completion::handle_completion,
	context_extraction::InvocationContextSettings,
	rate_limit_config::{
		PathRateLimits,
		RateLimitConfig,
	},
};

lazy_static! {
	pub static ref APP_VERSION: semver::Version = semver::Version::parse(env!("CARGO_PKG_VERSION"))
		.into_diagnostic()
		.wrap_err("failed to parse version")
		.unwrap();
	pub static ref APP_NAME: String = env!("CARGO_PKG_NAME").into();
}

#[derive(Envconfig)]
struct EnvConfig {
	#[envconfig(from = "OPENAI_TOKEN")]
	openai_token: String,

	#[envconfig(from = "DISCORD_TOKEN")]
	discord_token: String,

	#[envconfig(from = "DATABASE_URL")]
	database_url: String,

	#[envconfig(from = "TEMPLATE_DIR", default = "templates")]
	template_dir: String,

	#[envconfig(from = "RATE_LIMIT_CONFIG", default = "rate_limits.toml")]
	rate_limit_config: String,
}

struct AppState {
	tera: Tera,
	openai_client: Client<OpenAIConfig>,
	db: DatabaseConnection,
	path_rate_limits: Mutex<PathRateLimits>,
	context_settings: InvocationContextSettings,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
	tracing_subscriber::fmt::init();
	info!(version = %*APP_VERSION, "Starting {}...", *APP_NAME);

	let env_config = EnvConfig::init_from_env()
		.into_diagnostic()
		.wrap_err("failed to load environment variables")?;

	let tera = {
		let template_dir = format!("{}/{}", env_config.template_dir, "*.txt");
		Tera::new(&template_dir)
			.into_diagnostic()
			.wrap_err("failed to load templates")?
	};

	let openai_client = {
		let config = OpenAIConfig::new().with_api_key(&env_config.openai_token);
		Client::with_config(config)
	};

	let db = {
		let mut opt = ConnectOptions::new(env_config.database_url);
		opt
			.max_connections(5)
			.min_connections(1)
			.connect_timeout(Duration::from_secs(5))
			.acquire_timeout(Duration::from_secs(10))
			.idle_timeout(Duration::from_secs(60))
			.sqlx_logging(false);

		let db = Database::connect(opt)
			.await
			.into_diagnostic()
			.wrap_err("failed to connect to database")?;

		db.ping().await.into_diagnostic().wrap_err("failed to ping database")?;
		Migrator::up(&db, None)
			.await
			.into_diagnostic()
			.wrap_err("failed to run migrations")?;
		db
	};

	let path_rate_limits: PathRateLimits = {
		// start background worker to periodically persist rate limiter state
		let rate_limit_config =
			RateLimitConfig::from_file(&env_config.rate_limit_config).wrap_err("failed to load rate limit config")?;

		rate_limit_config.into()
	};

	// setup discord client with serenity
	let poise_options = FrameworkOptions {
		pre_command: |ctx| {
			Box::pin(async move {
				let invocation = ctx.invocation_string();
				trace!(sender = %ctx.author(), invocation = invocation, "Executing command...");
			})
		},
		on_error: |error: FrameworkError<'_, AppState, Report>| {
			Box::pin(async move {
				let err = match &error {
					FrameworkError::Setup {
						error, ..
					} => Some(error),
					FrameworkError::EventHandler {
						error, ..
					} => Some(error),
					_ => None,
				};

				// custom logging can use miette report and provides way more details, if available, use it, otherwise let poise handle
				// it
				if let Some(err) = err {
					error!(error = ?err, "generic error in bot framework");
					return;
				}

				error!("generic error in bot framework: {}", error);
				if let Err(e) = poise::builtins::on_error(error).await {
					error!("Error while notifying user about error: {}", e);
				}
			})
		},
		// block all mentions by default
		allowed_mentions: Some(CreateAllowedMentions::new().empty_roles().empty_users().replied_user(true)),
		manual_cooldowns: true,
		skip_checks_for_owners: false,
		event_handler: |ctx, ev, _framework, app| Box::pin(discord_listener(ctx, ev, app)),
		..Default::default()
	};

	let framework = Framework::builder()
		.setup(move |_ctx, _ready, _framework| {
			Box::pin(async move {
				Ok(AppState {
					tera,
					openai_client,
					db,
					path_rate_limits: Mutex::new(path_rate_limits),
					context_settings: InvocationContextSettings {
						max_token_count: 2000,
						max_channel_history: Some(10),
						reply_chain_depth: Some(4),
						reply_chain_window: Some(5),
						reply_chain_max_token_count: Some(1000),
					},
				})
			})
		})
		.options(poise_options)
		.initialize_owners(false)
		.build();

	ClientBuilder::new(
		&env_config.discord_token,
		GatewayIntents::MESSAGE_CONTENT | GatewayIntents::DIRECT_MESSAGES | GatewayIntents::GUILD_MESSAGES | GatewayIntents::GUILDS,
	)
	.framework(framework)
	.await
	.into_diagnostic()
	.wrap_err("failed to create discord client")
	.unwrap()
	.start_autosharded()
	.await
	.into_diagnostic()
	.wrap_err("failed to start discord client")?;

	Ok(())
}

async fn discord_listener<'a>(ctx: &'a poise::serenity_prelude::Context, ev: &'a FullEvent, app: &'a AppState) -> Result<()> {
	match ev {
		FullEvent::Message {
			new_message,
		} => {
			let span = info_span!("message", author = %new_message.author.name, content = %new_message.content);

			let our_id = ctx.cache.current_user().id;

			// ignore messages from bots or ourselves (we are a bot, but just in case)
			if new_message.author.bot || new_message.author.id == our_id {
				return Ok(());
			}

			// we only reply to message if user obviously wants us to
			let concerned = {
				// TODO: if user opted out, we instead send a message that we won't reply

				let mentioned = new_message.mentions_user_id(our_id);
				let in_dm = new_message.is_private();
				let replied_to_us = new_message
					.referenced_message
					.as_ref()
					.map(|m| m.author.id == our_id)
					.unwrap_or(false);
				mentioned || in_dm || replied_to_us
			};

			if !concerned {
				return Ok(());
			}

			if let Err(e) = handle_completion(ctx, app, new_message).instrument(span).await {
				new_message
					.reply_ping(ctx, format!("Error: {}", e))
					.await
					.into_diagnostic()
					.wrap_err("failed to send error message")?;
			}
		},
		FullEvent::MessageUpdate {
			new: Some(new), ..
		} => {
			// TODO: invalidate moderation cache for message
			debug!("message {} updated, invalidating cache", new.id);
		},
		_ => {},
	}

	Ok(())
}

#[cfg(test)]
mod tests {
	use ctor::ctor;

	#[ctor]
	fn init_tests() {
		// set RUST_LOG to trace for tests
		std::env::set_var("RUST_LOG", "trace");

		tracing_subscriber::fmt::init();
	}
}
