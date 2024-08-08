mod context_extraction;
mod gcra;
mod handler;
mod invocation_builder;
mod message_cache;
mod rate_limit_config;

use std::{
	collections::HashSet,
	num::NonZeroU32,
	str::FromStr,
	time::Duration,
};

use async_openai::{
	config::OpenAIConfig,
	Client,
};
use chrono::{
	DateTime,
	Utc,
};
use entity::user;
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
		CacheHttp,
		ChannelId,
		ClientBuilder,
		CreateAllowedMentions,
		FullEvent,
		GatewayIntents,
		User,
	},
	Framework,
	FrameworkContext,
	FrameworkError,
	FrameworkOptions,
};
use rand::random;
use sea_orm::{
	ActiveModelTrait,
	ActiveValue::Set,
	ColumnTrait,
	ConnectOptions,
	ConnectionTrait,
	Database,
	DatabaseConnection,
	EntityTrait,
	QueryFilter,
};
use tera::Tera;
use tokio::sync::Mutex;
use tracing::{
	error,
	info,
	info_span,
	trace,
	Instrument,
};

use crate::{
	context_extraction::InvocationContextSettings,
	gcra::GCRAConfig,
	handler::{
		admin,
		admin::get_blacklist_for_user,
		completion::handle_completion,
		opt_out,
	},
	message_cache::MessageCache,
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

	#[envconfig(from = "MODEL")]
	model: String,

	#[envconfig(from = "DISCORD_TOKEN")]
	discord_token: String,

	#[envconfig(from = "DATABASE_URL")]
	database_url: String,

	#[envconfig(from = "TEMPLATE_DIR", default = "templates")]
	template_dir: String,

	#[envconfig(from = "RATE_LIMIT_CONFIG", default = "rate_limits.toml")]
	rate_limit_config: String,

	#[envconfig(from = "OPT_OUT_LOCKOUT", default = "30d")]
	opt_out_lockout: ParsedDuration,

	#[envconfig(from = "WHITELIST", default = "")]
	whitelist: Whitelist,
}

struct ParsedDuration(Duration);
impl FromStr for ParsedDuration {
	type Err = Report;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		// we use humantime to parse duration
		humantime::parse_duration(s)
			.into_diagnostic()
			.wrap_err("failed to parse duration")
			.map(ParsedDuration)
	}
}

/// A whitelist of Discord snowflake IDs.
///
/// Ids can be for channels, guilds or categories.
struct Whitelist {
	ids: HashSet<u64>,
}
impl Whitelist {
	/// Checks recursively if the given channel id is in the whitelist.
	///
	/// This will check the channel itself and all parent categories up to and including the guild.
	pub async fn contains(&self, channel_id: ChannelId, http: &impl CacheHttp) -> Result<bool> {
		// direct hit
		if self.ids.contains(&channel_id.get()) {
			return Ok(true);
		}

		// check if channel is thread and check parent
		let channel = channel_id
			.to_channel(http)
			.await
			.into_diagnostic()
			.wrap_err("failed to get channel")?;

		let channel = channel.guild();
		let channel = match channel {
			Some(channel) => channel,

			// if channel is not in a guild, we can't check for parent categories
			None => return Ok(false),
		};

		// walk up the parent relationship
		let mut channel = channel;
		while let Some(parent) = channel.parent_id {
			if self.ids.contains(&parent.get()) {
				return Ok(true);
			}

			let parent = parent
				.to_channel(http)
				.await
				.into_diagnostic()
				.wrap_err("failed to get parent channel")?;

			// update channel to parent
			channel = parent.guild().expect("parent is not a guild somehow");
		}

		// finally check the guild
		if self.ids.contains(&channel.guild_id.get()) {
			return Ok(true);
		}

		Ok(false)
	}
}

impl FromStr for Whitelist {
	type Err = Report;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		let ids = s
			.split(',')
			.map(|s| s.parse().into_diagnostic().wrap_err("failed to parse channel id"))
			.collect::<Result<HashSet<_>, _>>()
			.wrap_err("failed to parse channel whitelist")?;

		Ok(Whitelist {
			ids,
		})
	}
}

