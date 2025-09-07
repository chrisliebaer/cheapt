use std::collections::HashMap;

use lazy_static::lazy_static;
use llm::chat::ChatMessage;
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
	static ref USER_HANDLE_REGEX: Regex = Regex::new(r"@(?P<handle>\w+)").unwrap();
	static ref EMOTE_NAME_REGEX: Regex = Regex::new(r":(?P<name>\w+):").unwrap();
}

/// This struct contains additional user provided context for the current context.
/// This allows server admins and users to provide additional information about their servers, the channel and the
/// expected output and quality of the response.
#[allow(dead_code)]
#[derive(Debug)]
pub struct InvocationContextLore {
	/// Lore about the current server. This provides additional information about the server which can not be immediately
	/// inferred from the messages themselves. Will be `None` if no lore has been provided or invocation was outside a
	/// server.
	pub guild_lore: Option<String>,

	/// Lore about the current channel. The bot will be given the channel topic and name as context.
	/// Many channels have poor names and topics, so this allows users to provide additional information about the
	/// channel. Will be `None` if no lore has been provided or invocation was outside a server.
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

	/// Mapping of usernames to user ids. Used to convert user mentions in replies to user ids.
	user_cache: HashMap<String, UserId>,

	/// Mapping of emotes that reply can use.
	emote_cache: HashMap<String, EmojiId>,
}

// TODO: implement database lookup for emoji and user ids
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
	/// Add all messages you want to include in the conversation before calling `build_llm_messages`.
	pub fn add_message(&mut self, message: &Message) {
		// add author to user cache
		self.user_cache.insert(message.author.name.clone(), message.author.id);

		// extract emotes from message
		for capture in EMOTE_MENTION_REGEX.captures_iter(&message.content) {
			let name = capture.name("name").unwrap().as_str().to_string();
			let id = capture.name("id").unwrap().as_str().parse().unwrap();

			self.emote_cache.insert(name, EmojiId::new(id));
		}

		// we can't transform the message directly, since other messages might provide additional emotes and users
		self.input_messages.push(message.to_owned());
	}

	/// Builds LLM ChatMessage objects from the messages added to the builder.
	/// This will transform the messages into a format that can be used by the LLM crate,
	/// preserving all metadata including author info, message numbers, and reply relationships.
	pub fn build_llm_messages(&self) -> Vec<ChatMessage> {
		let mut llm_messages = Vec::new();
		let mut message_lookup = HashMap::<MessageId, usize>::new();
		let mut message_counter = 1;

		for message in self.input_messages.iter() {
			// some messages are completely empty, since they only contain embeds or attachments, we skip those
			if message.content.is_empty() {
				continue;
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

			let is_own_message = message.author.id == self.own_id;
			let content = self.transform_markup(&message.content);

			// api names are restricted to a-zA-Z0-9_- and max 64 chars
			let mut author = message
				.author
				.name
				.replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "_");
			author.truncate(64);

			// For LLM crate, we need to encode metadata in the content
			// First add a system-style message with facts, then the actual message
			let header_content = facts.join(", ");
			let header_message = ChatMessage::user().content(format!("[SYSTEM: {}]", header_content)).build();
			llm_messages.push(header_message);

			// Then add the main message with proper role and author info
			let main_content = if is_own_message {
				content
			} else {
				format!("{}: {}", author, content)
			};

			let main_message = if is_own_message {
				ChatMessage::assistant().content(main_content).build()
			} else {
				ChatMessage::user().content(main_content).build()
			};

			llm_messages.push(main_message);

			// message successfully processed, keep track of it's position
			message_lookup.insert(message.id, message_counter);
			message_counter += 1;
		}

		llm_messages
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

			// try to find username in cache, if not found, use id with @ prefix
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

	/// Transforms a response from the LLM into a Discord message.
	/// This will replace @handle with user mentions and :emote_name: with emote mentions.
	pub fn retransform_response(&self, message: &str) -> String {
		let result = message.to_string();

		let result = USER_HANDLE_REGEX.replace_all(&result, |caps: &regex::Captures| {
			let handle = caps.name("handle").unwrap().as_str();

			// try to find user id in cache, if not found, use handle with @ prefix
			self
				.user_cache
				.get(handle)
				.map(|id| format!("<@{}>", id))
				.unwrap_or(format!("@{}", handle))
		});

		let result = EMOTE_NAME_REGEX.replace_all(&result, |caps: &regex::Captures| {
			let name = caps.name("name").unwrap().as_str();

			// try to find emote id in cache, if not found, use :name:
			self
				.emote_cache
				.get(name)
				.map(|id| format!("<:{}:{}>", name, id))
				.unwrap_or(format!(":{}:", name))
		});

		result.to_string()
	}
}
