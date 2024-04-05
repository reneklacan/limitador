use crate::counter::Counter;
use crate::storage::atomic_expiring_value::AtomicExpiringValue;
use crate::storage::redis::{
    DEFAULT_MAX_CACHED_COUNTERS, DEFAULT_MAX_TTL_CACHED_COUNTERS_SEC,
    DEFAULT_TTL_RATIO_CACHED_COUNTERS,
};
use moka::sync::Cache;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

pub struct CachedCounterValue {
    value: AtomicExpiringValue,
}

pub struct CountersCache {
    max_ttl_cached_counters: Duration,
    pub ttl_ratio_cached_counters: u64,
    cache: Cache<Counter, Arc<CachedCounterValue>>,
}

impl CachedCounterValue {
    pub fn from(counter: &Counter, value: i64) -> Self {
        let now = SystemTime::now();
        Self {
            value: AtomicExpiringValue::new(value, now + Duration::from_secs(counter.seconds())),
        }
    }

    pub fn delta(&self, counter: &Counter, delta: i64) -> i64 {
        self.value
            .update(delta, counter.seconds(), SystemTime::now())
    }

    pub fn hits(&self, _: &Counter) -> i64 {
        self.value.value_at(SystemTime::now())
    }

    pub fn remaining(&self, counter: &Counter) -> i64 {
        counter.max_value() - self.hits(counter)
    }

    pub fn is_limited(&self, counter: &Counter, delta: i64) -> bool {
        self.hits(counter) as i128 + delta as i128 > counter.max_value() as i128
    }
}

pub struct CountersCacheBuilder {
    max_cached_counters: usize,
    max_ttl_cached_counters: Duration,
    ttl_ratio_cached_counters: u64,
}

impl CountersCacheBuilder {
    pub fn new() -> Self {
        Self {
            max_cached_counters: DEFAULT_MAX_CACHED_COUNTERS,
            max_ttl_cached_counters: Duration::from_secs(DEFAULT_MAX_TTL_CACHED_COUNTERS_SEC),
            ttl_ratio_cached_counters: DEFAULT_TTL_RATIO_CACHED_COUNTERS,
        }
    }

    pub fn max_cached_counters(mut self, max_cached_counters: usize) -> Self {
        self.max_cached_counters = max_cached_counters;
        self
    }

    pub fn max_ttl_cached_counter(mut self, max_ttl_cached_counter: Duration) -> Self {
        self.max_ttl_cached_counters = max_ttl_cached_counter;
        self
    }

    pub fn ttl_ratio_cached_counter(mut self, ttl_ratio_cached_counter: u64) -> Self {
        self.ttl_ratio_cached_counters = ttl_ratio_cached_counter;
        self
    }

    pub fn build(&self) -> CountersCache {
        CountersCache {
            max_ttl_cached_counters: self.max_ttl_cached_counters,
            ttl_ratio_cached_counters: self.ttl_ratio_cached_counters,
            cache: Cache::new(self.max_cached_counters as u64),
        }
    }
}

impl CountersCache {
    pub fn get(&self, counter: &Counter) -> Option<Arc<CachedCounterValue>> {
        self.cache.get(counter)
    }

    pub fn insert(
        &self,
        counter: Counter,
        redis_val: Option<i64>,
        redis_ttl_ms: i64,
        ttl_margin: Duration,
    ) {
        let counter_val = redis_val.unwrap_or(0);
        let counter_ttl = self.ttl_from_redis_ttl(
            redis_ttl_ms,
            counter.seconds(),
            counter_val,
            counter.max_value(),
        );
        if let Some(ttl) = counter_ttl.checked_sub(ttl_margin) {
            if ttl > Duration::from_secs(0) {
                let value = CachedCounterValue::from(&counter, counter_val);
                self.cache.get_with(counter.clone(), || Arc::new(value));
            }
        }
    }

    pub fn increase_by(&self, counter: &Counter, delta: i64) {
        if let Some(val) = self.cache.get(counter) {
            val.delta(counter, delta);
        };
    }

