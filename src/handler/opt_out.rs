use std::time::Duration;

use entity::user::Model;
use miette::{
	miette,
	IntoDiagnostic,
	Result,
	WrapErr,
};
use poise::{
	serenity_prelude::{
		ButtonStyle,
		ComponentInteractionCollector,
		CreateActionRow,
		CreateButton,
		CreateEmbed,
		CreateInteractionResponseFollowup,
		ReactionType,
	},
	CreateReply,
};
use sea_orm::{
	ActiveModelTrait,
	ActiveValue::Set,
	IntoActiveModel,
};

use crate::{
	message_cache::MessageCache,
	user_from_db_or_create,
	Context,
};

#[derive(PartialEq)]
enum OptOutState {
	/// Indicates that user has not opted out.
	In,

	/// Indicates that user has opted out and is allowed to opt back in.
	Out,

	/// Indicates that user has opted out recently and is not allowed to opt back in.
	OutRecently,
}

const OPT_OUT_TEXT: &str = r#"
**Opt-out is potentially permanent or at least for a very long time!**

Opting out is for people who don't want their messages to be processed by OpenAI. Once you opt-out, the bot will no longer:

* Respond to your messages
* Include your messages in context sent to OpenAI
* Cache your messages in the database

Any existing messages will be deleted from the database. Your logged invocations will remain, to prevent abuse.

Opting back in is locked behind a very long waiting period to prevent abuse.
You either engage with the bot at all times, or not at all.
"#;

const OPT_IN_AVAILABLE_TEXT: &str = r#"
It has been a while since you opted out. If you have changed your mind, you can opt back in.

You will be able to opt back out immediately after opting back in.
"#;

const OPT_IN_NOT_AVAILABLE_TEXT: &str = r#"
You have recently opted out. You are not allowed to opt back in at this time.
"#;

/// Allows to manage opt-out state in order to prevent bot from processing messages of users.
#[poise::command(slash_command, ephemeral, rename = "opt-out")]
pub async fn opt_out_dialogue(ctx: Context<'_>) -> Result<()> {
	// bot owner can never opt out
	if ctx.framework().options.owners.contains(&ctx.author().id) {
		ctx
			.reply("Bot owner can not opt out.")
			.await
			.into_diagnostic()
			.wrap_err("failed to inform bot owner about inability to opt out")?;
		return Ok(());
	}

	let id = ctx.id();
	let app = ctx.data();
	let lockout_duration = &app.opt_out_lockout;
	let db_user = user_from_db_or_create(&app.db, ctx.author()).await?;

	let state = calculate_state(lockout_duration, db_user);

	let reply = match state {
		OptOutState::In => create_dialogue(
			OPT_OUT_TEXT,
			CreateButton::new(format!("{id}:opt-out"))
				.emoji(ReactionType::Unicode("🚫".to_string()))
				.style(ButtonStyle::Danger)
				.label("Yes, stop processing my messages!"),
		),
		OptOutState::Out => create_dialogue(
			OPT_IN_AVAILABLE_TEXT,
			CreateButton::new(format!("{id}:opt-in"))
				.emoji(ReactionType::Unicode("✅".to_string()))
				.style(ButtonStyle::Success)
				.label("Yes, you can process my messages!"),
		),
		OptOutState::OutRecently => create_dialogue(
			OPT_IN_NOT_AVAILABLE_TEXT,
			CreateButton::new(format!("{id}:noop"))
				.emoji(ReactionType::Unicode("🔒".to_string()))
				.style(ButtonStyle::Secondary)
				.disabled(true)
				.label("You have recently opted out."),
		),
	};

	ctx
		.send(reply)
		.await
		.into_diagnostic()
		.wrap_err("failed to send opt-out dialogue")?;

	// now we wait for user, wait is limited to 2 minutes, after that user has to start over
	let response = ComponentInteractionCollector::new(ctx)
		.author_id(ctx.author().id)
		.channel_id(ctx.channel_id())
		.timeout(Duration::from_secs(120))
		.filter(move |i| i.data.custom_id.starts_with(&format!("{}:", id)))
		.await;

	if let Some(interaction) = response {
		// user could modify state from different interaction, so we need to recalculate it, on mismatch, we return an error
		let db_user = user_from_db_or_create(&app.db, ctx.author()).await?;
		if calculate_state(lockout_duration, db_user.clone()) != state {
			return Err(miette!("state of user has changed during interaction"));
		}

		let opt_out = interaction.data.custom_id.ends_with(":opt-out");
		let opt_in = interaction.data.custom_id.ends_with(":opt-in");

		// db interaction could be slow or fail completely
		interaction
			.defer_ephemeral(ctx)
			.await
			.into_diagnostic()
			.wrap_err("failed to defer ephemeral response")?;

		let mut db_user = db_user.into_active_model();
		let response = if opt_out {
			db_user.opt_out_since = Set(Some(chrono::Utc::now()));
			"Opted out successfully! You will no longer receive responses from the bot."
		} else if opt_in {
			db_user.opt_out_since = Set(None);
			"Opted back in successfully! You will now receive responses from the bot."
		} else {
			// user somehow managed to click on the disabled button, thanks discord
			return Ok(());
		};

		let cache = MessageCache::new(&app.db);
		cache.delete_from_user(ctx.author().id).await?;

		db_user
			.update(&app.db)
			.await
			.into_diagnostic()
			.wrap_err("failed to update user")?;

		interaction
			.create_followup(ctx, CreateInteractionResponseFollowup::new().content(response))
			.await
			.into_diagnostic()
			.wrap_err("failed to send followup after opt-out dialogue")?;
	}

	Ok(())
}

fn calculate_state(lockout_duration: &Duration, db_user: Model) -> OptOutState {
	if let Some(opt_out_since) = db_user.opt_out_since {
		let now = chrono::Utc::now();
		let duration = now - opt_out_since;
		let duration = duration.to_std().expect("unable to convert chrono duration to std duration");
		if &duration < lockout_duration {
			OptOutState::OutRecently
		} else {
			OptOutState::Out
		}
	} else {
		OptOutState::In
	}
}

fn create_dialogue(text: &str, button: CreateButton) -> CreateReply {
	let components = vec![CreateActionRow::Buttons(vec![button])];

	CreateReply::default().ephemeral(true).components(components).embed(
		CreateEmbed::new()
			.title("Warning: read carefully")
			.color(0xff0000)
			.description(text),
	)
}
