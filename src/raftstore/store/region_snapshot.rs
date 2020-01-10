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

use kvproto::metapb::Region;
use rocksdb::{DBIterator, DBVector, TablePropertiesCollection, DB};
use std::cmp;
use std::sync::Arc;

use raftstore::store::engine::{IterOption, Peekable, Snapshot, SyncSnapshot};
use raftstore::store::{keys, util, PeerStorage};
use raftstore::{Error, Result};
use storage::engine::Iterator;
use util::metrics::CRITICAL_ERROR;
use util::{panic_when_unexpected_key_or_data, set_panic_mark};

/// Snapshot of a region.
///
/// Only data within a region can be accessed.
#[derive(Debug)]
pub struct RegionSnapshot {
    snap: SyncSnapshot,
    region: Arc<Region>,
}

impl RegionSnapshot {
    pub fn new(ps: &PeerStorage) -> RegionSnapshot {
        RegionSnapshot::from_snapshot(ps.raw_snapshot().into_sync(), ps.region().clone())
    }

    pub fn from_raw(db: Arc<DB>, region: Region) -> RegionSnapshot {
        RegionSnapshot::from_snapshot(Snapshot::new(db).into_sync(), region)
    }

    pub fn from_snapshot(snap: SyncSnapshot, region: Region) -> RegionSnapshot {
        RegionSnapshot {
            snap,
            region: Arc::new(region),
        }
    }

    pub fn get_region(&self) -> &Region {
        &self.region
    }

    pub fn iter(&self, iter_opt: IterOption) -> RegionIterator {
        RegionIterator::new(&self.snap, Arc::clone(&self.region), iter_opt)
    }

    pub fn iter_cf(&self, cf: &str, iter_opt: IterOption) -> Result<RegionIterator> {
        Ok(RegionIterator::new_cf(
            &self.snap,
            Arc::clone(&self.region),
            iter_opt,
            cf,
        ))
    }

    // scan scans database using an iterator in range [start_key, end_key), calls function f for
    // each iteration, if f returns false, terminates this scan.
    pub fn scan<F>(&self, start_key: &[u8], end_key: &[u8], fill_cache: bool, f: F) -> Result<()>
    where
        F: FnMut(&[u8], &[u8]) -> Result<bool>,
    {
        let iter_opt =
            IterOption::new(Some(start_key.to_vec()), Some(end_key.to_vec()), fill_cache);
        self.scan_impl(self.iter(iter_opt), start_key, f)
    }

    // like `scan`, only on a specific column family.
    pub fn scan_cf<F>(
        &self,
        cf: &str,
        start_key: &[u8],
        end_key: &[u8],
        fill_cache: bool,
        f: F,
    ) -> Result<()>
    where
        F: FnMut(&[u8], &[u8]) -> Result<bool>,
    {
        let iter_opt =
            IterOption::new(Some(start_key.to_vec()), Some(end_key.to_vec()), fill_cache);
        self.scan_impl(self.iter_cf(cf, iter_opt)?, start_key, f)
    }

    fn scan_impl<F>(&self, mut it: RegionIterator, start_key: &[u8], mut f: F) -> Result<()>
    where
        F: FnMut(&[u8], &[u8]) -> Result<bool>,
    {
        let mut it_valid = it.seek(start_key)?;
        while it_valid {
            it_valid = f(it.key(), it.value())? && it.next()?;
        }
        Ok(())
    }

    pub fn get_properties_cf(&self, cf: &str) -> Result<TablePropertiesCollection> {
        util::get_region_properties_cf(&self.snap.get_db(), cf, self.get_region())
    }

    pub fn get_start_key(&self) -> &[u8] {
        self.region.get_start_key()
    }

    pub fn get_end_key(&self) -> &[u8] {
        self.region.get_end_key()
    }
}

impl Clone for RegionSnapshot {
    fn clone(&self) -> Self {
        RegionSnapshot {
            snap: self.snap.clone(),
            region: Arc::clone(&self.region),
        }
    }
}

impl Peekable for RegionSnapshot {
    fn get_value(&self, key: &[u8]) -> Result<Option<DBVector>> {
        util::check_key_in_region(key, &self.region)?;
        let data_key = keys::data_key(key);
        self.snap.get_value(&data_key)
    }

