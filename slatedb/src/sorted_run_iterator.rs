use crate::bytes_range::BytesRange;
use crate::db_state::{SortedRun, SsTableHandle};
use crate::error::SlateDBError;
use crate::iter::{IterationOrder, KeyValueIterator};
use crate::sst_iter::{SstIterator, SstIteratorOptions, SstView};
use crate::tablestore::TableStore;
use crate::types::RowEntry;
use async_trait::async_trait;
use bytes::Bytes;
use std::collections::VecDeque;
use std::ops::{Bound, RangeBounds};
use std::sync::Arc;

#[derive(Debug)]
enum SortedRunView<'a> {
    Owned(VecDeque<SsTableHandle>, BytesRange),
    Borrowed(
        VecDeque<&'a SsTableHandle>,
        (Bound<&'a [u8]>, Bound<&'a [u8]>),
    ),
}

impl<'a> SortedRunView<'a> {
    fn pop_sst(&mut self, order: IterationOrder) -> Option<SstView<'a>> {
        match self {
            SortedRunView::Owned(tables, r) => {
                let table = match order {
                    IterationOrder::Ascending => tables.pop_front(),
                    IterationOrder::Descending => tables.pop_back(),
                };
                table.map(|table| SstView::Owned(Box::new(table), r.clone()))
            }
            SortedRunView::Borrowed(tables, r) => {
                let table = match order {
                    IterationOrder::Ascending => tables.pop_front(),
                    IterationOrder::Descending => tables.pop_back(),
                };
                table.map(|table| SstView::Borrowed(table, BytesRange::from_slice(*r)))
            }
        }
    }

    pub(crate) async fn build_next_iter(
        &mut self,
        table_store: Arc<TableStore>,
        sst_iterator_options: SstIteratorOptions,
    ) -> Result<Option<SstIterator<'a>>, SlateDBError> {
        let order = sst_iterator_options.order;
        let next_iter = if let Some(view) = self.pop_sst(order) {
            Some(SstIterator::new(view, table_store, sst_iterator_options)?)
        } else {
            None
        };
        Ok(next_iter)
    }

    fn peek_next_table(&self, order: IterationOrder) -> Option<&SsTableHandle> {
        match self {
            SortedRunView::Owned(tables, _) => match order {
                IterationOrder::Ascending => tables.front(),
                IterationOrder::Descending => tables.back(),
            },
            SortedRunView::Borrowed(tables, _) => match order {
                IterationOrder::Ascending => tables.front().copied(),
                IterationOrder::Descending => tables.back().copied(),
            },
        }
    }
}

pub(crate) struct SortedRunIterator<'a> {
    table_store: Arc<TableStore>,
    sst_iter_options: SstIteratorOptions,
    view: SortedRunView<'a>,
    current_iter: Option<SstIterator<'a>>,
    initialized: bool,
}

impl<'a> SortedRunIterator<'a> {
    async fn new(
        view: SortedRunView<'a>,
        table_store: Arc<TableStore>,
        sst_iter_options: SstIteratorOptions,
    ) -> Result<Self, SlateDBError> {
        let mut res = Self {
            table_store,
            sst_iter_options,
            view,
            current_iter: None,
            initialized: false,
        };
        res.advance_table().await?;
        Ok(res)
    }

    pub(crate) async fn new_owned<T: RangeBounds<Bytes>>(
        range: T,
        sorted_run: SortedRun,
        table_store: Arc<TableStore>,
        sst_iter_options: SstIteratorOptions,
    ) -> Result<Self, SlateDBError> {
        let range = BytesRange::from(range);
        let tables = sorted_run.into_tables_covering_range(&range);
        let view = SortedRunView::Owned(tables, range);
        SortedRunIterator::new(view, table_store, sst_iter_options).await
    }

    pub(crate) async fn new_owned_initialized<T: RangeBounds<Bytes>>(
        range: T,
        sorted_run: SortedRun,
        table_store: Arc<TableStore>,
        sst_iter_options: SstIteratorOptions,
    ) -> Result<Self, SlateDBError> {
        let mut iter =
            SortedRunIterator::new_owned(range, sorted_run, table_store, sst_iter_options).await?;
        iter.init().await?;
        Ok(iter)
    }

