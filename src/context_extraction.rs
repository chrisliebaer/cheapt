use miette::{
	IntoDiagnostic,
	Result,
	WrapErr,
};
use poise::serenity_prelude::{
	Context,
	GetMessages,
	Message,
};

/// This struct contains settings involved when building the context for an invocation.
/// Limiting what will be fetched from Discord and potentially included as context for the invocation.
#[derive(Debug)]
pub struct InvocationContextSettings {
	/// Maximum number of tokens to include in the context.
	/// This is an approximate limit, as we don't know the exact token count of the messages.
	/// But assuming a good enough estimate, we can prevent gobbling up too many tokens by fetching too many large
	/// messages.
	pub max_token_count: usize,

	/// Maximum number of messages that are simply fetched from the channel history.
	/// These provide potentially more context, but also increase token count.
	pub max_channel_history: Option<usize>,

	/// The maximum depth for fetching replied messages.
	pub reply_chain_depth: Option<usize>,

	/// Maximum number of messages to fetch from the same user after the replied message.
	/// This creates a window of messages around a message that was replied to and is used to provide more context in case
	/// a user typed multiple messages in a row.
	pub reply_chain_window: Option<usize>,

	/// Maximum number of tokens allowed to be included due to reply chain windows.
	/// Once this limit is reached, only directly replied messages will be included.
	pub reply_chain_max_token_count: Option<usize>,
}

impl InvocationContextSettings {
	pub async fn extract_context_from_message(&self, ctx: &Context, message: &Message) -> Result<Vec<ContextMessageVariant>> {
		// TODO: track which limits were exceeded
		let mut limit_tracker = LimitTracker::new();
		let mut messages = Vec::<ContextMessageVariant>::new();

		// the initial message is always added, regardless of limits, but still tracked
		let entry = ContextMessageVariant::Initial(message.to_owned());
		limit_tracker.add_message(&entry, self);
		messages.push(entry);

		// resolve reply chains if enabled
		if let Some(reply_chain_depth) = self.reply_chain_depth {
			// first we resolve the reply chain itself
			let chain_messages = {
				// track messages in chain to fetch window around message afterwords
				let mut chain_messages = Vec::new();
				let mut current_message = message.to_owned();

				for _ in 0..reply_chain_depth {
					if let Some(replied_message) = current_message.referenced_message.as_ref() {
						let entry = ContextMessageVariant::Reply(*replied_message.to_owned());
						if !limit_tracker.add_message(&entry, self) {
							break;
						}
						messages.push(entry);
						chain_messages.push(replied_message.to_owned());

						// refetch from discord, since discord won't give us next message in chain
						let replied_message = replied_message
							.channel_id
							.message(ctx, replied_message.id)
							.await
							.into_diagnostic()
							.wrap_err("failed to fetch replied message")?;
						current_message = replied_message;
					} else {
						break;
					}
				}
				chain_messages
			};

			// if enabled, we fetch a window of messages around the replied message, starting from the most recent
			if let Some(reply_chain_window) = self.reply_chain_window {
				for center in chain_messages {
					let window = message
						.channel_id
						.messages(ctx, GetMessages::new().around(center.id).limit(reply_chain_window as u8))
						.await
						.into_diagnostic()
						.wrap_err("failed to fetch reply chain window")?;

					// expand window around replied message by alternating between messages before and after the replied message
					let expanding_window = {
						let mut shrinking_window = Vec::<Message>::new();
						let mut window = window.into_iter();
						while let Some(message) = window.next() {
							// skip replied message, since we already have it
							if message.id == center.id {
								continue;
							}

							shrinking_window.push(message);
							if let Some(message) = window.next_back() {
								shrinking_window.push(message);
							}
						}
						// messages are now in collapsing order, so we reverse them to get the correct order
						shrinking_window.reverse();
						shrinking_window
					};

					// messages are now in expanding order, so we can add them to the context, aborting if we exceed limits
					for message in expanding_window.into_iter() {
						// abort if author is not the same as author of replied message
						if message.author.id != center.author.id {
							break;
						}

						let entry = ContextMessageVariant::ReplyWindow(message);
						if !limit_tracker.add_message(&entry, self) {
							break;
						}
						messages.push(entry);
					}
				}
			}
		}

		// fetch messages from channel history if enabled
		if let Some(max_channel_history) = self.max_channel_history {
			let history = message
				.channel_id
				.messages(ctx, GetMessages::new().before(message.id).limit(max_channel_history as u8))
				.await
				.into_diagnostic()
				.wrap_err("failed to fetch channel history")?;

			for message in history.into_iter() {
				let entry = ContextMessageVariant::History(message);
				if !limit_tracker.add_message(&entry, self) {
					break;
				}
				messages.push(entry);
			}
		}

		// order messages by increasing id and eliminate duplicates
		messages.sort_by_key(|m| {
			let message: &Message = m.into();
			message.timestamp
		});
		messages.dedup_by_key(|m| m.id());

		Ok(messages)
	}
}