    fn get_value_cf(&self, cf: &str, key: &[u8]) -> Result<Option<DBVector>> {
        util::check_key_in_region(key, &self.region)?;
        let data_key = keys::data_key(key);
        self.snap.get_value_cf(cf, &data_key)
    }
}

/// `RegionIterator` wrap a rocksdb iterator and only allow it to
/// iterate in the region. It behaves as if underlying
/// db only contains one region.
pub struct RegionIterator {
    iter: DBIterator<Arc<DB>>,
    region: Arc<Region>,
}

fn set_lower_bound(iter_opt: &mut IterOption, region: &Region) {
    let region_start_key = keys::enc_start_key(region);
    let lower_bound = match iter_opt.lower_bound() {
        Some(k) if !k.is_empty() => {
            let k = keys::data_key(k);
            cmp::max(k, region_start_key)
        }
        _ => region_start_key,
    };
    iter_opt.set_lower_bound(lower_bound);
}

fn set_upper_bound(iter_opt: &mut IterOption, region: &Region) {
    let region_end_key = keys::enc_end_key(region);
    let upper_bound = match iter_opt.upper_bound() {
        Some(k) if !k.is_empty() => {
            let k = keys::data_key(k);
            cmp::min(k, region_end_key)
        }
        _ => region_end_key,
    };
    iter_opt.set_upper_bound(upper_bound);
}

// we use rocksdb's style iterator, doesn't need to impl std iterator.
impl RegionIterator {
    pub fn new(snap: &Snapshot, region: Arc<Region>, mut iter_opt: IterOption) -> RegionIterator {
        set_lower_bound(&mut iter_opt, &region);
        set_upper_bound(&mut iter_opt, &region);
        let iter = snap.db_iterator(iter_opt);
        RegionIterator { iter, region }
    }

    pub fn new_cf(
        snap: &Snapshot,
        region: Arc<Region>,
        mut iter_opt: IterOption,
        cf: &str,
    ) -> RegionIterator {
        set_lower_bound(&mut iter_opt, &region);
        set_upper_bound(&mut iter_opt, &region);
        let iter = snap.db_iterator_cf(cf, iter_opt).unwrap();
        RegionIterator { iter, region }
    }

    pub fn seek_to_first(&mut self) -> Result<bool> {
        self.iter.seek_to_first().map_err(|e| box_err!(e))
    }

    pub fn seek_to_last(&mut self) -> Result<bool> {
        self.iter.seek_to_last().map_err(|e| box_err!(e))
    }

    pub fn seek(&mut self, key: &[u8]) -> Result<bool> {
        self.should_seekable(key)?;
        let key = keys::data_key(key);
        self.iter
            .seek(key.as_slice().into())
            .map_err(|e| box_err!(e))
    }

    pub fn seek_for_prev(&mut self, key: &[u8]) -> Result<bool> {
        self.should_seekable(key)?;
        let key = keys::data_key(key);
        self.iter
            .seek_for_prev(key.as_slice().into())
            .map_err(|e| box_err!(e))
    }

    pub fn prev(&mut self) -> Result<bool> {
        self.iter.prev().map_err(|e| box_err!(e))
    }

    pub fn next(&mut self) -> Result<bool> {
        self.iter.next().map_err(|e| box_err!(e))
    }

    #[inline]
    pub fn key(&self) -> &[u8] {
        keys::origin_key(self.iter.key())
    }

    #[inline]
    pub fn value(&self) -> &[u8] {
        self.iter.value()
    }

    #[inline]
    pub fn valid(&self) -> Result<bool> {
        self.iter.valid().map_err(|e| box_err!(e))
    }

    #[inline]
    pub fn should_seekable(&self, key: &[u8]) -> Result<()> {
        if let Err(e) = util::check_key_in_region_inclusive(key, &self.region) {
            return handle_check_key_in_region_error(e);
        }
        Ok(())
    }
}