    fn ttl_from_redis_ttl(
        &self,
        redis_ttl_ms: i64,
        counter_seconds: u64,
        counter_val: i64,
        counter_max: i64,
    ) -> Duration {
        // Redis returns -2 when the key does not exist. Ref:
        // https://redis.io/commands/ttl
        // This function returns a ttl of the given counter seconds in this
        // case.

        let counter_ttl = if redis_ttl_ms >= 0 {
            Duration::from_millis(redis_ttl_ms as u64)
        } else {
            Duration::from_secs(counter_seconds)
        };

        // If a counter is already at counter_max, we can cache it for as long as its TTL
        // is in Redis. This does not depend on the requests received by other
        // instances of Limitador. No matter what they do, we know that the
        // counter is not going to recover its quota until it expires in Redis.
        if counter_val >= counter_max {
            return counter_ttl;
        }

        // Expire the counter in the cache before it expires in Redis.
        // There might be several Limitador instances updating the Redis
        // counter. The tradeoff is as follows: the shorter the TTL in the
        // cache, the sooner we'll take into account those updates coming from
        // other instances. If the TTL in the cache is long, there will be less
        // accesses to Redis, so latencies will be better. However, it'll be
        // easier to go over the limits defined, because not taking into account
        // updates from other Limitador instances.
        let mut res =
            Duration::from_millis(counter_ttl.as_millis() as u64 / self.ttl_ratio_cached_counters);

        if res > self.max_ttl_cached_counters {
            res = self.max_ttl_cached_counters;
        }

        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limit::Limit;
    use std::collections::HashMap;

    #[test]
    fn get_existing_counter() {
        let mut values = HashMap::new();
        values.insert("app_id".to_string(), "1".to_string());
        let counter = Counter::new(
            Limit::new(
                "test_namespace",
                10,
                60,
                vec!["req.method == 'POST'"],
                vec!["app_id"],
            ),
            values,
        );

        let cache = CountersCacheBuilder::new().build();
        cache.insert(counter.clone(), Some(10), 10, Duration::from_secs(0));

        assert!(cache.get(&counter).is_some());
    }

    #[test]
    fn get_non_existing_counter() {
        let mut values = HashMap::new();
        values.insert("app_id".to_string(), "1".to_string());
        let counter = Counter::new(
            Limit::new(
                "test_namespace",
                10,
                60,
                vec!["req.method == 'POST'"],
                vec!["app_id"],
            ),
            values,
        );

        let cache = CountersCacheBuilder::new().build();

        assert!(cache.get(&counter).is_none());
    }

    #[test]
    fn insert_saves_the_given_value_when_is_some() {
        let max_val = 10;
        let current_value = max_val / 2;
        let mut values = HashMap::new();
        values.insert("app_id".to_string(), "1".to_string());
        let counter = Counter::new(
            Limit::new(
                "test_namespace",
                max_val,
                60,
                vec!["req.method == 'POST'"],
                vec!["app_id"],
            ),
            values,
        );

        let cache = CountersCacheBuilder::new().build();
        cache.insert(
            counter.clone(),
            Some(current_value),
            10,
            Duration::from_secs(0),
        );

        assert_eq!(
            cache.get(&counter).map(|e| e.hits(&counter)).unwrap(),
            current_value
        );
    }

    #[test]
    fn insert_saves_zero_when_redis_val_is_none() {
        let max_val = 10;
        let mut values = HashMap::new();
        values.insert("app_id".to_string(), "1".to_string());
        let counter = Counter::new(
            Limit::new(
                "test_namespace",
                max_val,
                60,
                vec!["req.method == 'POST'"],
                vec!["app_id"],
            ),
            values,
        );

        let cache = CountersCacheBuilder::new().build();
        cache.insert(counter.clone(), None, 10, Duration::from_secs(0));

        assert_eq!(cache.get(&counter).map(|e| e.hits(&counter)).unwrap(), 0);
    }

    #[test]
    fn increase_by() {
        let current_val = 10;
        let increase_by = 8;
        let mut values = HashMap::new();
        values.insert("app_id".to_string(), "1".to_string());
        let counter = Counter::new(
            Limit::new(
                "test_namespace",
                current_val,
                60,
                vec!["req.method == 'POST'"],
                vec!["app_id"],
            ),
            values,
        );

        let mut cache = CountersCacheBuilder::new().build();
        cache.insert(
            counter.clone(),
            Some(current_val),
            10,
            Duration::from_secs(0),
        );
        cache.increase_by(&counter, increase_by);

        assert_eq!(
            cache.get(&counter).map(|e| e.hits(&counter)).unwrap(),
            (current_val + increase_by)
        );
    }
}
