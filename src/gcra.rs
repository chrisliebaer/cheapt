use std::{
	cmp::{
		max,
		min,
	},
	num::NonZeroU32,
	time::Duration,
};

use chrono::{
	DateTime,
	Utc,
};
use tracing::instrument;

// TODO: allow burst to be zero
#[derive(Debug)]
pub struct GCRAConfig {
	/// Duration for which the rate limit is defined.
	pub period: Duration,

	/// Number of requests allowed in the period.
	pub quota: NonZeroU32,

	/// The maximum amount of quote that can accumulate.
	pub burst: u32,

	/// The interval between two emissions.
	emission_interval: Duration,

	/// The maximum amount of time a request can be delayed. Allows for burst.
	delay_tolerance: Duration,
}

impl GCRAConfig {
	pub fn new(period: Duration, quota: NonZeroU32, burst: Option<u32>) -> Self {
		// If burst is not defined, it’s assumed to be equal to quota
		let burst = burst.unwrap_or(quota.get() - 1);
		let emission_interval = period.div_f64(quota.get() as f64);
		let delay_tolerance = emission_interval.mul_f64(burst as f64);

		Self {
			period,
			quota,
			burst,
			emission_interval,
			delay_tolerance,
		}
	}

	/// Check if a request is allowed.
	///
	/// Returns the time at which the next request is allowed, or `None` if the request is not allowed.
	/// The caller is responsible for storing the returned time in a database or cache.
	///
	/// # Arguments
	/// * `now` - The current time.
	/// * `tob` - The time of burst, which is the time at which the entire burst is available.
	/// * `amount` - The amount of quota to consume.
	#[instrument]
	pub fn check(&self, now: DateTime<Utc>, tob: Option<DateTime<Utc>>, amount: NonZeroU32) -> Option<DateTime<Utc>> {
		// normally gcra implementations work with tat, the theoretical arrival time
		// this implementation is instead using the time of burst (tob), which describes the time at which the entire burst is
		// available. this allows the storage backend to discard all tob values which are in the past, without having to know
		// the configuration of the respective gcra. this greatly simplifies cleanup.

		assert!(amount <= self.quota, "amount must be less than or equal to quota");

		// increment is the number of emission intervals that are required to consume the amount of quota
		let increment = self.emission_interval.mul_f64(amount.get() as f64);

		// if no tob is given, we use `now`, which is equal to a fully replenished burst
		// otherwise we use pessimistic time, to prevent going over burst
		let tat = tob.map(|tob| max(tob - self.delay_tolerance, now)).unwrap_or(now);
		let allow_at = tat - self.delay_tolerance;

		// TODO: when allowing zero, this needs to be fixed, for initial call it would always be blocked and requires greater or
		// equal
		if now >= allow_at {
			// allow the request
			Some(tat + increment + self.delay_tolerance)
		} else {
			// block the request
			None
		}
	}

	/// Calculates the remaining quota based on the given parameters.
	///
	/// This function takes the current time `now` and an optional time of bucket last update `tob`,
	/// and calculates the remaining quota based on the configured quota, burst, and period values.
	///
	/// # Arguments
	///
	/// * `now` - The current time.
	/// * `tob` - An optional time of bucket last update.
	///
	/// # Returns
	///
	/// The remaining quota as an `u32` value.
	pub fn remaining(&self, now: DateTime<Utc>, tob: Option<DateTime<Utc>>) -> u32 {
		let tat = tob.map(|tob| max(tob - self.delay_tolerance, now)).unwrap_or(now);
		let allow_at = tat - self.emission_interval.mul_f64(self.burst as f64);

		match (now - allow_at).to_std() {
			Ok(delta) => {
				let remaining = delta.as_millis() as f64 / self.emission_interval.as_millis() as f64;
				// if remaining is negative, we return 0
				max(0, min(self.burst as i64, remaining as i64)) as u32 + 1
			},
			Err(_) => 0,
		}
	}
}

#[cfg(test)]
mod tests {
	use std::{
		num::NonZeroU32,
		time::Duration,
	};

	use super::*;

	struct TestWrapper(GCRAConfig, Option<DateTime<Utc>>);
	impl TestWrapper {
		fn new(config: GCRAConfig) -> Self {
			Self(config, None)
		}

		fn check(&mut self, now: DateTime<Utc>, amount: NonZeroU32) -> Option<DateTime<Utc>> {
			let result = self.0.check(now, self.1, amount);
			if result.is_some() {
				self.1 = result;
			}
			result
		}

		fn remaining(&self, now: DateTime<Utc>) -> u32 {
			self.0.remaining(now, self.1)
		}
	}

