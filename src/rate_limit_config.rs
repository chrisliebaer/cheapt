use std::{
	collections::HashMap,
	num::{
		NonZeroU32,
		NonZeroU64,
	},
	time::{
		Duration,
		Instant,
	},
};

use governor::{
	clock::MonotonicClock,
	middleware::NoOpMiddleware,
	Quota,
	RateLimiter,
};
use lazy_static::lazy_static;
use serde::{
	Deserialize,
	Serialize,
};
use tracing::trace;

use crate::rate_limiter::{
	HashMapStateStore,
	PathKey,
};

lazy_static! {
	static ref KEY_VARIABLE_REGEX: regex::Regex = regex::Regex::new(r"\{(?P<key>[a-zA-Z0-9_]+)\}").unwrap();
}

struct RateLimitConfig {
	limits: HashMap<String, Vec<RateLimitLine>>,
}

type SomethingRateLimiter = RateLimiter<PathKey, HashMapStateStore<PathKey>, MonotonicClock, NoOpMiddleware<Instant>>;
struct SomethingSomethingRateLimiter {
	/// Contains a list of routes and their template strings
	route_limits: Vec<(Vec<String>, String, Vec<SomethingRateLimiter>)>,
}

impl SomethingSomethingRateLimiter {
	fn test_key_check(&self, map: &HashMap<String, String>) {
		for (required_keys, format, rate_limiters) in &self.route_limits {
			// check if the map contains all the required keys, otherwise this limit doesn't apply
			if !required_keys.iter().all(|key| map.contains_key(key)) {
				continue;
			}

			// evaluate the template string to get concrete route
			let route = KEY_VARIABLE_REGEX
				.replace_all(&format, |caps: &regex::Captures| {
					let key = caps.name("key").unwrap().as_str();
					map.get(key).unwrap()
				})
				.to_string();

			trace!("hit route: {}", route);
		}
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
	requests: NonZeroU32,
}

impl Into<Quota> for &RateLimitLine {
	fn into(self) -> Quota {
		let requests = self.requests;
		let duration: Duration = (&self.slice).into();
		let duration = duration.div_f64(requests.get() as f64);

		Quota::with_period(duration).unwrap().allow_burst(requests)
	}
}

#[cfg(test)]
mod tests {
	use std::num::{
		NonZeroU32,
		NonZeroU64,
	};

	use super::*;

	fn bounds_helper(actual: Duration) -> (Duration, Duration) {
		let actual = actual.clone();
		let lower = actual - actual / 10;
		let upper = actual + actual / 10;
		(lower, upper)
	}

	fn test_builder(slice: Slice, requests: NonZeroU32) {
		let rate_limit_line = RateLimitLine {
			slice,
			requests,
		};
		let quota: Quota = (&rate_limit_line).into();

		let (lower, upper) = bounds_helper((&rate_limit_line.slice).into());
		assert!(quota.burst_size_replenished_in() > lower);
		assert!(quota.burst_size_replenished_in() < upper);
	}

	#[test]
	fn test_rate_limit_line_into_quota() {
		let numbers: Vec<u32> = vec![1, 5, 10, 20, 60, 51, 500];

		for i in &numbers {
			for j in &numbers {
				let nz = NonZeroU64::new((*i).into()).unwrap();
				let slices = vec![Slice::Seconds(nz), Slice::Minutes(nz), Slice::Hours(nz), Slice::Days(nz)];

				for slice in slices {
					let rate_limit_line = RateLimitLine {
						slice: slice.clone(),
						requests: NonZeroU32::new(*j).unwrap(),
					};
					let quota: Quota = (&rate_limit_line).into();

					let (lower, upper) = bounds_helper((&rate_limit_line.slice).into());
					assert!(quota.burst_size_replenished_in() > lower);
					assert!(quota.burst_size_replenished_in() < upper);
				}
			}
		}
	}
}
