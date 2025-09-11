use std::collections::{
	HashMap,
	HashSet,
};

use llm::{
	FunctionCall,
	ToolCall,
	chat::{
		ChatMessage,
		ChatRole,
	},
};
use log::debug;
use miette::{
	IntoDiagnostic,
	Report,
	Result,
	WrapErr,
	miette,
};
use poise::{
	FrameworkContext,
	serenity_prelude::{
		ChannelId,
		CreateMessage,
		Message,
	},
};
use sea_orm::DatabaseConnection;
use serde_json::{
	Value,
	json,
};
use tracing::trace;

use crate::{
	AppState,
	context_extraction::ContextMessageVariant,
	invocation_builder::InvocationBuilder,
	user_from_db_or_create,
};

#[derive(serde::Serialize)]
struct GuildContext {
	id: u64,
	name: String,
	members: Option<u64>,
}

impl From<&poise::serenity_prelude::PartialGuild> for GuildContext {
	fn from(guild: &poise::serenity_prelude::PartialGuild) -> Self {
		Self {
			id: guild.id.into(),
			name: guild.name.clone(),
			members: guild.approximate_member_count,
		}
	}
}

#[derive(serde::Serialize)]
struct ChannelContext {
	id: u64,
	name: String,
	topic: Option<String>,
}

impl From<&poise::serenity_prelude::GuildChannel> for ChannelContext {
	fn from(channel: &poise::serenity_prelude::GuildChannel) -> Self {
		Self {
			id: channel.id.into(),
			name: channel.name.clone(),
			topic: channel.topic.clone(),
		}
	}
}

#[derive(serde::Serialize)]
struct UserContext {
	id: u64,
	name: String,
}

impl From<&poise::serenity_prelude::User> for UserContext {
	fn from(user: &poise::serenity_prelude::User) -> Self {
		Self {
			id: user.id.into(),
			name: user.global_name.clone().unwrap_or(user.name.clone()),
		}
	}
}

pub async fn handle_completion(
	ctx: &poise::serenity_prelude::Context,
	framework: FrameworkContext<'_, AppState, Report>,
	app: &AppState,
	new_message: &Message,
) -> Result<()> {
	let db_user = user_from_db_or_create(&app.db, &new_message.author).await?;

	// if user opted out, we don't do anything, not even send a message, since that would be spammy
	if db_user.opt_out_since.is_some() {
		trace!("User opted out, not sending reply");
		return Ok(());
	}

	// check if channel is whitelisted
	if !app.whitelist.contains(new_message.channel_id, &ctx).await? {
		new_message
			.reply(ctx, "This channel is not whitelisted.")
			.await
			.into_diagnostic()
			.wrap_err("failed to send whitelist message")?;
		return Ok(());
	}

	// bot owner can always use the bot
	let is_owner = framework.options().owners.contains(&new_message.author.id);

	// note order, as this ensures we still hit database, even if user is owner
	if !check_rate_limit(new_message, app).await? && !is_owner {
		// prevent user from spamming us with timeout
		let error_report_future = tokio::time::timeout(std::time::Duration::from_secs(10), async {
			let rate_limited_message = new_message
				.reply(ctx, "I'm currently receiving too many requests, please try again later.")
				.await
				.into_diagnostic()
				.wrap_err("failed to send rate limit message")?;

			tokio::time::sleep(std::time::Duration::from_secs(5)).await;

			rate_limited_message
				.delete(ctx)
				.await
				.into_diagnostic()
				.wrap_err("failed to delete rate limit message")?;

			Ok(())
		})
		.await
		.into_diagnostic()
		.wrap_err("failed to send rate limit message");

		return error_report_future.unwrap_or_else(|_| {
			// timeout, don't care
			Ok(())
		});
	}

	let typing_notification = typing_indicator(ctx, new_message.channel_id);

	let completion_request = tokio::time::timeout(app.completion_timeout, generate_llm_response(ctx, app, new_message));

	// assuming typing notifications don't fail, we can just wait for the fork to finish and will keep sending typing
	// notifications in the meantime
	let result = tokio::select! {
		res = typing_notification => res,
		res = completion_request => {
			match res {
				Ok(res) => res,
				Err(_) => {
					return Err(miette!("completion request timed out"));
				},
			}
		},
	};

	result.wrap_err("failed to handle completion")?;

	Ok(())
}

