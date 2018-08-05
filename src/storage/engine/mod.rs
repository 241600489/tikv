// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::boxed::FnBox;
use std::cmp::Ordering;
use std::fmt::Debug;
use std::time::Duration;
use std::{error, result};

pub use self::rocksdb::{RocksEngine, RocksSnapshot};
use kvproto::errorpb::Error as ErrorHeader;
use kvproto::kvrpcpb::{Context, ScanDetail, ScanInfo};
use raftstore::store::{SeekRegionFilter, SeekRegionResult};
use rocksdb::{ColumnFamilyOptions, TablePropertiesCollection};
use storage::{CfName, Key, Value, CF_DEFAULT, CF_LOCK, CF_RAFT, CF_WRITE};

use config;

use util::rocksdb::CFOptions;

mod metrics;
mod perf_context;
pub mod raftkv;
mod rocksdb;
use super::super::raftstore::store::engine::IterOption;

pub use self::perf_context::{PerfStatisticsDelta, PerfStatisticsInstant};

// only used for rocksdb without persistent.
pub const TEMP_DIR: &str = "";

const SEEK_BOUND: usize = 30;
const DEFAULT_TIMEOUT_SECS: u64 = 5;

const STAT_TOTAL: &str = "total";
const STAT_PROCESSED: &str = "processed";
const STAT_GET: &str = "get";
const STAT_NEXT: &str = "next";
const STAT_PREV: &str = "prev";
const STAT_SEEK: &str = "seek";
const STAT_SEEK_FOR_PREV: &str = "seek_for_prev";
const STAT_OVER_SEEK_BOUND: &str = "over_seek_bound";

pub type Callback<T> = Box<FnBox((CbContext, Result<T>)) + Send>;
pub type BatchResults<T> = Vec<Option<(CbContext, Result<T>)>>;
pub type BatchCallback<T> = Box<FnBox(BatchResults<T>) + Send>;

#[derive(Clone, Debug)]
pub struct CbContext {
    pub term: Option<u64>,
}

impl CbContext {
    fn new() -> CbContext {
        CbContext { term: None }
    }
}

#[derive(Debug)]
pub enum Modify {
    Delete(CfName, Key),
    Put(CfName, Key, Value),
    DeleteRange(CfName, Key, Key),
}

pub trait Engine: Send + Debug + Clone + Sized + 'static {
    type Iter: Iterator;
    type Snap: Snapshot<Iter = Self::Iter>;

    fn async_write(&self, ctx: &Context, batch: Vec<Modify>, callback: Callback<()>) -> Result<()>;
    fn async_snapshot(&self, ctx: &Context, callback: Callback<Self::Snap>) -> Result<()>;
    /// Snapshots are token by `Context`s, the results are send to the `on_finished` callback,
    /// with the same order. If a read-index is occurred, a `None` is placed in the corresponding
    /// slot, and the caller is responsible for reissuing it again, in `async_snapshot`.
    // TODO:
    //   - replace Option with Result and define an Error for requiring read-index.
    //   - add a new method for force read-index, that may be done
    //     by renaming the `async_snapshot`.
    fn async_batch_snapshot(
        &self,
        batch: Vec<Context>,
        on_finished: BatchCallback<Self::Snap>,
    ) -> Result<()>;

    fn write(&self, ctx: &Context, batch: Vec<Modify>) -> Result<()> {
        let timeout = Duration::from_secs(DEFAULT_TIMEOUT_SECS);
        match wait_op!(|cb| self.async_write(ctx, batch, cb), timeout) {
            Some((_, res)) => res,
            None => Err(Error::Timeout(timeout)),
        }
    }

    fn snapshot(&self, ctx: &Context) -> Result<Self::Snap> {
        let timeout = Duration::from_secs(DEFAULT_TIMEOUT_SECS);
        match wait_op!(|cb| self.async_snapshot(ctx, cb), timeout) {
            Some((_, res)) => res,
            None => Err(Error::Timeout(timeout)),
        }
    }

    fn put(&self, ctx: &Context, key: Key, value: Value) -> Result<()> {
        self.put_cf(ctx, CF_DEFAULT, key, value)
    }

    fn put_cf(&self, ctx: &Context, cf: CfName, key: Key, value: Value) -> Result<()> {
        self.write(ctx, vec![Modify::Put(cf, key, value)])
    }

    fn delete(&self, ctx: &Context, key: Key) -> Result<()> {
        self.delete_cf(ctx, CF_DEFAULT, key)
    }

    fn delete_cf(&self, ctx: &Context, cf: CfName, key: Key) -> Result<()> {
        self.write(ctx, vec![Modify::Delete(cf, key)])
    }
}

pub trait Snapshot: Send + Debug + Clone + Sized {
    type Iter: Iterator;

    fn get(&self, key: &Key) -> Result<Option<Value>>;
    fn get_cf(&self, cf: CfName, key: &Key) -> Result<Option<Value>>;
    #[cfg_attr(feature = "cargo-clippy", allow(needless_lifetimes))]
    fn iter(&self, iter_opt: IterOption, mode: ScanMode) -> Result<Cursor<Self::Iter>>;
    #[cfg_attr(feature = "cargo-clippy", allow(needless_lifetimes))]
    fn iter_cf(
        &self,
        cf: CfName,
        iter_opt: IterOption,
        mode: ScanMode,
    ) -> Result<Cursor<Self::Iter>>;
    fn get_properties(&self) -> Result<TablePropertiesCollection> {
        self.get_properties_cf(CF_DEFAULT)
    }
    fn get_properties_cf(&self, _: CfName) -> Result<TablePropertiesCollection> {
        Err(Error::RocksDb("no user properties".to_owned()))
    }
}

