use std::{
	borrow::Borrow,
	collections::HashMap,
	num::{
		NonZeroU32,
		NonZeroU64,
	},
	time::Duration,
};

use chrono::{
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
};
use serde::{
	Deserialize,
	Serialize,
};
use tracing::trace;

use crate::gcra::GCRAConfig;

lazy_static! {
	static ref KEY_VARIABLE_REGEX: regex::Regex = regex::Regex::new(r"\{(?P<key>[a-zA-Z0-9_]+)\}").unwrap();
}

#[derive(Serialize, Deserialize, Debug)]
pub struct RateLimitConfig {
	limits: HashMap<String, Vec<RateLimitLine>>,
}

impl RateLimitConfig {
	pub fn from_file(path: &str) -> Result<Self> {
		let config: Self = toml::from_str(
			&std::fs::read_to_string(path)
				.into_diagnostic()
				.wrap_err("failed to read file")?,
		)
		.into_diagnostic()
		.wrap_err("failed to parse rate limit config")?;

		Ok(config)
	}
}

impl<T: Borrow<RateLimitConfig>> From<T> for PathRateLimits {
	fn from(value: T) -> Self {
		let config = value.borrow();
		let mut routes: Vec<Route> = Vec::new();

		for (path, lines) in &config.limits {
			let mut gcras: Vec<GCRAConfig> = Vec::new();
			for line in lines {
				gcras.push(line.into());
			}

			// use regex to extract keys from path
			let keys: Vec<String> = KEY_VARIABLE_REGEX
				.captures_iter(&path)
				.map(|caps| caps.name("key").unwrap().as_str().to_string())
				.collect();

			let entry = (keys, path.to_string(), gcras);
			routes.push(entry);
		}

		PathRateLimits {
			route_limits: routes,
		}
	}
}

type Route = (Vec<String>, String, Vec<GCRAConfig>);
pub struct PathRateLimits {
	/// Contains a list of routes and their template strings
	route_limits: Vec<Route>,
}

#[derive(Debug)]
enum DbAction {
	Insert(rate_limit::ActiveModel),
	Update(rate_limit::ActiveModel),
}

impl PathRateLimits {
	pub async fn check_route_with_context(&self, map: &HashMap<String, String>, db: &DatabaseConnection) -> Result<bool> {
		let now = Utc::now();

		// track new rate limit states and commit them at the end, if all checks pass
		let mut actions = Vec::new();

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

			// check all rate limiters on this route
			let mut allowed = true;
			for gcra in rate_limiters {
				// check if rate limit state exists
				let period = gcra.period.as_millis() as u64;
				let state = states.iter().find(|state| state.period == period);
				let tob = state
					.map(|state| state.state)
					.map(|milis| DateTime::<Utc>::from_timestamp((milis / 1000) as i64, ((milis % 1000) * 1_000_000) as u32).unwrap());

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
				return Ok(false);
			}
		}

		// if we reach this point, all rate limits passed, so we can commit the changes
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

		Ok(true)
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

#[derive(Serialize, Deserialize, Debug)]
struct RateLimitLine {
	#[serde(flatten)]
	slice: Slice,
	quota: NonZeroU32,
	burst: Option<NonZeroU32>,
}

impl<T: Borrow<RateLimitLine>> From<T> for GCRAConfig {
	fn from(value: T) -> Self {
		let line = value.borrow();
		Self::new((&line.slice).into(), line.quota, line.burst)
	}
}

#[cfg(test)]
mod tests {
	use std::collections::{
		HashMap,
		HashSet,
	};

	use sea_orm::{
		DatabaseBackend,
		MockDatabase,
		MockExecResult,
	};

	use super::*;

	fn dummy_config() -> RateLimitConfig {
		let str = r#"
			[limits]
			"global" = [
					{ seconds = 1, quota = 10 },
					{ minutes = 10, quota = 100 },
					{ days = 1, quota = 500 },
			]

			"guild/{guild_id}" = [
					{ seconds = 1, quota = 3 },
					{ minutes = 1, quota = 10 },
					{ hours = 1, quota = 30 },
			]

			"channel/{channel_id}" = [
					{ seconds = 5, quota = 1 },
			]

			"user/{user_id}" = [
					{ seconds = 15, quota = 2 },
					{ minutes = 1, quota = 10 },
					{ hours = 6, quota = 60 },
			]

			"guild/{guild_id}/channel/{channel_id}" = [
					{ seconds = 1, quota = 1 },
			]
		"#;
		let config = toml::from_str::<RateLimitConfig>(str).unwrap();
		config
	}