	fn new_test_gcra<F>(period: u32, quota: u32, burst: Option<u32>, f: F)
	where F: Fn(GCRAConfig) {
		let period = Duration::from_secs(period.into());
		let quota = NonZeroU32::new(quota).unwrap();
		let config = GCRAConfig::new(period, quota, burst);
		f(config);
	}

	#[test]
	fn test_setup() {
		// with burst
		new_test_gcra(60, 10, Some(5), |config| {
			assert_eq!(config.period, Duration::from_secs(60));
			assert_eq!(config.quota, NonZeroU32::new(10).unwrap());
			assert_eq!(config.burst, 5);
		});

		// without burst
		new_test_gcra(60, 10, None, |config| {
			assert_eq!(config.period, Duration::from_secs(60));
			assert_eq!(config.quota, NonZeroU32::new(10).unwrap());
			assert_eq!(config.burst, 9);
		});
	}

	#[test]
	fn simple_single_allow() {
		new_test_gcra(60, 10, None, |config| {
			let mut wrapper = TestWrapper::new(config);
			let now = Utc::now();
			let amount = NonZeroU32::new(1).unwrap();

			assert_eq!(wrapper.remaining(now), 10);

			// deplete all quota
			for _ in 0..10 {
				assert!(wrapper.check(now, amount).is_some());
			}

			assert_eq!(wrapper.remaining(now), 0);
		});
	}

	#[test]
	fn simple_single_last_fail() {
		new_test_gcra(60, 10, None, |config| {
			let mut wrapper = TestWrapper::new(config);
			let now = Utc::now();
			let amount = NonZeroU32::new(1).unwrap();

			// deplete all quota
			for _ in 0..10 {
				assert!(wrapper.check(now, amount).is_some());
			}

			// last request should fail
			assert!(wrapper.check(now, amount).is_none());
		});
	}

	#[test]
	fn refill_after_empty() {
		new_test_gcra(10, 10, None, |config| {
			let mut wrapper = TestWrapper::new(config);
			let now = Utc::now();
			let then = now + Duration::from_secs(1);
			let amount = NonZeroU32::new(1).unwrap();

			// deplete all quota
			for _ in 0..10 {
				assert!(wrapper.check(now, amount).is_some());
			}

			// confirm empty
			assert!(wrapper.check(now, amount).is_none());
			assert_eq!(wrapper.remaining(now), 0);

			// check for one (and only one) refill
			assert_eq!(wrapper.remaining(then), 1);
			assert!(wrapper.check(then, amount).is_some());
			assert_eq!(wrapper.remaining(then), 0);
			assert!(wrapper.check(then, amount).is_none());
		});
	}

	#[test]
	fn complex_interaction_test() {
		new_test_gcra(60, 10, Some(5), |config| {
			let mut wrapper = TestWrapper::new(config);
			let now = Utc::now();
			let middle = now + Duration::from_secs(30); // 30 seconds later
			let end = now + Duration::from_secs(120); // 120 seconds later
			let normal_amount = NonZeroU32::new(1).unwrap();

			// Check that burst works correctly (first has one + burst)
			for _ in 0..6 {
				assert!(wrapper.check(now, normal_amount).is_some());
			}
			// No quota should be left
			assert_eq!(wrapper.remaining(now), 0);

			// After 30 seconds, only 5 requests should be allowed as period hasn't completed yet
			assert_eq!(wrapper.remaining(middle), 5);
			for _ in 0..5 {
				assert!(wrapper.check(middle, normal_amount).is_some());
			}
			// No quota should be left
			assert_eq!(wrapper.remaining(middle), 0);
			assert!(wrapper.check(middle, normal_amount).is_none());

			// After 120 seconds, only 6 requests should be allowed, since burst is 5
			assert_eq!(wrapper.remaining(end), 6);
			for _ in 0..6 {
				assert!(wrapper.check(end, normal_amount).is_some());
			}

			// No quota should be left
			assert_eq!(wrapper.remaining(end), 0);
			assert!(wrapper.check(end, normal_amount).is_none());
		});
	}

	#[test]
	fn large_amount_consume_and_small_refill() {
		new_test_gcra(120, 10, None, |config| {
			let mut wrapper = TestWrapper::new(config);
			let now = Utc::now();
			let a_min_later = now + Duration::from_secs(60); // 60 seconds later
			let amount_large = NonZeroU32::new(7).unwrap();
			let amount_small = NonZeroU32::new(1).unwrap();

			// Deplete 7 quota at once
			assert!(wrapper.check(now, amount_large).is_some());

			// Only 3 Quota should be left
			assert_eq!(wrapper.remaining(now), 3);

			// After 60 seconds, the quota should be increased by only 5 because this is just half of the period
			assert_eq!(wrapper.remaining(a_min_later), 8);
			for _ in 0..8 {
				assert!(wrapper.check(a_min_later, amount_small).is_some());
			}

			// Now quota should be depleted
			assert_eq!(wrapper.remaining(a_min_later), 0);
			assert!(wrapper.check(a_min_later, amount_small).is_none());
		});
	}