#[inline(never)]
fn handle_check_key_in_region_error(e: Error) -> Result<()> {
    // Split out the error case to reduce hot-path code size.
    CRITICAL_ERROR
        .with_label_values(&["key not in region"])
        .inc();
    if panic_when_unexpected_key_or_data() {
        set_panic_mark();
        panic!("key exceed bound: {:?}", e);
    } else {
        Err(e)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::Path;
    use std::rc::Rc;
    use std::sync::Arc;

    use kvproto::metapb::{Peer, Region};
    use rocksdb::Writable;
    use tempdir::TempDir;

    use raftstore::store::engine::*;
    use raftstore::store::keys::*;
    use raftstore::store::{CacheQueryStats, Engines, PeerStorage};
    use raftstore::Result;
    use storage::{CFStatistics, Cursor, Key, ScanMode, ALL_CFS, CF_DEFAULT};
    use util::{escape, rocksdb, worker};

    use super::*;

    type DataSet = Vec<(Vec<u8>, Vec<u8>)>;

    fn new_temp_engine(path: &TempDir) -> Engines {
        let raft_path = path.path().join(Path::new("raft"));
        Engines::new(
            Arc::new(rocksdb::new_engine(path.path().to_str().unwrap(), ALL_CFS, None).unwrap()),
            Arc::new(
                rocksdb::new_engine(raft_path.to_str().unwrap(), &[CF_DEFAULT], None).unwrap(),
            ),
        )
    }

    fn new_peer_storage(engines: Engines, r: &Region) -> PeerStorage {
        let metrics = Rc::new(RefCell::new(CacheQueryStats::default()));
        PeerStorage::new(
            engines,
            r,
            worker::dummy_scheduler(),
            "".to_owned(),
            metrics,
        ).unwrap()
    }

    fn load_default_dataset(engines: Engines) -> (PeerStorage, DataSet) {
        let mut r = Region::new();
        r.mut_peers().push(Peer::new());
        r.set_id(10);
        r.set_start_key(b"a2".to_vec());
        r.set_end_key(b"a7".to_vec());

        let base_data = vec![
            (b"a1".to_vec(), b"v1".to_vec()),
            (b"a3".to_vec(), b"v3".to_vec()),
            (b"a5".to_vec(), b"v5".to_vec()),
            (b"a7".to_vec(), b"v7".to_vec()),
        ];

        for &(ref k, ref v) in &base_data {
            engines.kv.put(&data_key(k), v).expect("");
        }
        let store = new_peer_storage(engines, &r);
        (store, base_data)
    }

    #[test]
    fn test_peekable() {
        let path = TempDir::new("test-raftstore").unwrap();
        let engines = new_temp_engine(&path);
        let mut r = Region::new();
        r.set_id(10);
        r.set_start_key(b"key0".to_vec());
        r.set_end_key(b"key4".to_vec());
        let store = new_peer_storage(engines.clone(), &r);

        let (key1, value1) = (b"key1", 2u64);
        engines.kv.put_u64(&data_key(key1), value1).expect("");
        let (key2, value2) = (b"key2", 2i64);
        engines.kv.put_i64(&data_key(key2), value2).expect("");
        let key3 = b"key3";
        engines.kv.put_msg(&data_key(key3), &r).expect("");

        let snap = RegionSnapshot::new(&store);
        let v1 = snap.get_u64(key1).expect("");
        assert_eq!(v1, Some(value1));
        let v2 = snap.get_i64(key2).expect("");
        assert_eq!(v2, Some(value2));
        let v3 = snap.get_msg(key3).expect("");
        assert_eq!(v3, Some(r));

        let v0 = snap.get_value(b"key0").expect("");
        assert!(v0.is_none());

        let v4 = snap.get_value(b"key5");
        assert!(v4.is_err());
    }

    #[cfg_attr(feature = "cargo-clippy", allow(type_complexity))]
    #[test]
    fn test_iterate() {
        let path = TempDir::new("test-raftstore").unwrap();
        let engines = new_temp_engine(&path);
        let (store, base_data) = load_default_dataset(engines.clone());

        let snap = RegionSnapshot::new(&store);
        let mut data = vec![];
        snap.scan(b"a2", &[0xFF, 0xFF], false, |key, value| {
            data.push((key.to_vec(), value.to_vec()));
            Ok(true)
        }).unwrap();

        assert_eq!(data.len(), 2);
        assert_eq!(data, &base_data[1..3]);

        let seek_table: Vec<(_, _, Option<(&[u8], &[u8])>, Option<(&[u8], &[u8])>)> = vec![
            (b"a1", false, None, None),
            (b"a2", true, Some((b"a3", b"v3")), None),
            (b"a3", true, Some((b"a3", b"v3")), Some((b"a3", b"v3"))),
            (b"a4", true, Some((b"a5", b"v5")), Some((b"a3", b"v3"))),
            (b"a6", true, None, Some((b"a5", b"v5"))),
            (b"a7", true, None, Some((b"a5", b"v5"))),
            (b"a8", false, None, None),
        ];
        let upper_bounds: Vec<Option<&[u8]>> = vec![None, Some(b"a7")];
        for upper_bound in upper_bounds {
            let iter_opt = IterOption::new(None, upper_bound.map(|v| v.to_vec()), true);
            let mut iter = snap.iter(iter_opt);
            for (seek_key, in_range, seek_exp, prev_exp) in seek_table.clone() {
                let check_res =
                    |iter: &RegionIterator, res: Result<bool>, exp: Option<(&[u8], &[u8])>| {
                        if !in_range {
                            assert!(res.is_err(), "exp failed at {}", escape(seek_key));
                            return;
                        }
                        if exp.is_none() {
                            assert!(!res.unwrap(), "exp none at {}", escape(seek_key));
                            return;
                        }

                        assert!(res.unwrap(), "should succeed at {}", escape(seek_key));
                        let (exp_key, exp_val) = exp.unwrap();
                        assert_eq!(iter.key(), exp_key);
                        assert_eq!(iter.value(), exp_val);
                    };
                let seek_res = iter.seek(seek_key);
                check_res(&iter, seek_res, seek_exp);
                let prev_res = iter.seek_for_prev(seek_key);
                check_res(&iter, prev_res, prev_exp);
            }
        }

        data.clear();
        snap.scan(b"a2", &[0xFF, 0xFF], false, |key, value| {
            data.push((key.to_vec(), value.to_vec()));
            Ok(false)
        }).unwrap();

        assert_eq!(data.len(), 1);

        let mut iter = snap.iter(IterOption::default());
        assert!(iter.seek_to_first().unwrap());
        let mut res = vec![];
        loop {
            res.push((iter.key().to_vec(), iter.value().to_vec()));
            if !iter.next().unwrap() {
                break;
            }
        }
        assert_eq!(res, base_data[1..3].to_vec());

        // test last region
        let mut region = Region::new();
        region.mut_peers().push(Peer::new());
        let store = new_peer_storage(engines.clone(), &region);
        let snap = RegionSnapshot::new(&store);
        data.clear();
        snap.scan(b"", &[0xFF, 0xFF], false, |key, value| {
            data.push((key.to_vec(), value.to_vec()));
            Ok(true)
        }).unwrap();

        assert_eq!(data.len(), 4);
        assert_eq!(data, base_data);

        let mut iter = snap.iter(IterOption::default());
        assert!(iter.seek(b"a1").unwrap());

        assert!(iter.seek_to_first().unwrap());
        let mut res = vec![];
        loop {
            res.push((iter.key().to_vec(), iter.value().to_vec()));
            if !iter.next().unwrap() {
                break;
            }
        }
        assert_eq!(res, base_data);

        // test iterator with upper bound
        let store = new_peer_storage(engines, &region);
        let snap = RegionSnapshot::new(&store);
        let mut iter = snap.iter(IterOption::new(None, Some(b"a5".to_vec()), true));
        assert!(iter.seek_to_first().unwrap());
        let mut res = vec![];
        loop {
            res.push((iter.key().to_vec(), iter.value().to_vec()));
            if !iter.next().unwrap() {
                break;
            }
        }
        assert_eq!(res, base_data[0..2].to_vec());
    }

    #[test]
    fn test_reverse_iterate() {
        let path = TempDir::new("test-raftstore").unwrap();
        let engines = new_temp_engine(&path);
        let (store, test_data) = load_default_dataset(engines.clone());

        let snap = RegionSnapshot::new(&store);
        let mut statistics = CFStatistics::default();
        let it = snap.iter(IterOption::default());
        let mut iter = Cursor::new(it, ScanMode::Mixed);
        assert!(
            !iter
                .reverse_seek(&Key::from_encoded_slice(b"a2"), &mut statistics)
                .unwrap()
        );
        assert!(
            iter.reverse_seek(&Key::from_encoded_slice(b"a7"), &mut statistics)
                .unwrap()
        );
        let mut pair = (
            iter.key(&mut statistics).to_vec(),
            iter.value(&mut statistics).to_vec(),
        );
        assert_eq!(pair, (b"a5".to_vec(), b"v5".to_vec()));
        assert!(
            iter.reverse_seek(&Key::from_encoded_slice(b"a5"), &mut statistics)
                .unwrap()
        );
        pair = (
            iter.key(&mut statistics).to_vec(),
            iter.value(&mut statistics).to_vec(),
        );
        assert_eq!(pair, (b"a3".to_vec(), b"v3".to_vec()));
        assert!(
            !iter
                .reverse_seek(&Key::from_encoded_slice(b"a3"), &mut statistics)
                .unwrap()
        );
        assert!(
            iter.reverse_seek(&Key::from_encoded_slice(b"a1"), &mut statistics)
                .is_err()
        );
        assert!(
            iter.reverse_seek(&Key::from_encoded_slice(b"a8"), &mut statistics)
                .is_err()
        );

        assert!(iter.seek_to_last(&mut statistics).unwrap());
        let mut res = vec![];
        loop {
            res.push((
                iter.key(&mut statistics).to_vec(),
                iter.value(&mut statistics).to_vec(),
            ));
            if !iter.prev(&mut statistics).unwrap() {
                break;
            }
        }
        let mut expect = test_data[1..3].to_vec();
        expect.reverse();
        assert_eq!(res, expect);

        // test last region
        let mut region = Region::new();
        region.mut_peers().push(Peer::new());
        let store = new_peer_storage(engines, &region);
        let snap = RegionSnapshot::new(&store);
        let it = snap.iter(IterOption::default());
        let mut iter = Cursor::new(it, ScanMode::Mixed);
        assert!(
            !iter
                .reverse_seek(&Key::from_encoded_slice(b"a1"), &mut statistics)
                .unwrap()
        );
        assert!(
            iter.reverse_seek(&Key::from_encoded_slice(b"a2"), &mut statistics)
                .unwrap()
        );
        let pair = (
            iter.key(&mut statistics).to_vec(),
            iter.value(&mut statistics).to_vec(),
        );
        assert_eq!(pair, (b"a1".to_vec(), b"v1".to_vec()));
        for kv_pairs in test_data.windows(2) {
            let seek_key = Key::from_encoded(kv_pairs[1].0.clone());
            assert!(
                iter.reverse_seek(&seek_key, &mut statistics).unwrap(),
                "{}",
                seek_key
            );
            let pair = (
                iter.key(&mut statistics).to_vec(),
                iter.value(&mut statistics).to_vec(),
            );
            assert_eq!(pair, kv_pairs[0]);
        }

        assert!(iter.seek_to_last(&mut statistics).unwrap());
        let mut res = vec![];
        loop {
            res.push((
                iter.key(&mut statistics).to_vec(),
                iter.value(&mut statistics).to_vec(),
            ));
            if !iter.prev(&mut statistics).unwrap() {
                break;
            }
        }
        let mut expect = test_data.clone();
        expect.reverse();
        assert_eq!(res, expect);
    }

    #[test]
    fn test_reverse_iterate_with_lower_bound() {
        let path = TempDir::new("test-raftstore").unwrap();
        let engines = new_temp_engine(&path);
        let (store, test_data) = load_default_dataset(engines);

        let snap = RegionSnapshot::new(&store);
        let mut iter_opt = IterOption::default();
        iter_opt.set_lower_bound(b"a3".to_vec());
        let mut iter = snap.iter(iter_opt);
        assert!(iter.seek_to_last().unwrap());
        let mut res = vec![];
        loop {
            res.push((iter.key().to_vec(), iter.value().to_vec()));
            if !iter.prev().unwrap() {
                break;
            }
        }
        res.sort();
        assert_eq!(res, test_data[1..3].to_vec());
    }
}
