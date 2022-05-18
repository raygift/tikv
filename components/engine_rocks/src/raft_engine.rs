// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

// #[PerformanceCriticalPath]
use crate::{util, RocksEngine, RocksWriteBatch};

use engine_traits::{
    Error, Iterable, KvEngine, MiscExt, Mutable, Peekable, RaftEngine, RaftEngineReadOnly,
    RaftLogBatch, Result, SyncMutable, WriteBatch, WriteBatchExt, WriteOptions, CF_DEFAULT,
};
use kvproto::raft_serverpb::RaftLocalState;
use protobuf::Message;
use raft::eraftpb::Entry;
use tikv_util::{box_err, box_try};

const RAFT_LOG_MULTI_GET_CNT: u64 = 8;

impl RaftEngineReadOnly for RocksEngine {
    fn get_raft_state(&self, raft_group_id: u64) -> Result<Option<RaftLocalState>> {
        let key = keys::raft_state_key(raft_group_id);
        self.get_msg_cf(CF_DEFAULT, &key)
    }

    fn get_entry(&self, raft_group_id: u64, index: u64) -> Result<Option<Entry>> {
        let key = keys::raft_log_key(raft_group_id, index);
        self.get_msg_cf(CF_DEFAULT, &key)
    }

    fn fetch_entries_to(
        &self,
        region_id: u64,
        low: u64,
        high: u64,
        max_size: Option<usize>,
        buf: &mut Vec<Entry>,
    ) -> Result<usize> {
        let (max_size, mut total_size, mut count) = (max_size.unwrap_or(usize::MAX), 0, 0);

        if high - low <= RAFT_LOG_MULTI_GET_CNT {
            // If election happens in inactive regions, they will just try to fetch one empty log.
            for i in low..high {
                if total_size > 0 && total_size >= max_size {
                    break;
                }
                let key = keys::raft_log_key(region_id, i);
                match self.get_value(&key) {
                    Ok(None) => return Err(Error::EntriesCompacted),
                    Ok(Some(v)) => {
                        let mut entry = Entry::default();
                        entry.merge_from_bytes(&v)?;
                        assert_eq!(entry.get_index(), i);
                        buf.push(entry);
                        total_size += v.len();
                        count += 1;
                    }
                    Err(e) => return Err(box_err!(e)),
                }
            }
            return Ok(count);
        }

        let (mut check_compacted, mut next_index) = (true, low);
        let start_key = keys::raft_log_key(region_id, low);
        let end_key = keys::raft_log_key(region_id, high);
        self.scan(
            &start_key,
            &end_key,
            true, // fill_cache
            |_, value| {
                let mut entry = Entry::default();
                entry.merge_from_bytes(value)?;

                if check_compacted {
                    if entry.get_index() != low {
                        // May meet gap or has been compacted.
                        return Ok(false);
                    }
                    check_compacted = false;
                } else {
                    assert_eq!(entry.get_index(), next_index);
                }
                next_index += 1;

                buf.push(entry);
                total_size += value.len();
                count += 1;
                Ok(total_size < max_size)
            },
        )?;

        // If we get the correct number of entries, returns.
        // Or the total size almost exceeds max_size, returns.
        if count == (high - low) as usize || total_size >= max_size {
            return Ok(count);
        }

        // Here means we don't fetch enough entries.
        Err(Error::EntriesUnavailable)
    }
}

// FIXME: RaftEngine should probably be implemented generically
// for all KvEngines, but is currently implemented separately for
// every engine.
// RocksEngine 结构体对接口 RaftEngine 的实现
impl RaftEngine for RocksEngine {
    type LogBatch = RocksWriteBatch;

    fn log_batch(&self, capacity: usize) -> Self::LogBatch {
        RocksWriteBatch::with_capacity(self.as_inner().clone(), capacity)
    }

    fn sync(&self) -> Result<()> {
        self.sync_wal()
    }

    fn consume(&self, batch: &mut Self::LogBatch, sync_log: bool) -> Result<usize> {
        let bytes = batch.data_size();
        let mut opts = WriteOptions::default();
        opts.set_sync(sync_log);
        batch.write_opt(&opts)?;
        batch.clear();
        Ok(bytes)
    }

    fn consume_and_shrink(
        &self,
        batch: &mut Self::LogBatch,
        sync_log: bool,
        max_capacity: usize,
        shrink_to: usize,
    ) -> Result<usize> {
        let data_size = self.consume(batch, sync_log)?;
        if data_size > max_capacity {
            *batch = self.write_batch_with_cap(shrink_to);
        }
        Ok(data_size)
    }

    fn clean(
        &self,
        raft_group_id: u64,
        state: &RaftLocalState,
        batch: &mut Self::LogBatch,
    ) -> Result<()> {
        batch.delete(&keys::raft_state_key(raft_group_id))?;
        let seek_key = keys::raft_log_key(raft_group_id, 0);
        let prefix = keys::raft_log_prefix(raft_group_id);
        if let Some((key, _)) = self.seek(&seek_key)? {
            if !key.starts_with(&prefix) {
                // No raft logs for the raft group.
                return Ok(());
            }
            let first_index = match keys::raft_log_index(&key) {
                Ok(index) => index,
                Err(_) => return Ok(()),
            };
            for index in first_index..=state.last_index {
                let key = keys::raft_log_key(raft_group_id, index);
                batch.delete(&key)?;
            }
        }
        Ok(())
    }