	fn get_route_by_name<'a>(name: &str, config: &'a PathRateLimits) -> &'a Route {
		let routes = config
			.route_limits
			.iter()
			.filter(|(_, route, ..)| route == &name.to_string())
			.collect::<Vec<_>>();

		assert_eq!(routes.len(), 1);

		&routes[0]
	}

	fn db_backed_rate_limiter() -> (MockDatabase, PathRateLimits) {
		let config = dummy_config();
		let limits: PathRateLimits = (&config).into();

		let db = MockDatabase::new(DatabaseBackend::MySql);

		(db, limits)
	}

	fn verify_keys(route: &Route, keys: &[&str]) {
		let (required_keys, ..) = route;

		// check if all keys are present by using a set
		let mut set = HashSet::new();
		for key in required_keys {
			set.insert(key);
		}

		for key in keys {
			let key = key.to_string();
			assert!(set.contains(&key));
		}
	}

	#[test]
	fn test_rate_limit_config() {
		let config = dummy_config();
		let limits: PathRateLimits = (&config).into();

		let global = get_route_by_name("global", &limits);
		let guild = get_route_by_name("guild/{guild_id}", &limits);
		let channel = get_route_by_name("channel/{channel_id}", &limits);
		let user = get_route_by_name("user/{user_id}", &limits);
		let combined = get_route_by_name("guild/{guild_id}/channel/{channel_id}", &limits);

		// check all keys are present and no extra keys are present
		verify_keys(global, &[]);
		verify_keys(guild, vec!["guild_id"].as_slice());
		verify_keys(channel, vec!["channel_id"].as_slice());
		verify_keys(user, vec!["user_id"].as_slice());
		verify_keys(combined, vec!["guild_id", "channel_id"].as_slice());
	}

	#[test]
	fn test_enum_variants() {
		let config = dummy_config();
		let global = config.limits.get("global").unwrap();

		let r1 = &global[0];
		let r2 = &global[1];
		let r3 = &global[2];

		assert!(matches!(r1.slice, Slice::Seconds(_)));
		assert!(matches!(r2.slice, Slice::Minutes(_)));
		assert!(matches!(r3.slice, Slice::Days(_)));
	}

	#[tokio::test]
	async fn test_db_write_success() {
		let (mock_db, path_rate_limits) = db_backed_rate_limiter();

		// this model is retuned to the update code, but is not used
		let fake_model = rate_limit::Model {
			path: "fakepath".to_string(),
			period: 0,
			state: 0,
		};

		let fake_update = MockExecResult {
			last_insert_id: 0,
			rows_affected: 1,
		};

		let db = mock_db
			// initial lookup for rate limit state
			.append_query_results([vec![rate_limit::Model {
				path: "global".to_string(),
				period: 1000,
				state: 0,
			}]])
			// update rate limit state
			.append_exec_results([
				fake_update.clone(),
				fake_update.clone(),
				fake_update.clone(),
				fake_update.clone(),
			])
			// fetch updated rate limit state
			.append_query_results([
				vec![fake_model.clone()],
				vec![fake_model.clone()],
				vec![fake_model.clone()],
				vec![fake_model.clone()],
			])
			.into_connection();

		assert!(path_rate_limits.check_route_with_context(&HashMap::new(), &db).await.is_ok());

		let log = db.into_transaction_log();
		// we expect 2 queries, since select and update are combined into one respective query due to the transaction
		assert_eq!(log.len(), 2);
	}

	#[tokio::test]
	async fn test_db_denied_no_db_write() {
		let (mock_db, path_rate_limits) = db_backed_rate_limiter();

		let far_future = Utc::now() + chrono::Duration::days(100);

		// this period will cause the rate limit to be exceeded
		let exceed_model = rate_limit::Model {
			path: "global".to_string(),
			period: 1000,
			state: far_future.timestamp_millis() as u64,
		};

		let allowed_model = rate_limit::Model {
			path: "global".to_string(),
			period: 600000,
			state: 0,
		};

		let db = mock_db
			// return one period that is exceeded and one that is allowed, and leave the rest empty
			.append_query_results([vec![exceed_model, allowed_model]])
			.into_connection();

		assert_eq!(
			path_rate_limits.check_route_with_context(&HashMap::new(), &db).await.unwrap(),
			false
		);

		let log = db.into_transaction_log();
		// we expect a single query, and no update query since the rate limit was exceeded
		assert_eq!(log.len(), 1);
	}
}