struct AppState {
	tera: Tera,
	openai_client: Client<OpenAIConfig>,
	model: String,
	db: DatabaseConnection,
	path_rate_limits: Mutex<PathRateLimits>,
	context_settings: InvocationContextSettings,
	whitelist: Whitelist,
	opt_out_lockout: Duration,
}
type Context<'a> = poise::Context<'a, AppState, Report>;

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

	let mut commands = vec![help(), opt_out::opt_out_dialogue()];
	admin::register_commands(&mut commands);

	// setup discord client with serenity
	let poise_options = FrameworkOptions {
		commands,
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
		event_handler: |ctx, ev, framework, app| Box::pin(discord_listener(ctx, framework, ev, app)),
		..Default::default()
	};

	let framework = Framework::builder()
		.setup(move |_ctx, _ready, _framework| {
			Box::pin(async move {
				Ok(AppState {
					tera,
					model: env_config.model,
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
					whitelist: env_config.whitelist,
					opt_out_lockout: env_config.opt_out_lockout.0,
				})
			})
		})
		.options(poise_options)
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

lazy_static! {
	static ref GLOBAL_RATE_LIMIT: Mutex<(Option<DateTime<Utc>>, GCRAConfig)> = Mutex::new((
		None,
		GCRAConfig::new(Duration::from_secs(1), NonZeroU32::new(100).unwrap(), None)
	));
}

async fn discord_listener<'a>(
	ctx: &'a poise::serenity_prelude::Context,
	framework: FrameworkContext<'_, AppState, Report>,
	ev: &'a FullEvent,
	app: &'a AppState,
) -> Result<()> {
	match ev {
		FullEvent::Message {
			new_message,
		} => {
			// a large in-memory rate limit for all messages, to prevent overloading the bot
			{
				let mut global_rate_limit = GLOBAL_RATE_LIMIT.lock().await;
				let (state, gcre) = &mut *global_rate_limit;
				match gcre.check(Utc::now(), *state, NonZeroU32::new(1).unwrap()) {
					Some(new_state) => {
						*state = Some(new_state);
					},
					None => return Ok(()),
				}
			}

			let span = info_span!("message", author = %new_message.author.name, content = %new_message.content);

			// drop messages from blacklisted users
			if get_blacklist_for_user(&app.db, new_message.author.id).await?.is_some() {
				return Ok(());
			}

			let our_id = ctx.cache.current_user().id;

			// ignore messages from bots or ourselves (we are a bot, but just in case)
			if new_message.author.bot || new_message.author.id == our_id {
				return Ok(());
			}

			// we only reply to message if user obviously wants us to
			let concerned = {
				let mentioned = new_message.mentions_user_id(our_id);
				let in_dm = new_message.guild_id.is_none();
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

			if let Err(e) = handle_completion(ctx, framework, app, new_message).instrument(span).await {
				error!("Error handling completion: {:?}", e);
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
			let message_cache = MessageCache::new(&app.db);
			message_cache.invalidate(&new.id).await?;
		},
		FullEvent::MessageDelete {
			deleted_message_id, ..
		} => {
			let message_cache = MessageCache::new(&app.db);
			message_cache.invalidate(deleted_message_id).await?;
		},
		_ => {},
	}

	Ok(())
}

/// Provides help for the bot.
#[poise::command(prefix_command, track_edits, owners_only)]
pub async fn help(
	ctx: Context<'_>,
	#[description = "Specific command to show help about"] command: Option<String>,
) -> Result<()> {
	poise::builtins::help(ctx, command.as_deref(), Default::default())
		.await
		.into_diagnostic()
		.wrap_err("failed to send help message")?;
	Ok(())
}

pub async fn user_from_db_or_create<C: ConnectionTrait>(db: &C, user: &User) -> Result<user::Model> {
	let id = user.id.get();
	let name = &user.name;

	let user = entity::prelude::User::find()
		.filter(user::Column::DiscordUserId.eq(id))
		.one(db)
		.await
		.into_diagnostic()
		.wrap_err("failed to check for user in database")?;

	if let Some(user) = user {
		Ok(user)
	} else {
		let uuid = uuid::Builder::from_random_bytes(random()).into_uuid();
		let user = user::ActiveModel {
			uuid: Set(Vec::from(uuid)),
			discord_user_id: Set(id),
			username: Set(name.to_owned()),
			..Default::default()
		};
		let user = user.insert(db).await.into_diagnostic().wrap_err("failed to create user")?;
		Ok(user)
	}
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