pub trait Iterator: Send + Sized {
    fn next(&mut self) -> bool;
    fn prev(&mut self) -> bool;
    fn seek(&mut self, key: &Key) -> Result<bool>;
    fn seek_for_prev(&mut self, key: &Key) -> Result<bool>;
    fn seek_to_first(&mut self) -> bool;
    fn seek_to_last(&mut self) -> bool;
    fn valid(&self) -> bool;

    fn validate_key(&self, _: &Key) -> Result<()> {
        Ok(())
    }

    fn key(&self) -> &[u8];
    fn value(&self) -> &[u8];
}

pub trait RegionInfoProvider: Send + Sized + Clone + 'static {
    /// Find the first region `r` whose range contains or greater than `from_key` and the peer on
    /// this TiKV satisfies `filter(peer)` returns true.
    fn seek_region(
        &self,
        from: &[u8],
        filter: SeekRegionFilter,
        limit: u32,
    ) -> Result<SeekRegionResult>;
}

macro_rules! near_loop {
    ($cond:expr, $fallback:expr, $st:expr) => {{
        let mut cnt = 0;
        while $cond {
            cnt += 1;
            if cnt >= SEEK_BOUND {
                $st.over_seek_bound += 1;
                return $fallback;
            }
        }
    }};
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum ScanMode {
    Forward,
    Backward,
    Mixed,
}

/// Statistics collects the ops taken when fetching data.
#[derive(Default, Clone, Debug)]
pub struct CFStatistics {
    // How many keys that's effective to user. This counter should be increased
    // by the caller.
    pub processed: usize,
    pub get: usize,
    pub next: usize,
    pub prev: usize,
    pub seek: usize,
    pub seek_for_prev: usize,
    pub over_seek_bound: usize,
    pub flow_stats: FlowStatistics,
}

#[derive(Default, Debug, Clone)]
pub struct FlowStatistics {
    pub read_keys: usize,
    pub read_bytes: usize,
}

impl FlowStatistics {
    pub fn add(&mut self, other: &Self) {
        self.read_bytes = self.read_keys.saturating_add(other.read_bytes);
        self.read_keys = self.read_keys.saturating_add(other.read_keys);
    }
}

impl CFStatistics {
    #[inline]
    pub fn total_op_count(&self) -> usize {
        self.get + self.next + self.prev + self.seek + self.seek_for_prev
    }

    pub fn details(&self) -> Vec<(&str, usize)> {
        vec![
            (STAT_TOTAL, self.total_op_count()),
            (STAT_PROCESSED, self.processed),
            (STAT_GET, self.get),
            (STAT_NEXT, self.next),
            (STAT_PREV, self.prev),
            (STAT_SEEK, self.seek),
            (STAT_SEEK_FOR_PREV, self.seek_for_prev),
            (STAT_OVER_SEEK_BOUND, self.over_seek_bound),
        ]
    }

    pub fn add(&mut self, other: &Self) {
        self.processed = self.processed.saturating_add(other.processed);
        self.get = self.get.saturating_add(other.get);
        self.next = self.next.saturating_add(other.next);
        self.prev = self.prev.saturating_add(other.prev);
        self.seek = self.seek.saturating_add(other.seek);
        self.seek_for_prev = self.seek_for_prev.saturating_add(other.seek_for_prev);
        self.over_seek_bound = self.over_seek_bound.saturating_add(other.over_seek_bound);
        self.flow_stats.add(&other.flow_stats);
    }

    pub fn scan_info(&self) -> ScanInfo {
        let mut info = ScanInfo::new();
        info.set_processed(self.processed as i64);
        info.set_total(self.total_op_count() as i64);
        info
    }
}

#[derive(Default, Clone, Debug)]
pub struct Statistics {
    pub lock: CFStatistics,
    pub write: CFStatistics,
    pub data: CFStatistics,
}

impl Statistics {
    pub fn total_op_count(&self) -> usize {
        self.lock.total_op_count() + self.write.total_op_count() + self.data.total_op_count()
    }

    pub fn total_processed(&self) -> usize {
        self.lock.processed + self.write.processed + self.data.processed
    }

    pub fn details(&self) -> Vec<(&str, Vec<(&str, usize)>)> {
        vec![
            (CF_DEFAULT, self.data.details()),
            (CF_LOCK, self.lock.details()),
            (CF_WRITE, self.write.details()),
        ]
    }

    pub fn add(&mut self, other: &Self) {
        self.lock.add(&other.lock);
        self.write.add(&other.write);
        self.data.add(&other.data);
    }

    pub fn scan_detail(&self) -> ScanDetail {
        let mut detail = ScanDetail::new();
        detail.set_data(self.data.scan_info());
        detail.set_lock(self.lock.scan_info());
        detail.set_write(self.write.scan_info());
        detail
    }

    pub fn mut_cf_statistics(&mut self, cf: &str) -> &mut CFStatistics {
        if cf.is_empty() {
            return &mut self.data;
        }
        match cf {
            CF_DEFAULT => &mut self.data,
            CF_LOCK => &mut self.lock,
            CF_WRITE => &mut self.write,
            _ => unreachable!(),
        }
    }
}

#[derive(Default, Debug)]
pub struct StatisticsSummary {
    pub stat: Statistics,
    pub count: u64,
}

impl StatisticsSummary {
    pub fn add_statistics(&mut self, v: &Statistics) {
        self.stat.add(v);
        self.count += 1;
    }
}

pub struct Cursor<I: Iterator> {
    iter: I,
    scan_mode: ScanMode,
    // the data cursor can be seen will be
    min_key: Option<Vec<u8>>,
    max_key: Option<Vec<u8>>,

    is_key_read: bool,
    is_value_read: bool,
}

impl<I: Iterator> Cursor<I> {
    pub fn new(iter: I, mode: ScanMode) -> Self {
        Self {
            iter,
            scan_mode: mode,
            min_key: None,
            max_key: None,

            is_key_read: false,
            is_value_read: false,
        }
    }

    pub fn seek(&mut self, key: &Key, statistics: &mut CFStatistics) -> Result<bool> {
        assert_ne!(self.scan_mode, ScanMode::Backward);
        if self.max_key.as_ref().map_or(false, |k| k <= key.encoded()) {
            self.iter.validate_key(key)?;
            return Ok(false);
        }

        if !self.internal_seek(key, statistics)? {
            self.max_key = Some(key.encoded().to_owned());
            return Ok(false);
        }
        Ok(true)
    }

    /// Seek the specified key.
    ///
    /// When specified key < current key:
    ///     In forward mode:
    ///         `allow_reseek == true`: There will be a `seek()`.
    ///         `allow_reseek == false`: No operation will be performed.
    ///     In mixed mode:
    ///         There will be some `prev()` first, then a `seek()`.
    ///
    /// This method assume the current position of cursor is
    /// around `key`, otherwise you should use `seek` instead.
    pub fn near_seek(
        &mut self,
        key: &Key,
        allow_reseek: bool,
        statistics: &mut CFStatistics,
    ) -> Result<bool> {
        assert_ne!(self.scan_mode, ScanMode::Backward);
        if !self.iter.valid() {
            return self.seek(key, statistics);
        }
        let ord = self.key(statistics).cmp(key.encoded());
        if ord == Ordering::Equal {
            return Ok(true);
        }
        if ord == Ordering::Greater && self.scan_mode == ScanMode::Forward {
            // current key > specified key
            if allow_reseek {
                return self.seek(key, statistics);
            } else {
                return Ok(true);
            }
        }
        if self.max_key.as_ref().map_or(false, |k| k <= key.encoded()) {
            self.iter.validate_key(key)?;
            return Ok(false);
        }
        if ord == Ordering::Greater {
            near_loop!(
                self.prev(statistics) && self.key(statistics) > key.encoded().as_slice(),
                self.seek(key, statistics),
                statistics
            );
            if self.iter.valid() {
                if self.key(statistics) < key.encoded().as_slice() {
                    self.next(statistics);
                }
            } else {
                assert!(self.seek_to_first(statistics));
                return Ok(true);
            }
        } else {
            // ord == Less
            near_loop!(
                self.next(statistics) && self.key(statistics) < key.encoded().as_slice(),
                self.seek(key, statistics),
                statistics
            );
        }
        if !self.iter.valid() {
            self.max_key = Some(key.encoded().to_owned());
            return Ok(false);
        }
        Ok(true)
    }

    /// Get the value of specified key by using `near_seek_get`.
    ///
    /// This method assume the current position of cursor is
    /// around `key`, otherwise you should `seek` first.
    ///
    /// TODO: Remove this function.
    pub fn near_seek_get(
        &mut self,
        key: &Key,
        allow_reseek: bool,
        statistics: &mut CFStatistics,
    ) -> Result<Option<&[u8]>> {
        let seek_result = match self.scan_mode {
            ScanMode::Forward | ScanMode::Mixed => self.near_seek(key, allow_reseek, statistics),
            ScanMode::Backward => self.near_seek_for_prev(key, allow_reseek, statistics),
        };
        if seek_result? && self.key(statistics) == &**key.encoded() {
            Ok(Some(self.value(statistics)))
        } else {
            Ok(None)
        }
    }

    fn seek_for_prev(&mut self, key: &Key, statistics: &mut CFStatistics) -> Result<bool> {
        assert_ne!(self.scan_mode, ScanMode::Forward);
        if self.min_key.as_ref().map_or(false, |k| k >= key.encoded()) {
            self.iter.validate_key(key)?;
            return Ok(false);
        }

        if !self.internal_seek_for_prev(key, statistics)? {
            self.min_key = Some(key.encoded().to_owned());
            return Ok(false);
        }
        Ok(true)
    }

    /// Find the largest key that is not greater than the specific key.
    ///
    /// When specified key > current key:
    ///     In backward mode:
    ///         `allow_reseek == true`: There will be a `seek_for_prev()`.
    ///         `allow_reseek == false`: No operation will be performed.
    ///     In mixed mode:
    ///         There will be some `next()` first, then a `seek_for_prev()`.
    pub fn near_seek_for_prev(
        &mut self,
        key: &Key,
        allow_reseek: bool,
        statistics: &mut CFStatistics,
    ) -> Result<bool> {
        assert_ne!(self.scan_mode, ScanMode::Forward);
        if !self.iter.valid() {
            return self.seek_for_prev(key, statistics);
        }
        let ord = self.key(statistics).cmp(key.encoded());
        if ord == Ordering::Equal {
            return Ok(true);
        }
        if ord == Ordering::Less && self.scan_mode == ScanMode::Backward {
            // current key < specified key
            if allow_reseek {
                return self.seek_for_prev(key, statistics);
            } else {
                return Ok(true);
            }
        }
        if self.min_key.as_ref().map_or(false, |k| k >= key.encoded()) {
            self.iter.validate_key(key)?;
            return Ok(false);
        }
        if ord == Ordering::Less {
            near_loop!(
                self.next(statistics) && self.key(statistics) < key.encoded().as_slice(),
                self.seek_for_prev(key, statistics),
                statistics
            );
            if self.iter.valid() {
                if self.key(statistics) > key.encoded().as_slice() {
                    self.prev(statistics);
                }
            } else {
                assert!(self.seek_to_last(statistics));
                return Ok(true);
            }
        } else {
            near_loop!(
                self.prev(statistics) && self.key(statistics) > key.encoded().as_slice(),
                self.seek_for_prev(key, statistics),
                statistics
            );
        }
        if !self.iter.valid() {
            self.min_key = Some(key.encoded().to_owned());
            return Ok(false);
        }
        Ok(true)
    }

    pub fn reverse_seek(&mut self, key: &Key, statistics: &mut CFStatistics) -> Result<bool> {
        if !self.seek_for_prev(key, statistics)? {
            return Ok(false);
        }

        if self.key(statistics) == &**key.encoded() {
            // should not update min_key here. otherwise reverse_seek_le may not
            // work as expected.
            return Ok(self.prev(statistics));
        }

        Ok(true)
    }

    /// Reverse seek the specified key.
    ///
    /// This method assume the current position of cursor is
    /// around `key`, otherwise you should use `reverse_seek` instead.
    pub fn near_reverse_seek(
        &mut self,
        key: &Key,
        allow_reseek: bool,
        statistics: &mut CFStatistics,
    ) -> Result<bool> {
        if !self.near_seek_for_prev(key, allow_reseek, statistics)? {
            return Ok(false);
        }

        if self.key(statistics) == &**key.encoded() {
            return Ok(self.prev(statistics));
        }

        Ok(true)
    }

    #[inline]
    pub fn key(&mut self, statistics: &mut CFStatistics) -> &[u8] {
        let key = self.iter.key();
        if !self.is_key_read {
            self.is_key_read = true;
            statistics.flow_stats.read_bytes += key.len();
            statistics.flow_stats.read_keys += 1;
        }
        key
    }

    #[inline]
    pub fn value(&mut self, statistics: &mut CFStatistics) -> &[u8] {
        let value = self.iter.value();
        if !self.is_value_read {
            self.is_value_read = true;
            statistics.flow_stats.read_bytes += value.len();
        }
        value
    }

    #[inline]
    pub fn seek_to_first(&mut self, statistics: &mut CFStatistics) -> bool {
        statistics.seek += 1;
        self.is_key_read = false;
        self.is_value_read = false;
        self.iter.seek_to_first()
    }

    #[inline]
    pub fn seek_to_last(&mut self, statistics: &mut CFStatistics) -> bool {
        statistics.seek += 1;
        self.is_key_read = false;
        self.is_value_read = false;
        self.iter.seek_to_last()
    }

    #[inline]
    pub fn internal_seek(&mut self, key: &Key, statistics: &mut CFStatistics) -> Result<bool> {
        statistics.seek += 1;
        self.is_key_read = false;
        self.is_value_read = false;
        self.iter.seek(key)
    }

    #[inline]
    pub fn internal_seek_for_prev(
        &mut self,
        key: &Key,
        statistics: &mut CFStatistics,
    ) -> Result<bool> {
        statistics.seek_for_prev += 1;
        self.is_key_read = false;
        self.is_value_read = false;
        self.iter.seek_for_prev(key)
    }

    #[inline]
    pub fn next(&mut self, statistics: &mut CFStatistics) -> bool {
        statistics.next += 1;
        self.is_key_read = false;
        self.is_value_read = false;
        self.iter.next()
    }

    #[inline]
    pub fn prev(&mut self, statistics: &mut CFStatistics) -> bool {
        statistics.prev += 1;
        self.is_key_read = false;
        self.is_value_read = false;
        self.iter.prev()
    }

    #[inline]
    pub fn valid(&self) -> bool {
        self.iter.valid()
    }
}

/// Create a local Rocskdb engine. (Without raft, mainly for tests).
pub fn new_local_engine(path: &str, cfs: &[CfName]) -> Result<RocksEngine> {
    let mut cfs_opts = Vec::with_capacity(cfs.len());
    let cfg_rocksdb = config::DbConfig::default();
    for cf in cfs {
        let cf_opt = match *cf {
            CF_DEFAULT => CFOptions::new(CF_DEFAULT, cfg_rocksdb.defaultcf.build_opt()),
            CF_LOCK => CFOptions::new(CF_LOCK, cfg_rocksdb.lockcf.build_opt()),
            CF_WRITE => CFOptions::new(CF_WRITE, cfg_rocksdb.writecf.build_opt()),
            CF_RAFT => CFOptions::new(CF_RAFT, cfg_rocksdb.raftcf.build_opt()),
            _ => CFOptions::new(*cf, ColumnFamilyOptions::new()),
        };
        cfs_opts.push(cf_opt);
    }
    RocksEngine::new(path, cfs, Some(cfs_opts))
}

quick_error! {
    #[derive(Debug)]
    pub enum Error {
        Request(err: ErrorHeader) {
            from()
            description("request to underhook engine failed")
            display("{:?}", err)
        }
        RocksDb(msg: String) {
            from()
            description("RocksDb error")
            display("RocksDb {}", msg)
        }
        Timeout(d: Duration) {
            description("request timeout")
            display("timeout after {:?}", d)
        }
        EmptyRequest {
            description("an empty request")
            display("an empty request")
        }
        Other(err: Box<error::Error + Send + Sync>) {
            from()
            cause(err.as_ref())
            description(err.description())
            display("unknown error {:?}", err)
        }
    }
}

impl Error {
    pub fn maybe_clone(&self) -> Option<Error> {
        match *self {
            Error::Request(ref e) => Some(Error::Request(e.clone())),
            Error::RocksDb(ref msg) => Some(Error::RocksDb(msg.clone())),
            Error::Timeout(d) => Some(Error::Timeout(d)),
            Error::EmptyRequest => Some(Error::EmptyRequest),
            Error::Other(_) => None,
        }
    }
}

pub type Result<T> = result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::super::super::raftstore::store::engine::IterOption;
    use super::SEEK_BOUND;
    use super::*;
    use kvproto::kvrpcpb::Context;
    use storage::{make_key, CfName, CF_DEFAULT};
    use tempdir::TempDir;
    use util::codec::bytes;
    use util::escape;

    const TEST_ENGINE_CFS: &[CfName] = &["cf"];

    #[test]
    fn rocksdb() {
        let dir = TempDir::new("rocksdb_test").unwrap();
        let engine = new_local_engine(dir.path().to_str().unwrap(), TEST_ENGINE_CFS).unwrap();

        test_get_put(&engine);
        test_batch(&engine);
        test_empty_seek(&engine);
        test_seek(&engine);
        test_near_seek(&engine);
        test_cf(&engine);
        test_empty_write(&engine);
        test_empty_batch_snapshot(&engine);
    }

    #[test]
    fn rocksdb_reopen() {
        let dir = TempDir::new("rocksdb_test").unwrap();
        {
            let engine = new_local_engine(dir.path().to_str().unwrap(), TEST_ENGINE_CFS).unwrap();
            must_put_cf(&engine, "cf", b"k", b"v1");
        }
        {
            let engine = new_local_engine(dir.path().to_str().unwrap(), TEST_ENGINE_CFS).unwrap();
            assert_has_cf(&engine, "cf", b"k", b"v1");
        }
    }

    fn must_put<E: Engine>(engine: &E, key: &[u8], value: &[u8]) {
        engine
            .put(&Context::new(), make_key(key), value.to_vec())
            .unwrap();
    }

    fn must_put_cf<E: Engine>(engine: &E, cf: CfName, key: &[u8], value: &[u8]) {
        engine
            .put_cf(&Context::new(), cf, make_key(key), value.to_vec())
            .unwrap();
    }

    fn must_delete<E: Engine>(engine: &E, key: &[u8]) {
        engine.delete(&Context::new(), make_key(key)).unwrap();
    }

    fn must_delete_cf<E: Engine>(engine: &E, cf: CfName, key: &[u8]) {
        engine
            .delete_cf(&Context::new(), cf, make_key(key))
            .unwrap();
    }

    fn assert_has<E: Engine>(engine: &E, key: &[u8], value: &[u8]) {
        let snapshot = engine.snapshot(&Context::new()).unwrap();
        assert_eq!(snapshot.get(&make_key(key)).unwrap().unwrap(), value);
    }

    fn assert_has_cf<E: Engine>(engine: &E, cf: CfName, key: &[u8], value: &[u8]) {
        let snapshot = engine.snapshot(&Context::new()).unwrap();
        assert_eq!(snapshot.get_cf(cf, &make_key(key)).unwrap().unwrap(), value);
    }

    fn assert_none<E: Engine>(engine: &E, key: &[u8]) {
        let snapshot = engine.snapshot(&Context::new()).unwrap();
        assert_eq!(snapshot.get(&make_key(key)).unwrap(), None);
    }

    fn assert_none_cf<E: Engine>(engine: &E, cf: CfName, key: &[u8]) {
        let snapshot = engine.snapshot(&Context::new()).unwrap();
        assert_eq!(snapshot.get_cf(cf, &make_key(key)).unwrap(), None);
    }

    fn assert_seek<E: Engine>(engine: &E, key: &[u8], pair: (&[u8], &[u8])) {
        let snapshot = engine.snapshot(&Context::new()).unwrap();
        let mut iter = snapshot
            .iter(IterOption::default(), ScanMode::Mixed)
            .unwrap();
        let mut statistics = CFStatistics::default();
        iter.seek(&make_key(key), &mut statistics).unwrap();
        assert_eq!(iter.key(&mut statistics), &*bytes::encode_bytes(pair.0));
        assert_eq!(iter.value(&mut statistics), pair.1);
    }

    fn assert_reverse_seek<E: Engine>(engine: &E, key: &[u8], pair: (&[u8], &[u8])) {
        let snapshot = engine.snapshot(&Context::new()).unwrap();
        let mut iter = snapshot
            .iter(IterOption::default(), ScanMode::Mixed)
            .unwrap();
        let mut statistics = CFStatistics::default();
        iter.reverse_seek(&make_key(key), &mut statistics).unwrap();
        assert_eq!(iter.key(&mut statistics), &*bytes::encode_bytes(pair.0));
        assert_eq!(iter.value(&mut statistics), pair.1);
    }

    fn assert_near_seek<I: Iterator>(cursor: &mut Cursor<I>, key: &[u8], pair: (&[u8], &[u8])) {
        let mut statistics = CFStatistics::default();
        assert!(
            cursor
                .near_seek(&make_key(key), false, &mut statistics)
                .unwrap(),
            escape(key)
        );
        assert_eq!(cursor.key(&mut statistics), &*bytes::encode_bytes(pair.0));
        assert_eq!(cursor.value(&mut statistics), pair.1);
    }

    fn assert_near_reverse_seek<I: Iterator>(
        cursor: &mut Cursor<I>,
        key: &[u8],
        pair: (&[u8], &[u8]),
    ) {
        let mut statistics = CFStatistics::default();
        assert!(
            cursor
                .near_reverse_seek(&make_key(key), false, &mut statistics)
                .unwrap(),
            escape(key)
        );
        assert_eq!(cursor.key(&mut statistics), &*bytes::encode_bytes(pair.0));
        assert_eq!(cursor.value(&mut statistics), pair.1);
    }

    fn test_get_put<E: Engine>(engine: &E) {
        assert_none(engine, b"x");
        must_put(engine, b"x", b"1");
        assert_has(engine, b"x", b"1");
        must_put(engine, b"x", b"2");
        assert_has(engine, b"x", b"2");
    }

    fn test_batch<E: Engine>(engine: &E) {
        engine
            .write(
                &Context::new(),
                vec![
                    Modify::Put(CF_DEFAULT, make_key(b"x"), b"1".to_vec()),
                    Modify::Put(CF_DEFAULT, make_key(b"y"), b"2".to_vec()),
                ],
            )
            .unwrap();
        assert_has(engine, b"x", b"1");
        assert_has(engine, b"y", b"2");

        engine
            .write(
                &Context::new(),
                vec![
                    Modify::Delete(CF_DEFAULT, make_key(b"x")),
                    Modify::Delete(CF_DEFAULT, make_key(b"y")),
                ],
            )
            .unwrap();
        assert_none(engine, b"y");
        assert_none(engine, b"y");
    }

    fn test_seek<E: Engine>(engine: &E) {
        must_put(engine, b"x", b"1");
        assert_seek(engine, b"x", (b"x", b"1"));
        assert_seek(engine, b"a", (b"x", b"1"));
        assert_reverse_seek(engine, b"x1", (b"x", b"1"));
        must_put(engine, b"z", b"2");
        assert_seek(engine, b"y", (b"z", b"2"));
        assert_seek(engine, b"x\x00", (b"z", b"2"));
        assert_reverse_seek(engine, b"y", (b"x", b"1"));
        assert_reverse_seek(engine, b"z", (b"x", b"1"));
        let snapshot = engine.snapshot(&Context::new()).unwrap();
        let mut iter = snapshot
            .iter(IterOption::default(), ScanMode::Mixed)
            .unwrap();
        let mut statistics = CFStatistics::default();
        assert!(!iter.seek(&make_key(b"z\x00"), &mut statistics).unwrap());
        assert!(!iter.reverse_seek(&make_key(b"x"), &mut statistics).unwrap());
        must_delete(engine, b"x");
        must_delete(engine, b"z");
    }

    fn test_near_seek<E: Engine>(engine: &E) {
        must_put(engine, b"x", b"1");
        must_put(engine, b"z", b"2");
        let snapshot = engine.snapshot(&Context::new()).unwrap();
        let mut cursor = snapshot
            .iter(IterOption::default(), ScanMode::Mixed)
            .unwrap();
        assert_near_seek(&mut cursor, b"x", (b"x", b"1"));
        assert_near_seek(&mut cursor, b"a", (b"x", b"1"));
        assert_near_reverse_seek(&mut cursor, b"z1", (b"z", b"2"));
        assert_near_reverse_seek(&mut cursor, b"x1", (b"x", b"1"));
        assert_near_seek(&mut cursor, b"y", (b"z", b"2"));
        assert_near_seek(&mut cursor, b"x\x00", (b"z", b"2"));
        let mut statistics = CFStatistics::default();
        assert!(
            !cursor
                .near_seek(&make_key(b"z\x00"), false, &mut statistics)
                .unwrap()
        );
        // Insert many key-values between 'x' and 'z' then near_seek will fallback to seek.
        for i in 0..super::SEEK_BOUND {
            let key = format!("y{}", i);
            must_put(engine, key.as_bytes(), b"3");
        }
        let snapshot = engine.snapshot(&Context::new()).unwrap();
        let mut cursor = snapshot
            .iter(IterOption::default(), ScanMode::Mixed)
            .unwrap();
        assert_near_seek(&mut cursor, b"x", (b"x", b"1"));
        assert_near_seek(&mut cursor, b"z", (b"z", b"2"));

        must_delete(engine, b"x");
        must_delete(engine, b"z");
        for i in 0..super::SEEK_BOUND {
            let key = format!("y{}", i);
            must_delete(engine, key.as_bytes());
        }
    }

    fn test_empty_seek<E: Engine>(engine: &E) {
        let snapshot = engine.snapshot(&Context::new()).unwrap();
        let mut cursor = snapshot
            .iter(IterOption::default(), ScanMode::Mixed)
            .unwrap();
        let mut statistics = CFStatistics::default();
        assert!(
            !cursor
                .near_reverse_seek(&make_key(b"x"), false, &mut statistics)
                .unwrap()
        );
        assert!(
            !cursor
                .near_reverse_seek(&make_key(b"z"), false, &mut statistics)
                .unwrap()
        );
        assert!(
            !cursor
                .near_reverse_seek(&make_key(b"w"), false, &mut statistics)
                .unwrap()
        );
        assert!(
            !cursor
                .near_seek(&make_key(b"x"), false, &mut statistics)
                .unwrap()
        );
        assert!(
            !cursor
                .near_seek(&make_key(b"z"), false, &mut statistics)
                .unwrap()
        );
        assert!(
            !cursor
                .near_seek(&make_key(b"w"), false, &mut statistics)
                .unwrap()
        );
    }

    macro_rules! assert_seek {
        ($cursor:ident, $func:ident, $k:expr, $res:ident) => {{
            let mut statistics = CFStatistics::default();
            assert_eq!(
                $cursor.$func(&$k, &mut statistics).unwrap(),
                $res.is_some(),
                "assert_seek {} failed exp {:?}",
                $k,
                $res
            );
            if let Some((ref k, ref v)) = $res {
                assert_eq!(
                    $cursor.key(&mut statistics),
                    bytes::encode_bytes(k.as_bytes()).as_slice()
                );
                assert_eq!($cursor.value(&mut statistics), v.as_bytes());
            }
        }};
    }

    macro_rules! assert_near_seek {
        ($cursor:ident, $func:ident, $k:expr, $res:ident) => {{
            let mut statistics = CFStatistics::default();
            assert_eq!(
                $cursor.$func(&$k, false, &mut statistics).unwrap(),
                $res.is_some(),
                "assert_near_seek {} failed exp {:?}",
                $k,
                $res
            );
            if let Some((ref k, ref v)) = $res {
                assert_eq!(
                    $cursor.key(&mut statistics),
                    bytes::encode_bytes(k.as_bytes()).as_slice()
                );
                assert_eq!($cursor.value(&mut statistics), v.as_bytes());
            }
        }};
    }

    #[derive(PartialEq, Eq, Clone, Copy)]
    enum SeekMode {
        Normal,
        Reverse,
        ForPrev,
    }

    // use step to control the distance between target key and current key in cursor.
    fn test_linear_seek<S: Snapshot>(
        snapshot: &S,
        mode: ScanMode,
        seek_mode: SeekMode,
        start_idx: usize,
        step: usize,
    ) {
        let mut cursor = snapshot.iter(IterOption::default(), mode).unwrap();
        let mut near_cursor = snapshot.iter(IterOption::default(), mode).unwrap();
        let limit = (SEEK_BOUND * 10 + 50 - 1) * 2;

        for (_, mut i) in (start_idx..(SEEK_BOUND * 30))
            .enumerate()
            .filter(|&(i, _)| i % step == 0)
        {
            if seek_mode != SeekMode::Normal {
                i = SEEK_BOUND * 30 - 1 - i;
            }
            let key = format!("key_{:03}", i);
            let seek_key = make_key(key.as_bytes());
            let exp_kv = if i <= 100 {
                match seek_mode {
                    SeekMode::Reverse => None,
                    SeekMode::ForPrev if i < 100 => None,
                    SeekMode::Normal | SeekMode::ForPrev => {
                        Some(("key_100".to_owned(), "value_50".to_owned()))
                    }
                }
            } else if i <= limit {
                if seek_mode == SeekMode::Reverse {
                    Some((
                        format!("key_{}", (i - 1) / 2 * 2),
                        format!("value_{}", (i - 1) / 2),
                    ))
                } else if seek_mode == SeekMode::ForPrev {
                    Some((format!("key_{}", i / 2 * 2), format!("value_{}", i / 2)))
                } else {
                    Some((
                        format!("key_{}", (i + 1) / 2 * 2),
                        format!("value_{}", (i + 1) / 2),
                    ))
                }
            } else if seek_mode != SeekMode::Normal {
                Some((
                    format!("key_{:03}", limit),
                    format!("value_{:03}", limit / 2),
                ))
            } else {
                None
            };

            match seek_mode {
                SeekMode::Reverse => {
                    assert_seek!(cursor, reverse_seek, seek_key, exp_kv);
                    assert_near_seek!(near_cursor, near_reverse_seek, seek_key, exp_kv);
                }
                SeekMode::Normal => {
                    assert_seek!(cursor, seek, seek_key, exp_kv);
                    assert_near_seek!(near_cursor, near_seek, seek_key, exp_kv);
                }
                SeekMode::ForPrev => {
                    assert_seek!(cursor, seek_for_prev, seek_key, exp_kv);
                    assert_near_seek!(near_cursor, near_seek_for_prev, seek_key, exp_kv);
                }
            }
        }
    }

    // TODO: refactor engine tests
    #[test]
    fn test_linear() {
        let dir = TempDir::new("rocksdb_test").unwrap();
        let engine = new_local_engine(dir.path().to_str().unwrap(), TEST_ENGINE_CFS).unwrap();
        for i in 50..50 + SEEK_BOUND * 10 {
            let key = format!("key_{}", i * 2);
            let value = format!("value_{}", i);
            must_put(&engine, key.as_bytes(), value.as_bytes());
        }
        let snapshot = engine.snapshot(&Context::new()).unwrap();

        for step in 1..SEEK_BOUND * 3 {
            for start in 0..10 {
                test_linear_seek(
                    &snapshot,
                    ScanMode::Forward,
                    SeekMode::Normal,
                    start * SEEK_BOUND,
                    step,
                );
                test_linear_seek(
                    &snapshot,
                    ScanMode::Backward,
                    SeekMode::Reverse,
                    start * SEEK_BOUND,
                    step,
                );
                test_linear_seek(
                    &snapshot,
                    ScanMode::Backward,
                    SeekMode::ForPrev,
                    start * SEEK_BOUND,
                    step,
                );
            }
        }
        for &seek_mode in &[SeekMode::Reverse, SeekMode::Normal, SeekMode::ForPrev] {
            for step in 1..SEEK_BOUND * 3 {
                for start in 0..10 {
                    test_linear_seek(
                        &snapshot,
                        ScanMode::Mixed,
                        seek_mode,
                        start * SEEK_BOUND,
                        step,
                    );
                }
            }
        }
    }

    fn test_cf<E: Engine>(engine: &E) {
        assert_none_cf(engine, "cf", b"key");
        must_put_cf(engine, "cf", b"key", b"value");
        assert_has_cf(engine, "cf", b"key", b"value");
        must_delete_cf(engine, "cf", b"key");
        assert_none_cf(engine, "cf", b"key");
    }

    fn test_empty_write<E: Engine>(engine: &E) {
        engine.write(&Context::new(), vec![]).unwrap_err();
    }

    fn test_empty_batch_snapshot<E: Engine>(engine: &E) {
        let on_finished = box move |_| {};
        engine
            .async_batch_snapshot(vec![], on_finished)
            .unwrap_err();
    }

    #[test]
    fn test_statistics() {
        let dir = TempDir::new("rocksdb_statistics_test").unwrap();
        let engine = new_local_engine(dir.path().to_str().unwrap(), TEST_ENGINE_CFS).unwrap();

        must_put(&engine, b"foo", b"bar1");
        must_put(&engine, b"foo2", b"bar2");
        must_put(&engine, b"foo3", b"bar3"); // deleted
        must_put(&engine, b"foo4", b"bar4");
        must_put(&engine, b"foo42", b"bar42"); // deleted
        must_put(&engine, b"foo5", b"bar5"); // deleted
        must_put(&engine, b"foo6", b"bar6");
        must_delete(&engine, b"foo3");
        must_delete(&engine, b"foo42");
        must_delete(&engine, b"foo5");

        let snapshot = engine.snapshot(&Context::new()).unwrap();
        let mut iter = snapshot
            .iter(IterOption::default(), ScanMode::Forward)
            .unwrap();

        let perf_statistics = PerfStatisticsInstant::new();
        let mut statistics = CFStatistics::default();
        iter.seek(&make_key(b"foo30"), &mut statistics).unwrap();

        assert_eq!(iter.key(&mut statistics), &*bytes::encode_bytes(b"foo4"));
        assert_eq!(iter.value(&mut statistics), b"bar4");
        assert_eq!(statistics.seek, 1);
        assert_eq!(perf_statistics.delta().internal_delete_skipped_count, 0);

        let perf_statistics = PerfStatisticsInstant::new();
        let mut statistics = CFStatistics::default();
        iter.near_seek(&make_key(b"foo55"), false, &mut statistics)
            .unwrap();

        assert_eq!(iter.key(&mut statistics), &*bytes::encode_bytes(b"foo6"));
        assert_eq!(iter.value(&mut statistics), b"bar6");
        assert_eq!(statistics.seek, 0);
        assert_eq!(statistics.next, 1);
        assert_eq!(perf_statistics.delta().internal_delete_skipped_count, 2);

        let perf_statistics = PerfStatisticsInstant::new();
        let mut statistics = CFStatistics::default();
        iter.prev(&mut statistics);

        assert_eq!(iter.key(&mut statistics), &*bytes::encode_bytes(b"foo4"));
        assert_eq!(iter.value(&mut statistics), b"bar4");
        assert_eq!(statistics.prev, 1);
        assert_eq!(perf_statistics.delta().internal_delete_skipped_count, 2);

        iter.prev(&mut statistics);
        assert_eq!(iter.key(&mut statistics), &*bytes::encode_bytes(b"foo2"));
        assert_eq!(iter.value(&mut statistics), b"bar2");
        assert_eq!(statistics.prev, 2);
        assert_eq!(perf_statistics.delta().internal_delete_skipped_count, 3);

        iter.prev(&mut statistics);
        assert_eq!(iter.key(&mut statistics), &*bytes::encode_bytes(b"foo"));
        assert_eq!(iter.value(&mut statistics), b"bar1");
        assert_eq!(statistics.prev, 3);
        assert_eq!(perf_statistics.delta().internal_delete_skipped_count, 3);
    }
}