async fn check_rate_limit(new_message: &Message, app: &AppState) -> Result<bool> {
	let mut context = HashMap::<&str, String>::new();
	context.insert("user_id", new_message.author.id.to_string());
	context.insert("channel_id", new_message.channel_id.to_string());
	if let Some(guild_id) = new_message.guild_id {
		context.insert("guild_id", guild_id.to_string());
	}

	let db = &app.db;
	let limit = app.path_rate_limits.lock().await;
	let pass = limit.check_route_with_context(&context, db).await?;

	Ok(pass)
}

async fn create_tera_context<'a>(ctx: &'a poise::serenity_prelude::Context, message: &'a Message) -> Result<tera::Context> {
	let mut tera_context = tera::Context::new();

	// no real way of handling timezones, since we don't know the timezone of the user
	let now_str = chrono::Local::now().format("%d.%m.%Y %H:%M:%S (%Z)").to_string();
	tera_context.insert("current_time", &now_str);

	match message.guild_id {
		Some(guild_id) => {
			// fill context with information about guild
			let guild = guild_id
				.to_partial_guild_with_counts(ctx)
				.await
				.into_diagnostic()
				.wrap_err("failed to fetch guild")?;

			// if we are in guild, we will use whatever name we have in the guild
			let id = ctx.cache.current_user().id;
			let self_member = guild
				.member(ctx, id)
				.await
				.into_diagnostic()
				.wrap_err("failed to fetch ourselves as guild member")?
				.nick
				.unwrap_or_else(|| ctx.cache.current_user().name.to_string());
			tera_context.insert("name", &self_member);

			let channel = message
				.channel_id
				.to_channel(ctx)
				.await
				.into_diagnostic()
				.wrap_err("failed to fetch channel")?;

			// we know channel must be a guild channel, since we are in a guild
			let channel = channel.guild().unwrap();

			tera_context.insert("guild", &GuildContext::from(&guild));
			tera_context.insert("channel", &ChannelContext::from(&channel));
		},
		None => {
			tera_context.insert("name", &ctx.cache.current_user().name);

			// could be a DM or group DM, but group DMs are not supported by discord, assume DM with single user
			tera_context.insert("dm", &UserContext::from(&message.author));
		},
	}

	Ok(tera_context)
}

