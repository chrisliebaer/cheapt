use entity::{
	blacklist,
	user,
};
use miette::{
	IntoDiagnostic,
	Report,
	Result,
	WrapErr,
};
use poise::{
	serenity_prelude::{
		CreateEmbed,
		Mentionable,
		UserId,
	},
	Command,
	CreateReply,
};
use sea_orm::{
	ActiveModelTrait,
	ActiveValue::Set,
	ColumnTrait,
	DatabaseConnection,
	EntityTrait,
	ModelTrait,
	QueryFilter,
};

use crate::{
	AppState,
	Context,
};

pub fn register_commands(commands: &mut Vec<Command<AppState, Report>>) {
	commands.push(admin());
}

/// Only usable by bot owner, this command allows access to configuration and other administrative tasks.
#[poise::command(
	prefix_command,
	owners_only,
	dm_only,
	subcommand_required,
	subcommands("user", "register", "guilds")
)]
async fn admin(_ctx: Context<'_>) -> Result<()> {
	unreachable!("This command is only available as a subcommand")
}

/// Commands for user management.
#[poise::command(
	prefix_command,
	owners_only,
	dm_only,
	subcommand_required,
	subcommands("user_status", "user_blacklist")
)]
async fn user(_ctx: Context<'_>) -> Result<()> {
	unreachable!("This command is only available as a subcommand")
}

/// Outputs the status of a user.
#[poise::command(prefix_command, owners_only, dm_only, rename = "status")]
async fn user_status(ctx: Context<'_>, user: UserId) -> Result<()> {
	let db = &ctx.data().db;

	let db_user = entity::prelude::User::find()
		.filter(user::Column::DiscordUserId.eq(user.get()))
		.one(db)
		.await
		.into_diagnostic()?;

	if let Some(db_user) = db_user {
		// human readable output
		let uuid = uuid::Uuid::from_slice(db_user.uuid.as_slice())
			.expect("malformed UUID in database")
			.as_hyphenated()
			.to_string();
		let opt_out = db_user
			.opt_out_since
			.map(|date| date.format("%Y-%m-%d %H:%M:%S").to_string())
			.unwrap_or("No".to_string());

		ctx
			.send(
				CreateReply::default().embed(CreateEmbed::new().title(format!("User {}", db_user.username)).fields(vec![
					("UUID", uuid, true),
					("Discord User ID", db_user.discord_user_id.to_string(), true),
					("Opt Out", opt_out, true),
				])),
			)
			.await
			.into_diagnostic()
			.wrap_err("failed to send message")?;
	} else {
		ctx
			.reply(format!("User {} is not known to bot.", user.mention()))
			.await
			.into_diagnostic()
			.wrap_err("failed to send message")?;
	}

	Ok(())
}

/// Commands for managing user blacklist.
#[poise::command(
	prefix_command,
	owners_only,
	dm_only,
	rename = "blacklist",
	subcommand_required,
	subcommands("user_blacklist_set", "user_blacklist_get")
)]
async fn user_blacklist(_ctx: Context<'_>) -> Result<()> {
	unreachable!("This command is only available as a subcommand")
}

/// Checks blacklist status of a user.
#[poise::command(prefix_command, owners_only, dm_only, rename = "get")]
async fn user_blacklist_get(ctx: Context<'_>, user: UserId) -> Result<(), Report> {
	let db = &ctx.data().db;

	let blacklist_entry = entity::prelude::Blacklist::find()
		.filter(blacklist::Column::DiscordUserId.eq(user.get()))
		.one(db)
		.await
		.into_diagnostic()?;

	if let Some(blacklist_entry) = blacklist_entry {
		let created_at = blacklist_entry.created_at.format("%Y-%m-%d %H:%M:%S").to_string();

		ctx
			.send(
				CreateReply::default().embed(CreateEmbed::new().title(format!("User {}", user.mention())).fields(vec![
					("Blacklisted At", created_at, true),
					("Reason", blacklist_entry.reason, true),
				])),
			)
			.await
			.into_diagnostic()
			.wrap_err("failed to send message")?;
	} else {
		ctx
			.reply(format!("User {} is not blacklisted.", user.mention()))
			.await
			.into_diagnostic()
			.wrap_err("failed to send message")?;
	}

	Ok(())
}

/// Updates blacklist status of a user.
#[poise::command(prefix_command, owners_only, dm_only, rename = "set")]
async fn user_blacklist_set(ctx: Context<'_>, user: UserId, blacklisted: bool, #[rest] reason: String) -> Result<(), Report> {
	let db = &ctx.data().db;

	// check if target user is owner, as owners cannot be blacklisted
	if ctx.framework().options.owners.contains(&user) {
		ctx
			.reply("Owners cannot be blacklisted.")
			.await
			.into_diagnostic()
			.wrap_err("failed to send message")?;
		return Ok(());
	}

	let blacklist_entry = get_blacklist_for_user(db, user).await?;

	if blacklisted {
		// check if user is already blacklisted and report an error if so
		if blacklist_entry.is_some() {
			ctx
				.reply(format!("User {} is already blacklisted.", user.mention()))
				.await
				.into_diagnostic()
				.wrap_err("failed to send message")?;
			return Ok(());
		}

		// ensure reason is not empty or just whitespace
		if reason.trim().is_empty() {
			ctx.reply("Reason must not be empty.").await.into_diagnostic()?;
			return Ok(());
		}

		// create new blacklist entry
		let new_blacklist_entry = blacklist::ActiveModel {
			discord_user_id: Set(user.get()),
			reason: Set(reason),
			..Default::default()
		};
		new_blacklist_entry
			.insert(db)
			.await
			.into_diagnostic()
			.wrap_err("failed to insert blacklist entry")?;

		ctx
			.reply(format!("User {} has been blacklisted.", user.mention()))
			.await
			.into_diagnostic()
			.wrap_err("failed to send message")?;
	} else {
		// check if user is blacklisted, if not, report an error
		if blacklist_entry.is_none() {
			ctx
				.reply(format!("User {} is not blacklisted.", user.mention()))
				.await
				.into_diagnostic()
				.wrap_err("failed to send message")?;
			return Ok(());
		}

		// remove blacklist entry
		let blacklist_entry = blacklist_entry.unwrap();
		blacklist_entry
			.delete(db)
			.await
			.into_diagnostic()
			.wrap_err("failed to delete blacklist entry")?;

		ctx
			.reply(format!("User {} has been removed from blacklist.", user.mention()))
			.await
			.into_diagnostic()
			.wrap_err("failed to send message")?;
	}

	Ok(())
}

pub async fn get_blacklist_for_user(db: &DatabaseConnection, user: UserId) -> Result<Option<entity::blacklist::Model>> {
	let blacklist = entity::prelude::Blacklist::find()
		.filter(entity::blacklist::Column::DiscordUserId.eq(user.get()))
		.one(db)
		.await
		.into_diagnostic()
		.wrap_err("failed to fetch blacklist from database")?;
	Ok(blacklist)
}

/// Opens a dialogue to manage registered application commands.
#[poise::command(prefix_command, owners_only, dm_only)]
async fn register(ctx: Context<'_>) -> Result<()> {
	poise::builtins::register_application_commands_buttons(ctx)
		.await
		.into_diagnostic()
		.wrap_err("failed to register application commands")
}

/// Lists all guilds the bot is in.
#[poise::command(prefix_command, owners_only, dm_only)]
async fn guilds(ctx: Context<'_>) -> Result<(), Report> {
	poise::builtins::servers(ctx)
		.await
		.into_diagnostic()
		.wrap_err("failed to list servers")
}