/// Use during message selection to prevent exceeding specified limits.
#[derive(Clone)]
struct LimitTracker {
	/// Current number of total tokens.
	tokens: usize,

	/// Current number of tokens from the reply chain.
	reply_chain_tokens: usize,

	/// Current number of messages from channel history.
	history_count: usize,

	/// Current number of messages from the reply chain.
	reply_chain_count: usize,
}

impl LimitTracker {
	pub fn new() -> Self {
		Self {
			tokens: 0,
			reply_chain_tokens: 0,
			history_count: 0,
			reply_chain_count: 0,
		}
	}

	/// Add a message to the limit tracker.
	/// Returns `true` if the message was added, `false` if it was rejected due to exceeding limits.
	pub fn add_message(&mut self, message: &ContextMessageVariant, settings: &InvocationContextSettings) -> bool {
		// make a copy of the current state, so we can revert if the message is rejected
		let copy = self.clone();

		let message = match message {
			ContextMessageVariant::Initial(message) => {
				self.tokens += estimate_token_count(&message.content);
				message
			},
			ContextMessageVariant::History(message) => {
				self.history_count += 1;
				message
			},
			ContextMessageVariant::Reply(message) => {
				self.reply_chain_count += 1;
				self.reply_chain_tokens += estimate_token_count(&message.content);
				message
			},
			ContextMessageVariant::ReplyWindow(message) => {
				self.reply_chain_tokens += estimate_token_count(&message.content);
				message
			},
		};

		// all messages are added to the total token count
		self.tokens += estimate_token_count(&message.content);

		if self.is_within_limits(settings) {
			true
		} else {
			// revert to previous state
			*self = copy;
			false
		}
	}

	fn is_within_limits(&self, settings: &InvocationContextSettings) -> bool {
		// check if we are within the total token count
		if self.tokens > settings.max_token_count {
			return false;
		}

		// check if we are within the reply chain token count
		if let Some(reply_chain_max_token_count) = settings.reply_chain_max_token_count {
			if self.reply_chain_tokens > reply_chain_max_token_count {
				return false;
			}
		}

		// check if we are within the channel history count
		if let Some(max_channel_history) = settings.max_channel_history {
			if self.history_count > max_channel_history {
				return false;
			}
		}

		// check if we are within the reply chain count
		if let Some(reply_chain_depth) = settings.reply_chain_depth {
			if self.reply_chain_count > reply_chain_depth {
				return false;
			}
		}

		true
	}
}

/// This enum allows to differentiate between the different ways a message was included in the context.
pub enum ContextMessageVariant {
	/// The initial message that was used to start the invocation.
	Initial(Message),

	/// This message was included in the context because it was replied to.
	Reply(Message),

	/// This message was included in the context because it was from the same user as the replied message and was sent in
	/// a monologue.
	ReplyWindow(Message),

	/// This message was included in the context because it was fetched from the channel history.
	History(Message),
}

impl ContextMessageVariant {
	/// Returns the message id of the message.
	pub fn id(&self) -> u64 {
		match self {
			ContextMessageVariant::Initial(message) => message.id,
			ContextMessageVariant::History(message) => message.id,
			ContextMessageVariant::Reply(message) => message.id,
			ContextMessageVariant::ReplyWindow(message) => message.id,
		}
		.into()
	}
}

impl<'a> From<&'a ContextMessageVariant> for &'a Message {
	fn from(message: &'a ContextMessageVariant) -> Self {
		match message {
			ContextMessageVariant::Initial(message) => message,
			ContextMessageVariant::History(message) => message,
			ContextMessageVariant::Reply(message) => message,
			ContextMessageVariant::ReplyWindow(message) => message,
		}
	}
}

fn estimate_token_count(str: &str) -> usize {
	// TODO: use tiktoken-rs
	// for now we just count 6 characters as a token
	str.chars().count() / 6
}
