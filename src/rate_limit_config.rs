use std::{
	collections::HashMap,
	future::Future,
	num::{
		NonZeroU32,
		NonZeroU64,
	},
	process::Output,
	time::{
		Duration,
		Instant,
		SystemTime,
	},
};

use async_trait::async_trait;
use chrono::{
	format::Numeric::Timestamp,
	DateTime,
	Utc,
};
use entity::{
	prelude::RateLimit,
	rate_limit,
};
use lazy_static::lazy_static;
use miette::{
	IntoDiagnostic,
	Report,
	Result,
	WrapErr,
};
use sea_orm::{
	ActiveModelTrait,
	ActiveValue::Set,
	ColumnTrait,
	DatabaseConnection,
	DbErr,
	EntityTrait,
	QueryFilter,
	TransactionTrait,
	TryFromU64,
};
use serde::{
	Deserialize,
	Serialize,
};
use tracing::trace;

use crate::gcre::GCRAConfig;

lazy_static! {
	static ref KEY_VARIABLE_REGEX: regex::Regex = regex::Regex::new(r"\{(?P<key>[a-zA-Z0-9_]+)\}").unwrap();
}

struct RateLimitConfig {
	limits: HashMap<String, Vec<RateLimitLine>>,
}

struct PathRateLimits {
	/// Contains a list of routes and their template strings
	route_limits: Vec<(Vec<String>, String, Vec<GCRAConfig>)>,
}

enum DbAction {
	Insert(rate_limit::ActiveModel),
	Update(rate_limit::ActiveModel),
}

impl PathRateLimits {
	async fn test_key_check(&self, map: &HashMap<String, String>, db: &DatabaseConnection) -> Result<()> {
		let now = Utc::now();

		for (required_keys, format, rate_limiters) in &self.route_limits {
			// check if the map contains all the required keys, otherwise this limit doesn't apply
			if !required_keys.iter().all(|key| map.contains_key(key)) {
				continue;
			}

			// evaluate the template string to get concrete path
			let path = KEY_VARIABLE_REGEX
				.replace_all(&format, |caps: &regex::Captures| {
					let key = caps.name("key").unwrap().as_str();
					map.get(key).unwrap()
				})
				.to_string();

			trace!("hit path: {}", path);

			// fetch the rate limit state for this path
			let states = RateLimit::find()
				.filter(rate_limit::Column::Path.eq(path.clone()))
				.all(db)
				.await
				.into_diagnostic()
				.wrap_err("failed to fetch rate limit state")?;

			// track new rate limit states and commit them at the end, if all checks pass
			let mut actions = Vec::new();

			// check all rate limiters on this route
			let mut allowed = true;
			for gcra in rate_limiters {
				// check if rate limit state exists
				let period = gcra.period.as_millis() as u64;
				let state = states.iter().find(|state| state.period == period);
				let tob = state.map(|state| DateTime::<Utc>::try_from_u64(state.state).unwrap());

				match gcra.check(now, tob, 1.try_into().unwrap()) {
					Some(tob) => {
						// pass
						let action = match state {
							Some(state) => {
								// update
								let mut state: rate_limit::ActiveModel = state.clone().into();
								state.state = Set(tob.timestamp_millis() as u64);
								DbAction::Update(state)
							},
							None => {
								// insert
								DbAction::Insert(rate_limit::ActiveModel {
									path: Set(path.clone()),
									period: Set(period),
									state: Set(tob.timestamp_millis() as u64),
								})
							},
						};
						actions.push(action);
					},
					None => {
						// rate limit exceeded
						allowed = false;
						break;
					},
				};
			}

			if !allowed {
				// rate limit exceeded, database won't be touched
				continue;
			}

			// commit all rate limit state changes in single transaction
			db.transaction::<_, (), DbErr>(|tx| {
				Box::pin(async move {
					for action in actions {
						match action {
							DbAction::Insert(state) => state.insert(tx).await?,
							DbAction::Update(state) => state.update(tx).await?,
						};
					}

					Ok(())
				})
			})
			.await
			.into_diagnostic()
			.wrap_err("failed to commit rate limit state changes")?;
		}

		Ok(())
	}
}

#[derive(Serialize, Deserialize, Debug, Clone)]
enum Slice {
	#[serde(rename = "seconds")]
	Seconds(NonZeroU64),
	#[serde(rename = "minutes")]
	Minutes(NonZeroU64),
	#[serde(rename = "hours")]
	Hours(NonZeroU64),
	#[serde(rename = "days")]
	Days(NonZeroU64),
}

impl Into<Duration> for &Slice {
	fn into(self) -> Duration {
		match self {
			Slice::Seconds(n) => Duration::from_secs(n.get()),
			Slice::Minutes(n) => Duration::from_secs(n.get() * 60),
			Slice::Hours(n) => Duration::from_secs(n.get() * 60 * 60),
			Slice::Days(n) => Duration::from_secs(n.get() * 60 * 60 * 24),
		}
	}
}

#[derive(Serialize, Deserialize)]
struct RateLimitLine {
	#[serde(flatten)]
	slice: Slice,
	quota: NonZeroU32,
	burst: Option<NonZeroU32>,
}

impl<B: AsRef<RateLimitLine>> From<B> for GCRAConfig {
	fn from(line: B) -> Self {
		let line = line.as_ref();
		Self::new((&line.slice).into(), line.quota, line.burst)
	}
}