/// Called in preparation of invoking LLM for response generation.
/// This function will load the configuration for the current execution context and fetch required messages from
/// Discord.
/// This includes possible resolution of reply chains and potential follow-up messages.
async fn generate_llm_response<'a>(
	ctx: &'a poise::serenity_prelude::Context,
	app: &'a AppState,
	message: &'a Message,
) -> Result<()> {
	let tera = &app.tera;
	let context_settings = &app.context_settings;
	let llm_client = &app.llm_client;
	let mcp_manager = &app.mcp_manager;

	// create a new MCP connection session for this LLM response generation
	let mcp_connection = mcp_manager.create_connection().await?;
	let tera_context = create_tera_context(ctx, message).await?;

	// remove empty lines, and truncate leading and trailing whitespace
	let preprompt = tera
		.render("preprompt.txt", &tera_context)
		.into_diagnostic()
		.wrap_err("failed to render preprompt")?
		.lines()
		.map(|l| l.trim())
		.filter(|l| !l.is_empty())
		.collect::<Vec<_>>()
		.join("\n");

	// TODO: implement message cache to avoid fetching messages multiple times
	// TODO: pass message cache as argument
	let mut chat_history = context_settings.extract_context_from_message(ctx, message).await?;

	if std::env::var("DUMP_EXTRACTED_HISTORY")
		.map(|v| v.to_lowercase())
		.map(|v| v == "true" || v == "1")
		.unwrap_or(false)
	{
		dump_extracted_messages(&chat_history);
	}

	// remove all messages for users that opted out
	remove_opted_out_users(&app.db, &mut chat_history).await?;

	// unpack chat history into messages, we longer need inclusion reason
	let chat_history = chat_history.iter().map(|m| m.into()).collect::<Vec<&Message>>();

	// add all messages to invocation builder, so it can remove markup and extract users and emotes
	let mut invocation_builder = InvocationBuilder::new(ctx.cache.current_user().id, "you");
	for message in chat_history {
		invocation_builder.add_message(message);
	}

	// Build the conversation using LLM crate's ChatMessage format
	let mut messages = Vec::new();

	// Add system message as the first user message with system context
	messages.push(ChatMessage::user().content(preprompt).build());

	// Convert invocation builder's messages to LLM format - this needs to be implemented
	// For now, let's manually convert the messages
	let llm_messages = invocation_builder.build_llm_messages();
	messages.extend(llm_messages);

	dump_llm_messages(&messages);

	// Tool calling loop - allow up to 10 iterations before forcing completion
	const MAX_TOOL_ITERATIONS: usize = 10;
	const MAX_TOOL_CALLS: usize = 30;
	let mut conversation = messages;

	// store all tool calls and results outside the loop for persistence across iterations
	let mut tool_calls: Vec<ToolCall> = Vec::new();
	let mut tool_results: Vec<ToolCall> = Vec::new();

	for iteration in 0..MAX_TOOL_ITERATIONS {
		// we limit the number of iterations
		let tools_available = if iteration == MAX_TOOL_ITERATIONS - 1 {
			None
		} else {
			llm_client.tools()
		};

		let response = llm_client
			.chat_with_tools(&conversation, tools_available)
			.await
			.into_diagnostic()
			.wrap_err("completion request failed")?;

		// Check if the model wants to use tools
		if let Some(new_calls) = response.tool_calls() {
			// Process tool calls and collect results
			for call in new_calls.into_iter() {
				// if we would be over the limit with this call, we stop processing further calls
				if tool_calls.len() >= MAX_TOOL_CALLS {
					debug!(
						"Reached maximum number of tool calls ({}), stopping further tool processing",
						MAX_TOOL_CALLS
					);
					break;
				}

				debug!("Processing tool call: {}", call.function.name);
				trace!("  - Arguments: {}", call.function.arguments);

				let result = process_tool_call(&call, &mcp_connection).await?;
				let pretty_json = serde_json::to_string_pretty(&result)
					.into_diagnostic()
					.wrap_err("failed to pretty-print tool result")?;
				trace!("  - Response: {}", pretty_json);

				// results are reported back with same tool call struct, yes
				tool_results.push(ToolCall {
					id: call.id.clone(),
					call_type: "function".to_string(),
					function: FunctionCall {
						name: call.function.name.clone(),
						arguments: serde_json::to_string(&result)
							.into_diagnostic()
							.wrap_err("failed to serialize tool result")?,
					},
				});

				// add last so we avoid a clone and can consume the call directly
				tool_calls.push(call);
			}

			// add assistant's tool call to conversation
			conversation.push(ChatMessage::assistant().tool_use(tool_calls.clone()).build());

			// add tool results to conversation
			conversation.push(ChatMessage::user().tool_result(tool_results.clone()).build());
		} else {
			// No tool calls - we have our final response
			let content = response.text().ok_or(miette!("LLM response has no content"))?;
			trace!(
				"Final response after {} iterations, {} tools called{}",
				iteration + 1,
				tool_calls.len(),
				if tool_calls.is_empty() {
					String::new()
				} else {
					format!(
						":\n{}",
						tool_calls
							.iter()
							.map(|call| format!("  - {} ({})", call.function.name, call.function.arguments))
							.collect::<Vec<_>>()
							.join("\n")
					)
				}
			);

			let content = invocation_builder.retransform_response(&content);

			message
				.channel_id
				.send_message(ctx, CreateMessage::new().reference_message(message).content(content))
				.await
				.into_diagnostic()
				.wrap_err("failed to send reply message")?;

			return Ok(());
		}
	}
	Ok(())
}

