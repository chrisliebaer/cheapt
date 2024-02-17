use entity::message_cache;
use log::debug;
use miette::{
	IntoDiagnostic,
	Result,
	WrapErr,
};
use poise::serenity_prelude::{
	ChannelId,
	Message,
	MessageId,
	UserId,
};
use sea_orm::{
	ActiveModelTrait,
	ActiveValue::Set,
	ColumnTrait,
	ConnectionTrait,
	EntityTrait,
	QueryFilter,
};

use crate::{
	user_from_db_or_create,
	Context,
};

/// Database backed message cache. Used to minimize the amount of requests to the Discord API. Once a message has been
/// fetched, it is stored in the cache for a certain amount of time. On message updates or deletions, the cache needs to
/// be invalidated.
pub struct MessageCache<'a, C> {
	db: &'a C,
}

impl<'a, C: ConnectionTrait> MessageCache<'a, C> {
	/// Creates a new handle to the message cache.
	pub fn new(db: &'a C) -> Self {
		Self {
			db,
		}
	}

	pub async fn _add(&self, message: &Message) -> Result<message_cache::Model> {
		// ensure author is also present in database
		let _ = user_from_db_or_create(self.db, &message.author).await?;

		let existing = {
			message_cache::Entity::find()
				.filter(message_cache::Column::DiscordMessageId.eq(message.id.get()))
				.one(self.db)
				.await
				.into_diagnostic()
				.wrap_err("failed to fetch message cache entry")?
		};

		if let Some(existing) = existing {
			debug!("message {} already in cache", message.id);
			return Ok(existing);
		}

		let entry = message_cache::ActiveModel {
			discord_message_id: Set(message.id.get()),
			discord_user_id: Set(message.author.id.get()),
			content: Set(message.content.clone()),
			..Default::default()
		};

		let model = entry
			.insert(self.db)
			.await
			.into_diagnostic()
			.wrap_err("failed to insert message cache entry")?;

		Ok(model)
	}

	pub async fn delete_from_user(&self, user_id: UserId) -> Result<()> {
		let entry = message_cache::ActiveModel {
			discord_user_id: Set(user_id.get()),
			..Default::default()
		};

		entity::prelude::MessageCache::delete(entry)
			.exec(self.db)
			.await
			.into_diagnostic()
			.wrap_err("failed to delete message cache entry")?;

		Ok(())
	}

	/// Fetches a message from the cache. If the message is not in the cache, it will be loaded from the Discord API.
	pub async fn _fetch(
		&self,
		channel_id: ChannelId,
		message_id: MessageId,
		ctx: &Context<'_>,
	) -> Result<Option<message_cache::Model>> {
		let entry = entity::prelude::MessageCache::find()
			.filter(message_cache::Column::DiscordMessageId.eq(message_id.get()))
			.one(self.db)
			.await
			.into_diagnostic()
			.wrap_err("failed to fetch message cache entry")?;

		// if not in cache, we fetch from discord and add to cache
		let entry = match entry {
			Some(entry) => entry,
			None => {
				debug!("message {} not in cache, fetching from discord", message_id);
				let discord_message = ctx
					.http()
					.get_message(channel_id, message_id)
					.await
					.into_diagnostic()
					.wrap_err("failed to fetch message from discord")?;
				self._add(&discord_message).await?
			},
		};

		Ok(Some(entry))
	}

	/// Invalidates a message in the cache. This is used when a message is updated or deleted.
	pub async fn invalidate(&self, message_id: &MessageId) -> Result<()> {
		entity::prelude::MessageCache::delete_many()
			.filter(message_cache::Column::DiscordMessageId.eq(message_id.get()))
			.exec(self.db)
			.await
			.into_diagnostic()
			.wrap_err("failed to delete message cache entry")?;

		Ok(())
	}
}
