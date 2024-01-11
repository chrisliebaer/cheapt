use std::collections::HashMap;

use async_openai::types::{
	ChatCompletionRequestMessage,
	ChatCompletionRequestSystemMessage,
	ChatCompletionRequestUserMessage,
	ChatCompletionRequestUserMessageContent,
	Role,
};
use lazy_static::lazy_static;
use poise::serenity_prelude::{
	EmojiId,
	Message,
	MessageId,
	UserId,
};
use regex::Regex;

// discord user mention regex
lazy_static! {
	static ref USER_MENTION_REGEX: Regex = Regex::new(r"<@!?(?P<id>\d+)>").unwrap();
	static ref EMOTE_MENTION_REGEX: Regex = Regex::new(r"<:(?P<name>\w+):(?P<id>\d+)>").unwrap();
}

/// This struct contains additional user provided context for the current context.
/// This allows server admins and users to provide additional information about their servers, the channel and the
/// expected output and quality of the response.
#[derive(Debug)]
pub struct InvocationContextLore {
	/// This lore describes what the user expects from the bot.
	/// This can be used to control the output length, the quality of the output as well as giving the bot a persona to
	/// use.
	pub personality: Option<String>,

	/// Lore about the current server. This provides additional information about the server which can not be immediately
	/// inferred from the messages themself. Will be `None` if no lore has been provided or invocation was outside of a
	/// server.
	pub guild_lore: Option<String>,

	/// Lore about the current channel. The bot will be given the channel topic and name as context.
	/// Many channels have poor names and topics, so this allows users to provide additional information about the
	/// channel. Will be `None` if no lore has been provided or invocation was outside of a server.
	pub channel_lore: Option<String>,

	/// Lore about the current user.
	pub user_lore: Option<String>,
}

/// This struct is responsible for processing and potentially filtering messages from a context.
/// It will remove inappropriate messages and transform Discord specific markup into a more generic format.
pub struct InvocationBuilder {
	/// The bots own user id.
	own_id: UserId,

	/// List of messages that will be included in the conversation. Implemented as a map to allow lookup of replies.
	input_messages: Vec<Message>,

	/// Mapping of user names to user ids. Used to convert user mentions in replies to user ids.
	user_cache: HashMap<String, UserId>,

	/// Mapping of emotes that reply can use.
	emote_cache: HashMap<String, EmojiId>,
}

impl InvocationBuilder {
	pub fn new(own_id: UserId, bot_name: &str) -> Self {
		let mut user_cache = HashMap::new();

		// add own id to user cache, since we are not always included in the conversation
		user_cache.insert(bot_name.to_string(), own_id);

		Self {
			own_id,
			input_messages: Vec::new(),
			user_cache,
			emote_cache: HashMap::new(),
		}
	}

	/// Adds a message to the conversation.
	/// This will extract emotes and users from the message and add them to the cache.
	/// Add all messages you want to include in the conversation before calling `build_request`.
	pub fn add_message(&mut self, message: &Message) {
		// add author to user cache
		self.user_cache.insert(message.author.name.clone(), message.author.id);

		// extract emotes from message
		for capture in EMOTE_MENTION_REGEX.captures_iter(&message.content) {
			if let (Some(name), Some(id)) = (capture.name("name"), capture.name("id")) {
				let emoji_id = EmojiId(id.as_str().parse().unwrap_or(0));
				self.emote_cache.insert(name.as_str().to_string(), emoji_id);
			}
		}

		// we can't transform the message directly, since other messages might provide additional emotes and users
		self.input_messages.push(message.to_owned());
	}

	/// Builds a request from the messages added to the builder.
	/// This will transform the messages into a format that can be used by the OpenAI API, as well as adding additional
	/// information about the context. Note that no preprompt is added, this is the responsibility of the caller.
	pub fn build_request(&self) -> Vec<ChatCompletionRequestMessage> {
		// bookkeeping, since we can't present raw markup to the model
		let mut request_messages = Vec::<ChatCompletionRequestMessage>::new();
		let mut message_lookup = HashMap::<MessageId, usize>::new();
		let mut message_counter = 1;

		// convert message to request message
		for message in self.input_messages.iter() {
			self.transform_message(message, &mut request_messages, &mut message_lookup, &mut message_counter);
		}

		request_messages
	}

	fn transform_message(
		&self,
		message: &Message,
		vec: &mut Vec<ChatCompletionRequestMessage>,
		message_lookup: &mut HashMap<MessageId, usize>,
		message_counter: &mut usize,
	) {
		// some messages are completely empty, since they only contain embeds or attachments, we skip those
		if message.content.is_empty() {
			return;
		}

		let has_attachments = !message.attachments.is_empty();
		let has_embeds = !message.embeds.is_empty();

		let mut facts = vec![format!("message no. {}", message_counter)];

		// if message is reply to other message, check if the message is in the lookup table and include reference
		if let Some(referenced_message) = &message.referenced_message {
			if let Some(ref_number) = message_lookup.get(&referenced_message.id) {
				facts.push(format!("reply to message no. {}", ref_number));
			};
		};

		// add information about things the model can't see
		if has_attachments || has_embeds {
			facts.push("contains removed attachments or embeds".to_string());
		}

		let header = ChatCompletionRequestSystemMessage {
			content: facts.join(", "),
			role: Role::System,
			..Default::default()
		};

		let is_own_message = message.author.id == self.own_id;
		let content = self.transform_markup(&message.content);

		// api names are restricted to a-zA-Z0-9_- and max 64 chars
		let mut author = message
			.author
			.name
			.replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "_");
		author.truncate(64);
		let author = author;

		let main = ChatCompletionRequestUserMessage {
			content: ChatCompletionRequestUserMessageContent::Text(content),
			name: match is_own_message {
				true => Some("Assistant".to_string()),
				false => Some(author),
			},
			role: match is_own_message {
				true => Role::Assistant,
				false => Role::User,
			},
		};

		vec.push(header.into());
		vec.push(main.into());

		// message successfully processed, keep track of it's position
		message_lookup.insert(message.id, *message_counter);
		*message_counter += 1;
	}

	/// Transforms markup in a message by replacing user mentions and emote mentions
	/// with corresponding formatted strings.
	///
	/// # Arguments
	///
	/// * `message` - The input message to transform.
	///
	/// # Returns
	///
	/// The transformed message with user mentions and emote mentions replaced.
	fn transform_markup(&self, message: &str) -> String {
		let result = message.to_string();

		// replace user mentions with @handle
		let result = USER_MENTION_REGEX.replace_all(&result, |caps: &regex::Captures| {
			let id_str = caps.name("id").unwrap().as_str();
			let id = id_str.parse::<u64>().ok().map(UserId::from);

			// try to find user name in cache, if not found, use id with @ prefix
			id.and_then(|id| {
				self
					.user_cache
					.iter()
					.find_map(|(name, user_id)| if *user_id == id { Some(name) } else { None })
			})
			.map(|name| format!("@{}", name))
			.unwrap_or(format!("@{}", id_str))
		});

		// replace emote mentions with :emote_name:
		let result = EMOTE_MENTION_REGEX.replace_all(&result, ":$name:");

		result.to_string()
	}
}