async fn remove_opted_out_users(db: &DatabaseConnection, messages: &mut Vec<ContextMessageVariant>) -> Result<()> {
	// extract all user ids from messages
	let authors = messages
		.iter()
		// convert into message and get	user id
		.map(|m| {
			let msg: &Message = m.into();
			&msg.author
		})
		.collect::<HashSet<_>>();

	// fetch database objects to check for opt-out status
	let mut opt_out_users = HashSet::new();
	for author in authors {
		let user = user_from_db_or_create(db, author).await?;

		if user.opt_out_since.is_some() {
			opt_out_users.insert(user.discord_user_id);
			continue;
		}
	}

	messages.retain(|m| {
		let msg: &Message = m.into();
		let retain = !opt_out_users.contains(&msg.author.id.get());

		if !retain {
			trace!("Removing message {} from user {} due to opt-out", msg.id, msg.author.name);
		}

		retain
	});

	Ok(())
}

/// Sends a typing indicator to a specified channel every 5 seconds, while running a separate task to handle messages
/// concurrently.
///
/// # Arguments
///
/// * `ctx` - A reference to the `Context` provided by Serenity.
/// * `channel_id` - The ID of the channel to send the typing indicator to.
///
/// # Returns
///
/// Returns a `Result` indicating whether the typing indicator was successfully sent or if an error occurred.
///
/// # Errors
///
/// This function will return an `Err` value if:
/// * There was an error sending the typing notification.
async fn typing_indicator(ctx: &poise::serenity_prelude::Context, channel_id: ChannelId) -> Result<()> {
	loop {
		channel_id
			.broadcast_typing(ctx)
			.await
			.into_diagnostic()
			.wrap_err("failed to send typing notification")?;
		tokio::time::sleep(std::time::Duration::from_secs(5)).await;
	}
}

fn dump_extracted_messages(messages: &[ContextMessageVariant]) {
	let mut lines = Vec::new();

	for message in messages {
		let reason = match message {
			ContextMessageVariant::Reply(_) => "Reply",
			ContextMessageVariant::Initial(_) => "Initial",
			ContextMessageVariant::ReplyWindow(_) => "ReplyWindow",
			ContextMessageVariant::History(_) => "History",
		};

		let message: &Message = message.into();
		lines.push(format!("{}({}): {}", reason, message.author.name, message.content));
	}

	trace!("Extracted the following messages from discord:\n{}", lines.join("\n"));
}

fn dump_llm_messages(messages: &[ChatMessage]) {
	let mut lines = Vec::new();

	for message in messages {
		let role_str = match message.role {
			ChatRole::User => "USER",
			ChatRole::Assistant => "ASSISTANT",
		};

		lines.push(format!("{}: {}", role_str, message.content));
	}
}

async fn process_tool_call(tool_call: &ToolCall, mcp_connection: &crate::mcp::McpConnection) -> Result<Value> {
	match mcp_connection.handle_llm_tool_call(tool_call).await {
		None => Ok(json!({
			"id": "tool_not_found",
			"error": format!("No tool found with name '{}'", tool_call.function.name)
		})),
		Some(result) => match result {
			Ok(value) => Ok(value),
			Err(err) => Ok(json!({
				"id": "tool_error",
				"error": format!("Tool execution failed: {}", err)
			})),
		},
	}
}
