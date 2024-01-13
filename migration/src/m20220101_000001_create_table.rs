use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		// user table
		manager
			.create_table(
				Table::create()
					.table(User::Table)
					.col(
						ColumnDef::new(User::Id)
							.big_unsigned()
							.not_null()
							.auto_increment()
							.primary_key(),
					)
					.col(ColumnDef::new(User::Uuid).uuid().not_null().unique_key())
					.col(ColumnDef::new(User::DiscordUserId).big_unsigned().not_null().unique_key())
					.col(ColumnDef::new(User::Username).text().not_null())
					.to_owned(),
			)
			.await?;

		// message cache table
		manager
			.create_table(
				Table::create()
					.table(MessageCache::Table)
					.col(
						ColumnDef::new(MessageCache::Id)
							.big_unsigned()
							.not_null()
							.auto_increment()
							.primary_key(),
					)
					.col(
						ColumnDef::new(MessageCache::DiscordMessageId)
							.big_unsigned()
							.not_null()
							.unique_key(),
					)
					.col(ColumnDef::new(MessageCache::RefDiscordMessageId).big_unsigned().null())
					.col(
						ColumnDef::new(MessageCache::DiscordUserId)
							.big_unsigned()
							.not_null()
							.unique_key(),
					)
					.col(ColumnDef::new(MessageCache::Content).string().not_null())
					.to_owned(),
			)
			.await?;
		manager
			.create_foreign_key(
				ForeignKey::create()
					.name("fk_message_cache_ref_discord_user_id")
					.from(MessageCache::Table, MessageCache::DiscordUserId)
					.to(User::Table, User::DiscordUserId)
					.on_update(ForeignKeyAction::Cascade)
					.on_delete(ForeignKeyAction::Cascade)
					.to_owned(),
			)
			.await?;
		manager
			.create_foreign_key(
				ForeignKey::create()
					.name("fk_message_cache_ref_discord_message_id")
					.from(MessageCache::Table, MessageCache::RefDiscordMessageId)
					.to(MessageCache::Table, MessageCache::DiscordMessageId)
					.on_update(ForeignKeyAction::Cascade)
					.on_delete(ForeignKeyAction::Cascade)
					.to_owned(),
			)
			.await?;

		// rate limit table
		manager
			.create_table(
				Table::create()
					.table(RateLimit::Table)
					.col(ColumnDef::new(RateLimit::Path).string().not_null().primary_key())
					.col(ColumnDef::new(RateLimit::State).big_unsigned().not_null())
					.to_owned(),
			)
			.await?;

		// blacklist table
		manager
			.create_table(
				Table::create()
					.table(Blacklist::Table)
					.col(
						ColumnDef::new(Blacklist::Id)
							.big_unsigned()
							.not_null()
							.auto_increment()
							.primary_key(),
					)
					.col(
						ColumnDef::new(Blacklist::DiscordUserId)
							.big_unsigned()
							.not_null()
							.unique_key(),
					)
					.col(ColumnDef::new(Blacklist::Reason).text().not_null())
					.col(ColumnDef::new(Blacklist::CreatedAt).timestamp().not_null())
					.to_owned(),
			)
			.await?;

		Ok(())
	}

	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager.drop_table(Table::drop().table(User::Table).to_owned()).await?;
		manager
			.drop_table(Table::drop().table(MessageCache::Table).to_owned())
			.await?;
		manager.drop_table(Table::drop().table(RateLimit::Table).to_owned()).await?;
		manager.drop_table(Table::drop().table(Blacklist::Table).to_owned()).await?;

		Ok(())
	}
}

/// User table.
///
/// Associates a Discord user with a UUID. This allows us to use internal UUIDs when calling the OpenAI API for abuse
/// prevention.
#[derive(DeriveIden)]
enum User {
	Table,

	/// Database ID for primary key.
	Id,

	/// Internal UUID to use when calling the OpenAI API.
	Uuid,

	/// Discord ID of the user.
	DiscordUserId,

	/// Discord Username, not guaranteed to be unique, since users can change their name.
	Username,
}

/// Message cache.
///
/// This table is used to cache messages from Discord, so we don't have to fetch them from Discord every time assemble a
/// context. Message cache is invalidated when a message is edited or deleted and periodically pruned.
#[derive(DeriveIden)]
enum MessageCache {
	Table,

	/// Discord ID for primary key.
	Id,

	/// Discord ID of the message.
	DiscordMessageId,

	/// Optional reference to the message this message is a reply to.
	RefDiscordMessageId,

	/// Reference to the user that sent the message.
	DiscordUserId,

	/// The message content.
	Content,
}

/// Rate limit table.
///
/// This table is used to track rate limits for the application. This table represents the entire state of the rate
/// limiter and is loaded into memory on startup.
#[derive(DeriveIden)]
enum RateLimit {
	Table,

	/// Path describing the rate limit.
	Path,

	/// The start of the rate limit.
	State,
}

/// Blacklist table.
///
/// This table is used to blacklist users from using the bot. This overrides any opt-in or opt-out settings and makes
/// the bot completely ignore the user.
#[derive(DeriveIden)]
enum Blacklist {
	Table,

	/// Discord ID for primary key.
	Id,

	/// Discord ID of the user.
	DiscordUserId,

	/// Reason for the blacklist.
	Reason,

	/// Timestamp when the blacklist was created.
	CreatedAt,
}