    fn append(&self, raft_group_id: u64, entries: Vec<Entry>) -> Result<usize> {
        let mut wb = RocksWriteBatch::new(self.as_inner().clone());// 创建 self.db 的副本
        let buf = Vec::with_capacity(1024);
        wb.append_impl(raft_group_id, &entries, buf)?;// 调用 RocksEngine 的 append_impl 方法
        self.consume(&mut wb, false)
    }

    fn put_raft_state(&self, raft_group_id: u64, state: &RaftLocalState) -> Result<()> {
        self.put_msg(&keys::raft_state_key(raft_group_id), state)
    }

    fn gc(&self, raft_group_id: u64, mut from: u64, to: u64) -> Result<usize> {
        if from >= to {
            return Ok(0);
        }
        if from == 0 {
            let start_key = keys::raft_log_key(raft_group_id, 0);
            let prefix = keys::raft_log_prefix(raft_group_id);
            match self.seek(&start_key)? {
                Some((k, _)) if k.starts_with(&prefix) => from = box_try!(keys::raft_log_index(&k)),
                // No need to gc.
                _ => return Ok(0),
            }
        }

        let mut raft_wb = self.write_batch_with_cap(4 * 1024);
        for idx in from..to {
            let key = keys::raft_log_key(raft_group_id, idx);
            raft_wb.delete(&key)?;
            if raft_wb.count() >= Self::WRITE_BATCH_MAX_KEYS {
                raft_wb.write()?;
                raft_wb.clear();
            }
        }

        // TODO: disable WAL here.
        if !WriteBatch::is_empty(&raft_wb) {
            raft_wb.write()?;
        }
        Ok((to - from) as usize)
    }

    fn purge_expired_files(&self) -> Result<Vec<u64>> {
        Ok(vec![])
    }

    fn has_builtin_entry_cache(&self) -> bool {
        false
    }

    fn flush_metrics(&self, instance: &str) {
        KvEngine::flush_metrics(self, instance)
    }

    fn reset_statistics(&self) {
        KvEngine::reset_statistics(self)
    }

    fn dump_stats(&self) -> Result<String> {
        MiscExt::dump_stats(self)
    }

    fn get_engine_size(&self) -> Result<u64> {
        let handle = util::get_cf_handle(self.as_inner(), CF_DEFAULT)?;
        let used_size = util::get_engine_cf_used_size(self.as_inner(), handle);

        Ok(used_size)
    }
}

impl RaftLogBatch for RocksWriteBatch {
    fn append(&mut self, raft_group_id: u64, entries: Vec<Entry>) -> Result<()> {
        if let Some(max_size) = entries.iter().map(|e| e.compute_size()).max() {// 获取 entries 中 计算compute_size()得到的最大值
            let ser_buf = Vec::with_capacity(max_size as usize);// 创建一个大小为最大值的 Vector，作为buffer
            return self.append_impl(raft_group_id, &entries, ser_buf);// 创建key，将entries 作为value，调用rocksdb 的put 方法将key:value 写入rocksdb 完成持久化
        }
        Ok(())
    }

    // 将从 from ~ to 的日志对应的key/value 从 rocks 中删除
    fn cut_logs(&mut self, raft_group_id: u64, from: u64, to: u64) {
        for index in from..to {
            let key = keys::raft_log_key(raft_group_id, index);
            self.delete(&key).unwrap();
        }
    }

    fn put_raft_state(&mut self, raft_group_id: u64, state: &RaftLocalState) -> Result<()> {
        self.put_msg(&keys::raft_state_key(raft_group_id), state)
    }

    fn persist_size(&self) -> usize {
        self.data_size()
    }

    fn is_empty(&self) -> bool {
        WriteBatch::is_empty(self)
    }

    fn merge(&mut self, src: Self) {
        WriteBatch::<RocksEngine>::merge(self, src);
    }
}

impl RocksWriteBatch {
    fn append_impl(
        &mut self,
        raft_group_id: u64,
        entries: &[Entry],
        mut ser_buf: Vec<u8>,
    ) -> Result<()> {
        for entry in entries {// 针对 entries 中每个 entry
            let key = keys::raft_log_key(raft_group_id, entry.get_index());// 为每条待处理的日志生成 key，格式为 raft_log_key 格式：0x01 0x02 region_id 0x01 log_idx
            ser_buf.clear();// 清空buffer
            entry.write_to_vec(&mut ser_buf).unwrap();// 将entries 写入 buffer
            self.put(&key, &ser_buf)?;// 调用 rocksdb 的put 操作，将 key:value 写入 RocksDB
        }
        Ok(())
    }
}
