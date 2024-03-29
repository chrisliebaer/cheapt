//! `SeaORM` Entity. Generated by sea-orm-codegen 0.12.10

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "blacklist")]
pub struct Model {
	#[sea_orm(primary_key)]
	pub id: u64,
	#[sea_orm(unique)]
	pub discord_user_id: u64,
	#[sea_orm(column_type = "Text")]
	pub reason: String,
	pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
