use std::{
	collections::HashMap,
	fmt,
	fmt::Debug,
	hash::Hash,
	num::NonZeroU64,
	sync::{
		atomic::{
			AtomicU64,
			Ordering,
		},
		Mutex,
	},
	time::Duration,
};

use entity::rate_limit;
use governor::{
	nanos::Nanos,
	state::{
		keyed::ShrinkableKeyedStateStore,
		NotKeyed,
		StateStore,
	},
};
use miette::Result;
use sea_orm::ActiveValue::Set;

// TODO: would be great to have this in the governor crate, but first it seems abandoned and second it needs a marker in
// order to work properly, since relative clocks can't be reinstantiated with states from a difference instance

// This file contains an almost verbatim copy of the governor crate's HashMapStateStore, with a different internal state
// that allows for serialization and deserialization. Something which is not possible with the governor crate's version,
// since the internal AtomicU64 can't be accessed.

#[derive(Default)]
pub struct InMemoryState(AtomicU64);

impl From<InMemoryState> for u64 {
	fn from(state: InMemoryState) -> Self {
		state.0.into_inner()
	}
}

impl From<u64> for InMemoryState {
	fn from(state: u64) -> Self {
		Self(AtomicU64::new(state))
	}
}

impl InMemoryState {
	pub(crate) fn measure_and_replace_one<T, F, E>(&self, mut f: F) -> std::result::Result<T, E>
	where F: FnMut(Option<Nanos>) -> Result<(T, Nanos), E> {
		let mut prev = self.0.load(Ordering::Acquire);
		let mut decision = f(NonZeroU64::new(prev).map(|n| n.get().into()));
		while let Ok((result, new_data)) = decision {
			match self
				.0
				.compare_exchange_weak(prev, new_data.into(), Ordering::Release, Ordering::Relaxed)
			{
				Ok(_) => return Ok(result),
				Err(next_prev) => prev = next_prev,
			}
			decision = f(NonZeroU64::new(prev).map(|n| n.get().into()));
		}
		// This map shouldn't be needed, as we only get here in the error case, but the compiler
		// can't see it.
		decision.map(|(result, _)| result)
	}

	pub(crate) fn is_older_than(&self, nanos: Nanos) -> bool {
		self.0.load(Ordering::Relaxed) <= nanos.into()
	}
}

impl StateStore for InMemoryState {
	type Key = NotKeyed;

	fn measure_and_replace<T, F, E>(&self, _key: &Self::Key, f: F) -> std::result::Result<T, E>
	where F: Fn(Option<Nanos>) -> Result<(T, Nanos), E> {
		self.measure_and_replace_one(f)
	}
}

impl Debug for InMemoryState {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> std::result::Result<(), fmt::Error> {
		let d = Duration::from_nanos(self.0.load(Ordering::Relaxed));
		write!(f, "InMemoryState({:?})", d)
	}
}

pub struct HashMapStateStore<K>(Mutex<HashMap<K, InMemoryState>>);

impl From<Vec<rate_limit::Model>> for StoredRateLimiterFile<PathKey> {
	fn from(state: Vec<rate_limit::Model>) -> Self {
		let mut vec = Vec::with_capacity(state.len());
		for rate_limit in state {
			vec.push(RateLimiterLine(PathKey(rate_limit.path), rate_limit.state));
		}
		Self(vec)
	}
}

impl Into<Vec<rate_limit::ActiveModel>> for StoredRateLimiterFile<PathKey> {
	fn into(self) -> Vec<rate_limit::ActiveModel> {
		let mut vec = Vec::with_capacity(self.0.len());
		for RateLimiterLine(path, state) in self.0 {
			vec.push(rate_limit::ActiveModel {
				path: Set(path.into()),
				state: Set(state),
			});
		}
		vec
	}
}

impl<K: Hash + Eq + Clone> HashMapStateStore<K> {
	pub fn new() -> Self {
		Self(Mutex::new(HashMap::new()))
	}
}

impl<K: Hash + Eq + Clone> StateStore for HashMapStateStore<K> {
	type Key = K;

	fn measure_and_replace<T, F, E>(&self, key: &Self::Key, f: F) -> std::result::Result<T, E>
	where F: Fn(Option<Nanos>) -> Result<(T, Nanos), E> {
		let mut map = self.0.lock().unwrap();
		if let Some(v) = (*map).get(key) {
			// fast path: a rate limiter is already present for the key.
			return v.measure_and_replace_one(f);
		}
		// not-so-fast path: make a new entry and measure it.
		let entry = (*map).entry(key.clone()).or_insert_with(InMemoryState::default);
		entry.measure_and_replace_one(f)
	}
}

impl<K: Hash + Eq + Clone> ShrinkableKeyedStateStore<K> for HashMapStateStore<K> {
	fn retain_recent(&self, drop_below: Nanos) {
		let mut map = self.0.lock().unwrap();
		map.retain(|_, v| !v.is_older_than(drop_below));
	}

	fn shrink_to_fit(&self) {
		let mut map = self.0.lock().unwrap();
		map.shrink_to_fit();
	}

	fn len(&self) -> usize {
		let map = self.0.lock().unwrap();
		(*map).len()
	}

	fn is_empty(&self) -> bool {
		let map = self.0.lock().unwrap();
		(*map).is_empty()
	}
}

pub struct StoredRateLimiterFile<K>(Vec<RateLimiterLine<K>>);
struct RateLimiterLine<K>(K, u64);

/// Trait for persisting and loading a HashMap state store
pub trait PersistantHashMapStateStore<K> {
	fn load(state: StoredRateLimiterFile<K>) -> Result<Self>
	where Self: Sized;
	fn save(&self) -> Result<StoredRateLimiterFile<K>>;
}

impl<K: Hash + Eq + Clone> PersistantHashMapStateStore<K> for HashMapStateStore<K> {
	fn load(state: StoredRateLimiterFile<K>) -> Result<Self> {
		let store = HashMapStateStore::new();
		let mut map = store.0.lock().unwrap();
		for RateLimiterLine(key, state) in state.0 {
			let state = InMemoryState(state.into());
			map.insert(key, state);
		}
		drop(map);
		Ok(store)
	}

	fn save(&self) -> Result<StoredRateLimiterFile<K>> {
		let map = self.0.lock().unwrap();
		let mut state = Vec::with_capacity(map.len());
		for (key, value) in map.iter() {
			let atomic = value.0.load(Ordering::Relaxed);
			state.push(RateLimiterLine(key.clone(), atomic));
		}

		Ok(StoredRateLimiterFile(state))
	}
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PathKey(String);

impl From<&str> for PathKey {
	fn from(s: &str) -> Self {
		Self(s.to_string())
	}
}

impl From<PathKey> for String {
	fn from(key: PathKey) -> Self {
		key.0
	}
}