	#[test]
	fn partial_refill() {
		new_test_gcra(10, 10, None, |config| {
			let mut wrapper = TestWrapper::new(config);
			let now = Utc::now();
			let then = now + Duration::from_secs(10);
			let amount = NonZeroU32::new(1).unwrap();

			// deplete 3 quota
			for _ in 0..3 {
				assert!(wrapper.check(now, amount).is_some());
			}
			assert_eq!(wrapper.remaining(now), 7);

			// confirm refill of all quota
			assert_eq!(wrapper.remaining(then), 10);
			for _ in 0..10 {
				assert!(wrapper.check(then, amount).is_some());
			}

			// but no more
			assert_eq!(wrapper.remaining(then), 0);
			assert!(wrapper.check(then, amount).is_none());
		});
	}

	#[test]
	fn exact_quota() {
		new_test_gcra(60, 10, None, |config| {
			let mut wrapper = TestWrapper::new(config);
			let now = Utc::now();
			let amount = NonZeroU32::new(10).unwrap();

			assert_eq!(wrapper.remaining(now), 10);
			assert!(wrapper.check(now, amount).is_some());

			assert_eq!(wrapper.remaining(now), 0);
			assert!(wrapper.check(now, amount).is_none());
		});
	}

	#[test]
	fn check_request_allowed() {
		let period = Duration::from_secs(60);
		let quota = NonZeroU32::new(10).unwrap();
		let config = GCRAConfig::new(period, quota, None);
		let now = Utc::now();
		let amount = NonZeroU32::new(1).unwrap();

		assert!(config.check(now, None, amount).is_some());
	}

	#[test]
	fn check_request_blocked() {
		let period = Duration::from_secs(60);
		let quota = NonZeroU32::new(10).unwrap();
		let config = GCRAConfig::new(period, quota, None);
		let now = Utc::now();

		assert!(config.check(now, None, NonZeroU32::new(9).unwrap()).is_some());
	}

	#[test]
	#[should_panic(expected = "amount must be less than or equal to quota")]
	fn check_amount_greater_than_quota() {
		let period = Duration::from_secs(60);
		let quota = NonZeroU32::new(10).unwrap();
		let config = GCRAConfig::new(period, quota, None);
		let now = Utc::now();
		let amount = NonZeroU32::new(11).unwrap();

		config.check(now, None, amount);
	}

	#[test]
	fn invalid_remaining_quota_with_no_requests() {
		new_test_gcra(60, 10, None, |config| {
			let wrapper = TestWrapper::new(config);
			let now = Utc::now();

			// The remaining quota should be equal to total quota initially
			assert_eq!(wrapper.remaining(now), 10);

			// After half period, the remaining quota should still be same as no request has been made
			let half_period_later = now + Duration::from_secs(30);
			assert_eq!(wrapper.remaining(half_period_later), 10);
		});
	}

	#[test]
	fn invalid_remaining_quota_after_exhaustion() {
		new_test_gcra(60, 1, None, |config| {
			let mut wrapper = TestWrapper::new(config);
			let now = Utc::now();

			// The remaining quota should be equal to total quota initially
			assert_eq!(wrapper.remaining(now), 1);

			// Making a request should exhaust the quota
			assert!(wrapper.check(now, NonZeroU32::new(1).unwrap()).is_some());

			// The remaining quota should be 0 now
			assert_eq!(wrapper.remaining(now), 0);

			// Even after half period, the remaining quota should still be 0 as full period has not ended yet
			let half_period_later = now + Duration::from_secs(30);
			assert_eq!(wrapper.remaining(half_period_later), 0);
		});
	}

	#[test]
	fn slow_limit_fast_requests() {
		new_test_gcra(86400000, 10, None, |config| {
			let mut wrapper = TestWrapper::new(config);

			let amount = NonZeroU32::new(1).unwrap();

			let mut now = Utc::now();
			for i in 0..10 {
				assert_eq!(wrapper.remaining(now), 10 - i);
				assert!(wrapper.check(now, amount).is_some());

				now += Duration::from_millis(200);
			}

			assert_eq!(wrapper.remaining(now), 0);
			assert!(wrapper.check(now, amount).is_none());
			assert_eq!(wrapper.remaining(now), 0);
		});
	}
}