    pub(crate) async fn new_borrowed<T: RangeBounds<&'a [u8]>>(
        range: T,
        sorted_run: &'a SortedRun,
        table_store: Arc<TableStore>,
        sst_iter_options: SstIteratorOptions,
    ) -> Result<Self, SlateDBError> {
        let range = (range.start_bound().cloned(), range.end_bound().cloned());
        // todo remove conversion to bytesrange
        let tables = sorted_run.tables_covering_range(&BytesRange::from_slice(range));
        let view = SortedRunView::Borrowed(tables, range);
        SortedRunIterator::new(view, table_store, sst_iter_options).await
    }

    #[cfg(test)]
    pub(crate) async fn new_borrowed_initialized<T: RangeBounds<&'a [u8]>>(
        range: T,
        sorted_run: &'a SortedRun,
        table_store: Arc<TableStore>,
        sst_iter_options: SstIteratorOptions,
    ) -> Result<Self, SlateDBError> {
        let mut iter =
            SortedRunIterator::new_borrowed(range, sorted_run, table_store, sst_iter_options)
                .await?;
        iter.init().await?;
        Ok(iter)
    }

    async fn advance_table(&mut self) -> Result<(), SlateDBError> {
        self.current_iter = self
            .view
            .build_next_iter(self.table_store.clone(), self.sst_iter_options)
            .await?;
        if self.initialized {
            if let Some(iter) = self.current_iter.as_mut() {
                iter.init().await?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl KeyValueIterator for SortedRunIterator<'_> {
    async fn init(&mut self) -> Result<(), SlateDBError> {
        if !self.initialized {
            if let Some(iter) = self.current_iter.as_mut() {
                iter.init().await?;
            }
            self.initialized = true;
        }
        Ok(())
    }

    async fn next_entry(&mut self) -> Result<Option<RowEntry>, SlateDBError> {
        if !self.initialized {
            return Err(SlateDBError::IteratorNotInitialized);
        }
        while let Some(iter) = &mut self.current_iter {
            if let Some(kv) = iter.next_entry().await? {
                return Ok(Some(kv));
            } else {
                self.advance_table().await?;
            }
        }
        Ok(None)
    }

    async fn seek(&mut self, next_key: &[u8]) -> Result<(), SlateDBError> {
        if !self.initialized {
            return Err(SlateDBError::IteratorNotInitialized);
        }
        let order = self.sst_iter_options.order;

        // The skip logic differs between ascending and descending orders:
        //
        // For ascending order:
        //   - We check the NEXT table's start key
        //   - Skip (advance) if next_table.start < seek_key
        //   - This works because if next.start < seek_key, then current.end < next.start < seek_key,
        //     so the seek key cannot be in the current table
        //
        // For descending order:
        //   - We check the CURRENT table's start key
        //   - Skip (advance) if current_table.start > seek_key
        //   - This is because we're looking for keys <= seek_key, and if current.start > seek_key,
        //     then all keys in current are > seek_key, so we need to move to an earlier table
        loop {
            let should_skip = match order {
                IterationOrder::Ascending => {
                    if let Some(next_table) = self.view.peek_next_table(order) {
                        next_table.compacted_effective_start_key().as_ref() < next_key
                    } else {
                        false
                    }
                }
                IterationOrder::Descending => {
                    if let Some(current_iter) = &self.current_iter {
                        // For descending: skip if current table's start key > seek_key
                        current_iter.sst_start_key().as_ref() > next_key
                            && self.view.peek_next_table(order).is_some()
                    } else {
                        false
                    }
                }
            };

            if should_skip {
                self.advance_table().await?;
            } else {
                break;
            }
        }

        if let Some(iter) = &mut self.current_iter {
            iter.seek(next_key).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytes_generator::OrderedBytesGenerator;
    use crate::db_state::SsTableId;
    use crate::format::sst::SsTableFormat;
    use crate::proptest_util;
    use crate::proptest_util::sample;
    use crate::test_utils::{assert_kv, gen_attrs};

    use crate::object_stores::ObjectStores;
    use bytes::{BufMut, BytesMut};
    use object_store::path::Path;
    use object_store::{memory::InMemory, ObjectStore};
    use proptest::test_runner::TestRng;
    use rand::distr::uniform::SampleRange;
    use rand::Rng;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_one_sst_sr_iter() {
        let root_path = Path::from("");
        let object_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let format = SsTableFormat {
            min_filter_keys: 3,
            ..SsTableFormat::default()
        };
        let table_store = Arc::new(TableStore::new(
            ObjectStores::new(object_store, None),
            format,
            root_path.clone(),
            None,
        ));
        let mut builder = table_store.table_builder();
        builder
            .add_value(b"key1", b"value1", gen_attrs(1))
            .await
            .unwrap();
        builder
            .add_value(b"key2", b"value2", gen_attrs(2))
            .await
            .unwrap();
        builder
            .add_value(b"key3", b"value3", gen_attrs(3))
            .await
            .unwrap();
        let encoded = builder.build().await.unwrap();
        let id = SsTableId::Compacted(ulid::Ulid::new());
        let handle = table_store.write_sst(&id, encoded, false).await.unwrap();
        let sr = SortedRun {
            id: 0,
            ssts: vec![handle],
        };

        let mut iter = SortedRunIterator::new_owned_initialized(
            ..,
            sr,
            table_store,
            SstIteratorOptions::default(),
        )
        .await
        .unwrap();

        let kv = iter.next().await.unwrap().unwrap();
        assert_eq!(kv.key, b"key1".as_slice());
        assert_eq!(kv.value, b"value1".as_slice());
        let kv = iter.next().await.unwrap().unwrap();
        assert_eq!(kv.key, b"key2".as_slice());
        assert_eq!(kv.value, b"value2".as_slice());
        let kv = iter.next().await.unwrap().unwrap();
        assert_eq!(kv.key, b"key3".as_slice());
        assert_eq!(kv.value, b"value3".as_slice());
        let kv = iter.next().await.unwrap();
        assert!(kv.is_none());
    }

    #[tokio::test]
    async fn test_many_sst_sr_iter() {
        let root_path = Path::from("");
        let object_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let format = SsTableFormat {
            min_filter_keys: 3,
            ..SsTableFormat::default()
        };
        let table_store = Arc::new(TableStore::new(
            ObjectStores::new(object_store, None),
            format,
            root_path.clone(),
            None,
        ));
        let mut builder = table_store.table_builder();
        builder
            .add_value(b"key1", b"value1", gen_attrs(1))
            .await
            .unwrap();
        builder
            .add_value(b"key2", b"value2", gen_attrs(2))
            .await
            .unwrap();
        let encoded = builder.build().await.unwrap();
        let id1 = SsTableId::Compacted(ulid::Ulid::new());
        let handle1 = table_store.write_sst(&id1, encoded, false).await.unwrap();
        let mut builder = table_store.table_builder();
        builder
            .add_value(b"key3", b"value3", gen_attrs(3))
            .await
            .unwrap();
        let encoded = builder.build().await.unwrap();
        let id2 = SsTableId::Compacted(ulid::Ulid::new());
        let handle2 = table_store.write_sst(&id2, encoded, false).await.unwrap();
        let sr = SortedRun {
            id: 0,
            ssts: vec![handle1, handle2],
        };

        let mut iter = SortedRunIterator::new_owned_initialized(
            ..,
            sr,
            table_store.clone(),
            SstIteratorOptions::default(),
        )
        .await
        .unwrap();

        let kv = iter.next().await.unwrap().unwrap();
        assert_eq!(kv.key, b"key1".as_slice());
        assert_eq!(kv.value, b"value1".as_slice());
        let kv = iter.next().await.unwrap().unwrap();
        assert_eq!(kv.key, b"key2".as_slice());
        assert_eq!(kv.value, b"value2".as_slice());
        let kv = iter.next().await.unwrap().unwrap();
        assert_eq!(kv.key, b"key3".as_slice());
        assert_eq!(kv.value, b"value3".as_slice());
        let kv = iter.next().await.unwrap();
        assert!(kv.is_none());
    }

    #[tokio::test]
    async fn test_sr_iter_from_key() {
        let root_path = Path::from("");
        let object_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let format = SsTableFormat {
            min_filter_keys: 3,
            ..SsTableFormat::default()
        };
        let table_store = Arc::new(TableStore::new(
            ObjectStores::new(object_store, None),
            format,
            root_path.clone(),
            None,
        ));
        let key_gen = OrderedBytesGenerator::new_with_byte_range(&[b'a'; 16], b'a', b'z');
        let mut test_case_key_gen = key_gen.clone();
        let val_gen = OrderedBytesGenerator::new_with_byte_range(&[0u8; 16], 0u8, 26u8);
        let mut test_case_val_gen = val_gen.clone();
        let sr = build_sr_with_ssts(table_store.clone(), 3, 10, key_gen, val_gen).await;

        for i in 0..30 {
            let mut expected_key_gen = test_case_key_gen.clone();
            let mut expected_val_gen = test_case_val_gen.clone();
            let from_key = test_case_key_gen.next();
            _ = test_case_val_gen.next();
            let mut iter = SortedRunIterator::new_borrowed_initialized(
                from_key.as_ref()..,
                &sr,
                table_store.clone(),
                SstIteratorOptions::default(),
            )
            .await
            .unwrap();
            for _ in 0..30 - i {
                assert_kv(
                    &iter.next().await.unwrap().unwrap(),
                    expected_key_gen.next().as_ref(),
                    expected_val_gen.next().as_ref(),
                );
            }
            assert!(iter.next().await.unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn test_sr_iter_from_key_lower_than_range() {
        let root_path = Path::from("");
        let object_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let format = SsTableFormat {
            min_filter_keys: 3,
            ..SsTableFormat::default()
        };
        let table_store = Arc::new(TableStore::new(
            ObjectStores::new(object_store, None),
            format,
            root_path.clone(),
            None,
        ));
        let key_gen = OrderedBytesGenerator::new_with_byte_range(&[b'a'; 16], b'a', b'z');
        let mut expected_key_gen = key_gen.clone();
        let val_gen = OrderedBytesGenerator::new_with_byte_range(&[0u8; 16], 0u8, 26u8);
        let mut expected_val_gen = val_gen.clone();
        let sr = build_sr_with_ssts(table_store.clone(), 3, 10, key_gen, val_gen).await;
        let mut iter = SortedRunIterator::new_borrowed_initialized(
            [b'a', 10].as_ref()..,
            &sr,
            table_store.clone(),
            SstIteratorOptions::default(),
        )
        .await
        .unwrap();

        for _ in 0..30 {
            assert_kv(
                &iter.next().await.unwrap().unwrap(),
                expected_key_gen.next().as_ref(),
                expected_val_gen.next().as_ref(),
            );
        }
        assert!(iter.next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_sr_iter_from_key_higher_than_range() {
        let root_path = Path::from("");
        let object_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let format = SsTableFormat {
            min_filter_keys: 3,
            ..SsTableFormat::default()
        };
        let table_store = Arc::new(TableStore::new(
            ObjectStores::new(object_store, None),
            format,
            root_path.clone(),
            None,
        ));
        let key_gen = OrderedBytesGenerator::new_with_byte_range(&[b'a'; 16], b'a', b'z');
        let val_gen = OrderedBytesGenerator::new_with_byte_range(&[0u8; 16], 0u8, 26u8);
        let sr = build_sr_with_ssts(table_store.clone(), 3, 10, key_gen, val_gen).await;

        let mut iter = SortedRunIterator::new_borrowed_initialized(
            [b'z', 30].as_ref()..,
            &sr,
            table_store.clone(),
            SstIteratorOptions::default(),
        )
        .await
        .unwrap();

        assert!(iter.next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_seek_through_sorted_run() {
        let root_path = Path::from("");
        let object_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let table_store = Arc::new(TableStore::new(
            ObjectStores::new(object_store, None),
            SsTableFormat::default(),
            root_path.clone(),
            None,
        ));

        let mut rng = proptest_util::rng::new_test_rng(None);
        let table = sample::table(&mut rng, 400, 10);
        let max_entries_per_sst = 20u64;
        let entries_per_sst = 1..max_entries_per_sst;
        let sr =
            build_sorted_run_from_table(&table, table_store.clone(), entries_per_sst, &mut rng)
                .await;
        let mut sr_iter = SortedRunIterator::new_owned_initialized(
            ..,
            sr,
            table_store.clone(),
            SstIteratorOptions::default(),
        )
        .await
        .unwrap();
        let mut table_iter = table.iter();
        loop {
            let skip = rng.random::<u64>() % (max_entries_per_sst * 2);
            let run = rng.random::<u64>() % (max_entries_per_sst * 2);

            let Some((k, _)) = table_iter.nth(skip as usize) else {
                break;
            };
            let seek_key = increment_length(k);
            sr_iter.seek(&seek_key).await.unwrap();

            for (key, value) in table_iter.by_ref().take(run as usize) {
                let kv = sr_iter.next().await.unwrap().unwrap();
                assert_eq!(*key, kv.key);
                assert_eq!(*value, kv.value);
            }
        }
    }

    fn increment_length(b: &[u8]) -> Bytes {
        let mut buf = BytesMut::from(b);
        buf.put_u8(u8::MIN);
        buf.freeze()
    }

    async fn build_sorted_run_from_table<R: SampleRange<u64> + Clone>(
        table: &BTreeMap<Bytes, Bytes>,
        table_store: Arc<TableStore>,
        entries_per_sst: R,
        rng: &mut TestRng,
    ) -> SortedRun {
        let mut ssts = Vec::new();
        let mut entries = table.iter();
        loop {
            let sst_len = rng.random_range(entries_per_sst.clone());
            let mut builder = table_store.table_builder();

            let sst_kvs: Vec<(&Bytes, &Bytes)> = entries.by_ref().take(sst_len as usize).collect();
            if sst_kvs.is_empty() {
                break;
            }

            for (key, value) in sst_kvs {
                builder.add_value(key, value, gen_attrs(0)).await.unwrap();
            }

            let encoded = builder.build().await.unwrap();
            let id = SsTableId::Compacted(ulid::Ulid::new());
            let handle = table_store.write_sst(&id, encoded, false).await.unwrap();
            ssts.push(handle);
        }

        SortedRun { id: 0, ssts }
    }

    async fn build_sr_with_ssts(
        table_store: Arc<TableStore>,
        n: usize,
        keys_per_sst: usize,
        mut key_gen: OrderedBytesGenerator,
        mut val_gen: OrderedBytesGenerator,
    ) -> SortedRun {
        let mut ssts = Vec::<SsTableHandle>::new();
        for _ in 0..n {
            let mut writer = table_store.table_writer(SsTableId::Compacted(ulid::Ulid::new()));
            for _ in 0..keys_per_sst {
                let entry =
                    RowEntry::new_value(key_gen.next().as_ref(), val_gen.next().as_ref(), 0);
                writer.add(entry).await.unwrap();
            }
            let sst = writer.close().await.unwrap();
            ssts.push(sst);
        }
        SortedRun { id: 0, ssts }
    }

    mod mixed_version_tests {
        use super::*;
        use crate::sst_builder::BlockFormat;

        async fn build_sst_v1(
            table_store: &Arc<TableStore>,
            keys_and_values: &[(&[u8], &[u8])],
        ) -> SsTableHandle {
            let mut builder = table_store.table_builder();
            for (key, value) in keys_and_values {
                builder.add_value(key, value, gen_attrs(0)).await.unwrap();
            }
            let encoded = builder.build().await.unwrap();
            let id = SsTableId::Compacted(ulid::Ulid::new());
            table_store.write_sst(&id, encoded, false).await.unwrap()
        }

        async fn build_sst_v2(
            table_store: &Arc<TableStore>,
            keys_and_values: &[(&[u8], &[u8])],
        ) -> SsTableHandle {
            let mut builder = table_store
                .table_builder()
                .with_block_format(BlockFormat::V2);
            for (key, value) in keys_and_values {
                builder.add_value(key, value, gen_attrs(0)).await.unwrap();
            }
            let encoded = builder.build().await.unwrap();
            let id = SsTableId::Compacted(ulid::Ulid::new());
            table_store.write_sst(&id, encoded, false).await.unwrap()
        }

        #[tokio::test]
        async fn should_iterate_sorted_run_with_mixed_v1_and_v2_ssts() {
            // given: a sorted run with alternating v1 and v2 SSTs
            let root_path = Path::from("");
            let object_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
            let format = SsTableFormat {
                min_filter_keys: 10,
                ..SsTableFormat::default()
            };
            let table_store = Arc::new(TableStore::new(
                ObjectStores::new(object_store, None),
                format,
                root_path,
                None,
            ));

            // Build a sorted run with v1, v2, v1, v2 SSTs
            let sst1_v1 = build_sst_v1(
                &table_store,
                &[(b"key01", b"value01"), (b"key02", b"value02")],
            )
            .await;
            let sst2_v2 = build_sst_v2(
                &table_store,
                &[(b"key03", b"value03"), (b"key04", b"value04")],
            )
            .await;
            let sst3_v1 = build_sst_v1(
                &table_store,
                &[(b"key05", b"value05"), (b"key06", b"value06")],
            )
            .await;
            let sst4_v2 = build_sst_v2(
                &table_store,
                &[(b"key07", b"value07"), (b"key08", b"value08")],
            )
            .await;

            let sorted_run = SortedRun {
                id: 0,
                ssts: vec![sst1_v1, sst2_v2, sst3_v1, sst4_v2],
            };

            // when: iterating over the sorted run
            let mut iter = SortedRunIterator::new_owned_initialized(
                ..,
                sorted_run,
                table_store.clone(),
                SstIteratorOptions::default(),
            )
            .await
            .unwrap();

            // then: all keys should be returned in order across both v1 and v2 SSTs
            for i in 1..=8 {
                let kv = iter.next().await.unwrap().unwrap();
                let expected_key = format!("key{:02}", i);
                let expected_value = format!("value{:02}", i);
                assert_eq!(kv.key.as_ref(), expected_key.as_bytes());
                assert_eq!(kv.value.as_ref(), expected_value.as_bytes());
            }

            let kv = iter.next().await.unwrap();
            assert!(kv.is_none());
        }

        #[tokio::test]
        async fn should_seek_through_mixed_v1_and_v2_ssts() {
            // given: a sorted run with alternating v1 and v2 SSTs
            let root_path = Path::from("");
            let object_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
            let format = SsTableFormat {
                min_filter_keys: 10,
                ..SsTableFormat::default()
            };
            let table_store = Arc::new(TableStore::new(
                ObjectStores::new(object_store, None),
                format,
                root_path,
                None,
            ));

            // Build a sorted run with v1, v2, v1, v2 SSTs
            let sst1_v1 = build_sst_v1(
                &table_store,
                &[(b"key01", b"value01"), (b"key02", b"value02")],
            )
            .await;
            let sst2_v2 = build_sst_v2(
                &table_store,
                &[(b"key03", b"value03"), (b"key04", b"value04")],
            )
            .await;
            let sst3_v1 = build_sst_v1(
                &table_store,
                &[(b"key05", b"value05"), (b"key06", b"value06")],
            )
            .await;
            let sst4_v2 = build_sst_v2(
                &table_store,
                &[(b"key07", b"value07"), (b"key08", b"value08")],
            )
            .await;

            let sorted_run = SortedRun {
                id: 0,
                ssts: vec![sst1_v1, sst2_v2, sst3_v1, sst4_v2],
            };

            let mut iter = SortedRunIterator::new_owned_initialized(
                ..,
                sorted_run,
                table_store.clone(),
                SstIteratorOptions::default(),
            )
            .await
            .unwrap();

            // when: seeking to key05 (which is in a v1 SST after a v2 SST)
            iter.seek(b"key05").await.unwrap();

            // then: we should get key05 and subsequent keys
            let kv = iter.next().await.unwrap().unwrap();
            assert_eq!(kv.key.as_ref(), b"key05");
            assert_eq!(kv.value.as_ref(), b"value05");

            let kv = iter.next().await.unwrap().unwrap();
            assert_eq!(kv.key.as_ref(), b"key06");
            assert_eq!(kv.value.as_ref(), b"value06");

            // Seek again to a v2 SST
            iter.seek(b"key07").await.unwrap();

            let kv = iter.next().await.unwrap().unwrap();
            assert_eq!(kv.key.as_ref(), b"key07");
            assert_eq!(kv.value.as_ref(), b"value07");
        }
    }

    #[tokio::test]
    async fn test_sorted_run_descending_single_sst() {
        // given: a SortedRun with one SST
        let root_path = Path::from("");
        let object_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let format = SsTableFormat {
            min_filter_keys: 10,
            ..SsTableFormat::default()
        };
        let table_store = Arc::new(TableStore::new(
            ObjectStores::new(object_store, None),
            format,
            root_path,
            None,
        ));

        let mut builder = table_store.table_builder();
        builder
            .add_value(b"key1", b"value1", gen_attrs(1))
            .await
            .unwrap();
        builder
            .add_value(b"key2", b"value2", gen_attrs(2))
            .await
            .unwrap();
        builder
            .add_value(b"key3", b"value3", gen_attrs(3))
            .await
            .unwrap();
        let encoded = builder.build().await.unwrap();
        let id = SsTableId::Compacted(ulid::Ulid::new());
        let handle = table_store.write_sst(&id, encoded, false).await.unwrap();
        let sr = SortedRun {
            id: 0,
            ssts: vec![handle],
        };

        // when: iterating in descending order
        let sst_iter_options = SstIteratorOptions {
            order: IterationOrder::Descending,
            ..SstIteratorOptions::default()
        };
        let mut iter =
            SortedRunIterator::new_owned_initialized(.., sr, table_store, sst_iter_options)
                .await
                .unwrap();

        // then: keys should be returned in reverse order
        let kv = iter.next().await.unwrap().unwrap();
        assert_eq!(kv.key, b"key3".as_slice());
        assert_eq!(kv.value, b"value3".as_slice());
        let kv = iter.next().await.unwrap().unwrap();
        assert_eq!(kv.key, b"key2".as_slice());
        assert_eq!(kv.value, b"value2".as_slice());
        let kv = iter.next().await.unwrap().unwrap();
        assert_eq!(kv.key, b"key1".as_slice());
        assert_eq!(kv.value, b"value1".as_slice());
        let kv = iter.next().await.unwrap();
        assert!(kv.is_none());
    }

    #[tokio::test]
    async fn test_sorted_run_descending_multiple_ssts() {
        // given: a SortedRun with 3 SSTs (each with different key ranges)
        let root_path = Path::from("");
        let object_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let format = SsTableFormat {
            min_filter_keys: 10,
            ..SsTableFormat::default()
        };
        let table_store = Arc::new(TableStore::new(
            ObjectStores::new(object_store, None),
            format,
            root_path,
            None,
        ));

        // SST 1: key1, key2
        let mut builder = table_store.table_builder();
        builder
            .add_value(b"key1", b"value1", gen_attrs(1))
            .await
            .unwrap();
        builder
            .add_value(b"key2", b"value2", gen_attrs(2))
            .await
            .unwrap();
        let encoded = builder.build().await.unwrap();
        let id1 = SsTableId::Compacted(ulid::Ulid::new());
        let handle1 = table_store.write_sst(&id1, encoded, false).await.unwrap();

        // SST 2: key3, key4
        let mut builder = table_store.table_builder();
        builder
            .add_value(b"key3", b"value3", gen_attrs(3))
            .await
            .unwrap();
        builder
            .add_value(b"key4", b"value4", gen_attrs(4))
            .await
            .unwrap();
        let encoded = builder.build().await.unwrap();
        let id2 = SsTableId::Compacted(ulid::Ulid::new());
        let handle2 = table_store.write_sst(&id2, encoded, false).await.unwrap();

        // SST 3: key5, key6
        let mut builder = table_store.table_builder();
        builder
            .add_value(b"key5", b"value5", gen_attrs(5))
            .await
            .unwrap();
        builder
            .add_value(b"key6", b"value6", gen_attrs(6))
            .await
            .unwrap();
        let encoded = builder.build().await.unwrap();
        let id3 = SsTableId::Compacted(ulid::Ulid::new());
        let handle3 = table_store.write_sst(&id3, encoded, false).await.unwrap();

        let sr = SortedRun {
            id: 0,
            ssts: vec![handle1, handle2, handle3],
        };

        // when: iterating in descending order
        let sst_iter_options = SstIteratorOptions {
            order: IterationOrder::Descending,
            ..SstIteratorOptions::default()
        };
        let mut iter = SortedRunIterator::new_owned_initialized(
            ..,
            sr,
            table_store.clone(),
            sst_iter_options,
        )
        .await
        .unwrap();

        // then: iteration traverses SSTs from last to first, keys within each SST in reverse order
        let kv = iter.next().await.unwrap().unwrap();
        assert_eq!(kv.key, b"key6".as_slice());
        assert_eq!(kv.value, b"value6".as_slice());
        let kv = iter.next().await.unwrap().unwrap();
        assert_eq!(kv.key, b"key5".as_slice());
        assert_eq!(kv.value, b"value5".as_slice());
        let kv = iter.next().await.unwrap().unwrap();
        assert_eq!(kv.key, b"key4".as_slice());
        assert_eq!(kv.value, b"value4".as_slice());
        let kv = iter.next().await.unwrap().unwrap();
        assert_eq!(kv.key, b"key3".as_slice());
        assert_eq!(kv.value, b"value3".as_slice());
        let kv = iter.next().await.unwrap().unwrap();
        assert_eq!(kv.key, b"key2".as_slice());
        assert_eq!(kv.value, b"value2".as_slice());
        let kv = iter.next().await.unwrap().unwrap();
        assert_eq!(kv.key, b"key1".as_slice());
        assert_eq!(kv.value, b"value1".as_slice());
        let kv = iter.next().await.unwrap();
        assert!(kv.is_none());
    }

    #[tokio::test]
    async fn test_sorted_run_descending_seek() {
        // given: a SortedRun with multiple SSTs
        let root_path = Path::from("");
        let object_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let format = SsTableFormat {
            min_filter_keys: 10,
            ..SsTableFormat::default()
        };
        let table_store = Arc::new(TableStore::new(
            ObjectStores::new(object_store, None),
            format,
            root_path,
            None,
        ));

        // SST 1: key01, key02
        let mut builder = table_store.table_builder();
        builder
            .add_value(b"key01", b"value01", gen_attrs(1))
            .await
            .unwrap();
        builder
            .add_value(b"key02", b"value02", gen_attrs(2))
            .await
            .unwrap();
        let encoded = builder.build().await.unwrap();
        let id1 = SsTableId::Compacted(ulid::Ulid::new());
        let handle1 = table_store.write_sst(&id1, encoded, false).await.unwrap();

        // SST 2: key03, key04 (middle SST)
        let mut builder = table_store.table_builder();
        builder
            .add_value(b"key03", b"value03", gen_attrs(3))
            .await
            .unwrap();
        builder
            .add_value(b"key04", b"value04", gen_attrs(4))
            .await
            .unwrap();
        let encoded = builder.build().await.unwrap();
        let id2 = SsTableId::Compacted(ulid::Ulid::new());
        let handle2 = table_store.write_sst(&id2, encoded, false).await.unwrap();

        // SST 3: key05, key06
        let mut builder = table_store.table_builder();
        builder
            .add_value(b"key05", b"value05", gen_attrs(5))
            .await
            .unwrap();
        builder
            .add_value(b"key06", b"value06", gen_attrs(6))
            .await
            .unwrap();
        let encoded = builder.build().await.unwrap();
        let id3 = SsTableId::Compacted(ulid::Ulid::new());
        let handle3 = table_store.write_sst(&id3, encoded, false).await.unwrap();

        let sr = SortedRun {
            id: 0,
            ssts: vec![handle1, handle2, handle3],
        };

        // when: creating descending iterator and seeking to key in middle SST
        let sst_iter_options = SstIteratorOptions {
            order: IterationOrder::Descending,
            ..SstIteratorOptions::default()
        };
        let mut iter = SortedRunIterator::new_owned_initialized(
            ..,
            sr,
            table_store.clone(),
            sst_iter_options,
        )
        .await
        .unwrap();

        // Seek to key04 (in middle SST)
        iter.seek(b"key04").await.unwrap();

        // then: should return key04 and preceding keys
        let kv = iter.next().await.unwrap().unwrap();
        assert_eq!(kv.key.as_ref(), b"key04");
        assert_eq!(kv.value.as_ref(), b"value04");

        let kv = iter.next().await.unwrap().unwrap();
        assert_eq!(kv.key.as_ref(), b"key03");
        assert_eq!(kv.value.as_ref(), b"value03");

        let kv = iter.next().await.unwrap().unwrap();
        assert_eq!(kv.key.as_ref(), b"key02");
        assert_eq!(kv.value.as_ref(), b"value02");

        let kv = iter.next().await.unwrap().unwrap();
        assert_eq!(kv.key.as_ref(), b"key01");
        assert_eq!(kv.value.as_ref(), b"value01");

        let kv = iter.next().await.unwrap();
        assert!(kv.is_none());
    }

    #[test]
    fn should_iterate_arbitrary_order() {
        use crate::proptest_util::arbitrary;
        use crate::test_utils;
        use tokio::runtime::Runtime;

        let mut runner = proptest_util::runner::new(file!(), None);
        let runtime = Runtime::new().unwrap();
        let sample_table = sample::table(runner.rng(), 5, 10);

        let root_path = Path::from("");
        let object_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let format = SsTableFormat::default();
        let table_store = Arc::new(TableStore::new(
            ObjectStores::new(object_store, None),
            format,
            root_path,
            None,
        ));
        let sorted_run = runtime.block_on(build_sorted_run_from_table(
            &sample_table,
            table_store.clone(),
            1..3,
            runner.rng(),
        ));

        runner
            .run(&arbitrary::iteration_order(), |ordering| {
                let sst_iter_options = SstIteratorOptions {
                    order: ordering,
                    ..SstIteratorOptions::default()
                };
                let mut iter = runtime
                    .block_on(SortedRunIterator::new_owned_initialized(
                        ..,
                        sorted_run.clone(),
                        table_store.clone(),
                        sst_iter_options,
                    ))
                    .unwrap();
                runtime.block_on(test_utils::assert_ranged_kv_scan(
                    &sample_table,
                    &BytesRange::from(..),
                    ordering,
                    &mut iter,
                ));
                Ok(())
            })
            .unwrap();
    }
}
