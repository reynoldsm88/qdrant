use std::collections::HashMap;
use std::hash::Hash;
use std::iter;

use serde_json::Value;

use crate::common::rocksdb_operations::db_write_options;
use crate::entry::entry_point::{OperationError, OperationResult};
use crate::index::field_index::PayloadFieldIndex;
use crate::index::field_index::{
    CardinalityEstimation, PayloadBlockCondition, PrimaryCondition, ValueIndexer,
};
use crate::types::{
    FieldCondition, IntPayloadType, Match, MatchValue, PayloadKeyType, PointOffsetType,
    ValueVariants,
};
use atomic_refcell::AtomicRefCell;
use rocksdb::{IteratorMode, DB};
use std::fmt::Display;
use std::str::FromStr;
use std::sync::Arc;

/// HashMap-based type of index
pub struct OnDiskMapIndex<N: Hash + Eq + Clone + Display> {
    map: HashMap<N, Vec<PointOffsetType>>,
    point_to_values: Vec<Vec<N>>,
    total_points: usize,
    store_cf_name: String,
    store: Arc<AtomicRefCell<DB>>,
}

impl<N: Hash + Eq + Clone + Display + FromStr> OnDiskMapIndex<N> {
    #[allow(dead_code)]
    pub fn new(store: Arc<AtomicRefCell<DB>>, store_cf_name: &str) -> OnDiskMapIndex<N> {
        OnDiskMapIndex {
            map: HashMap::new(),
            point_to_values: Vec::new(),
            total_points: 0,
            store_cf_name: String::from(store_cf_name),
            store,
        }
    }

    #[allow(dead_code)]
    fn load(&mut self) -> OperationResult<()> {
        let store_ref = self.store.borrow();
        let cf_handle = store_ref.cf_handle(&self.store_cf_name).ok_or_else(|| {
            OperationError::service_error(&format!(
                "Index load error: column family {} not found",
                self.store_cf_name
            ))
        })?;
        for (record, _) in store_ref.iterator_cf(cf_handle, IteratorMode::Start) {
            let record = std::str::from_utf8(&record).map_err(|_| {
                OperationError::service_error("Index load error: UTF8 error while DB parsing")
            })?;
            let (value, idx) = Self::decode_db_record(record)?;
            if self.point_to_values.len() <= idx as usize {
                self.point_to_values.resize(idx as usize + 1, Vec::new())
            }
            if self.point_to_values[idx as usize].is_empty() {
                self.total_points += 1;
            }
            self.point_to_values[idx as usize].push(value.clone());
            self.map.entry(value).or_default().push(idx);
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn flush(&self) -> OperationResult<()> {
        let store_ref = self.store.borrow();
        let cf_handle = store_ref.cf_handle(&self.store_cf_name).ok_or_else(|| {
            OperationError::service_error(&format!(
                "Index flush error: column family {} not found",
                self.store_cf_name
            ))
        })?;
        Ok(store_ref.flush_cf(cf_handle)?)
    }

    pub fn match_cardinality(&self, value: &N) -> CardinalityEstimation {
        let values_count = match self.map.get(value) {
            None => 0,
            Some(points) => points.len(),
        };

        CardinalityEstimation {
            primary_clauses: vec![],
            min: values_count,
            exp: values_count,
            max: values_count,
        }
    }

    #[allow(dead_code)]
    pub fn get_values(&self, idx: PointOffsetType) -> Option<&Vec<N>> {
        self.point_to_values.get(idx as usize)
    }

    fn add_many_to_map(&mut self, idx: PointOffsetType, values: Vec<N>) -> OperationResult<()> {
        let store_ref = self.store.borrow();
        let cf_handle = store_ref.cf_handle(&self.store_cf_name).ok_or_else(|| {
            OperationError::service_error(&format!(
                "Index add error: column family {} not found",
                self.store_cf_name
            ))
        })?;

        if self.point_to_values.len() <= idx as usize {
            self.point_to_values.resize(idx as usize + 1, Vec::new())
        }
        self.point_to_values[idx as usize] = values.into_iter().collect();
        let mut empty = true;
        for value in &self.point_to_values[idx as usize] {
            let entry = self.map.entry(value.clone()).or_default();
            entry.push(idx);
            empty = false;

            let db_record = Self::encode_db_record(value, idx);
            store_ref
                .put_cf_opt(cf_handle, &db_record, &[], &db_write_options())
                .map_err(|e| {
                    OperationError::service_error(&format!("Index db update error: {}", e))
                })?;
        }
        if !empty {
            self.total_points += 1;
        }
        Ok(())
    }

    fn get_iterator(&self, value: &N) -> Box<dyn Iterator<Item = PointOffsetType> + '_> {
        self.map
            .get(value)
            .map(|ids| Box::new(ids.iter().copied()) as Box<dyn Iterator<Item = PointOffsetType>>)
            .unwrap_or_else(|| Box::new(iter::empty::<PointOffsetType>()))
    }

    fn encode_db_record(value: &N, idx: PointOffsetType) -> String {
        format!("{}/{}", value, idx)
    }

    fn decode_db_record(s: &str) -> OperationResult<(N, PointOffsetType)> {
        const DECODE_ERR: &str = "Index db parsing error: wrong data format";
        let separator_pos = s
            .rfind('/')
            .ok_or_else(|| OperationError::service_error(DECODE_ERR))?;
        if separator_pos == s.len() - 1 {
            return Err(OperationError::service_error(DECODE_ERR));
        }
        let value_str = &s[..separator_pos];
        let value =
            N::from_str(value_str).map_err(|_| OperationError::service_error(DECODE_ERR))?;
        let idx_str = &s[separator_pos + 1..];
        let idx = PointOffsetType::from_str(idx_str)
            .map_err(|_| OperationError::service_error(DECODE_ERR))?;
        Ok((value, idx))
    }

    #[allow(dead_code)]
    fn remove_point(&mut self, idx: PointOffsetType) -> OperationResult<()> {
        let store_ref = self.store.borrow();

        let cf_handle = store_ref.cf_handle(&self.store_cf_name).ok_or_else(|| {
            OperationError::service_error(&format!(
                "point remove error: column family {} not found",
                self.store_cf_name
            ))
        })?;

        if self.point_to_values.len() <= idx as usize {
            return Ok(());
        }

        let removed_values = std::mem::take(&mut self.point_to_values[idx as usize]);

        for value in &removed_values {
            let key = OnDiskMapIndex::encode_db_record(value, idx);
            self.map.remove(value);
            store_ref.delete_cf(cf_handle, key)?;
        }

        Ok(())
    }
}

impl PayloadFieldIndex for OnDiskMapIndex<String> {
    fn load(&mut self) -> OperationResult<()> {
        OnDiskMapIndex::load(self)
    }

    fn flush(&self) -> OperationResult<()> {
        OnDiskMapIndex::flush(self)
    }

    fn filter(
        &self,
        condition: &FieldCondition,
    ) -> Option<Box<dyn Iterator<Item = PointOffsetType> + '_>> {
        match &condition.r#match {
            Some(Match::Value(MatchValue {
                value: ValueVariants::Keyword(keyword),
            })) => Some(self.get_iterator(keyword)),
            _ => None,
        }
    }

    fn estimate_cardinality(&self, condition: &FieldCondition) -> Option<CardinalityEstimation> {
        match &condition.r#match {
            Some(Match::Value(MatchValue {
                value: ValueVariants::Keyword(keyword),
            })) => {
                let mut estimation = self.match_cardinality(keyword);
                estimation
                    .primary_clauses
                    .push(PrimaryCondition::Condition(condition.clone()));
                Some(estimation)
            }
            _ => None,
        }
    }

    fn payload_blocks(
        &self,
        threshold: usize,
        key: PayloadKeyType,
    ) -> Box<dyn Iterator<Item = PayloadBlockCondition> + '_> {
        let iter = self
            .map
            .iter()
            .filter(move |(_value, point_ids)| point_ids.len() > threshold)
            .map(move |(value, point_ids)| PayloadBlockCondition {
                condition: FieldCondition::new_match(key.clone(), value.to_owned().into()),
                cardinality: point_ids.len(),
            });
        Box::new(iter)
    }

    fn count_indexed_points(&self) -> usize {
        self.total_points
    }
}

impl PayloadFieldIndex for OnDiskMapIndex<IntPayloadType> {
    fn load(&mut self) -> OperationResult<()> {
        OnDiskMapIndex::load(self)
    }

    fn flush(&self) -> OperationResult<()> {
        OnDiskMapIndex::flush(self)
    }

    fn filter(
        &self,
        condition: &FieldCondition,
    ) -> Option<Box<dyn Iterator<Item = PointOffsetType> + '_>> {
        match &condition.r#match {
            Some(Match::Value(MatchValue {
                value: ValueVariants::Integer(integer),
            })) => Some(self.get_iterator(integer)),
            _ => None,
        }
    }

    fn estimate_cardinality(&self, condition: &FieldCondition) -> Option<CardinalityEstimation> {
        match &condition.r#match {
            Some(Match::Value(MatchValue {
                value: ValueVariants::Integer(integer),
            })) => {
                let mut estimation = self.match_cardinality(integer);
                estimation
                    .primary_clauses
                    .push(PrimaryCondition::Condition(condition.clone()));
                Some(estimation)
            }
            _ => None,
        }
    }

    fn payload_blocks(
        &self,
        threshold: usize,
        key: PayloadKeyType,
    ) -> Box<dyn Iterator<Item = PayloadBlockCondition> + '_> {
        let iter = self
            .map
            .iter()
            .filter(move |(_value, point_ids)| point_ids.len() >= threshold)
            .map(move |(value, point_ids)| PayloadBlockCondition {
                condition: FieldCondition::new_match(key.clone(), (*value).into()),
                cardinality: point_ids.len(),
            });
        Box::new(iter)
    }

    fn count_indexed_points(&self) -> usize {
        self.total_points
    }
}

impl ValueIndexer<String> for OnDiskMapIndex<String> {
    fn add_many(&mut self, id: PointOffsetType, values: Vec<String>) {
        self.add_many_to_map(id, values).unwrap();
    }

    fn get_value(&self, value: &Value) -> Option<String> {
        if let Value::String(keyword) = value {
            return Some(keyword.to_owned());
        }
        None
    }

    fn remove_point(&mut self, id: PointOffsetType) {
        self.remove_point(id).unwrap()
    }
}

impl ValueIndexer<IntPayloadType> for OnDiskMapIndex<IntPayloadType> {
    fn add_many(&mut self, id: PointOffsetType, values: Vec<IntPayloadType>) {
        self.add_many_to_map(id, values).unwrap();
    }

    fn get_value(&self, value: &Value) -> Option<IntPayloadType> {
        if let Value::Number(num) = value {
            return num.as_i64();
        }
        None
    }

    fn remove_point(&mut self, id: PointOffsetType) {
        self.remove_point(id).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use crate::common::rocksdb_operations::db_options;
    use std::collections::HashSet;
    use std::fmt::Debug;
    use std::iter::FromIterator;
    use std::path::Path;
    use tempdir::TempDir;

    use super::*;

    const CF_NAME: &str = "test_cf";

    fn open_db(path: &Path) -> Arc<AtomicRefCell<DB>> {
        let db = DB::open_cf(&db_options(), path, &[CF_NAME]).unwrap();
        Arc::new(AtomicRefCell::new(db))
    }

    fn save_map_index<N: Hash + Eq + Clone + Display + FromStr + Debug>(
        data: &[Vec<N>],
        path: &Path,
    ) {
        let mut index = OnDiskMapIndex::<N>::new(open_db(path), CF_NAME);
        for (idx, values) in data.iter().enumerate() {
            index
                .add_many_to_map(idx as PointOffsetType, values.clone())
                .unwrap();
        }
        index.flush().unwrap();
    }

    fn load_map_index<N: Hash + Eq + Clone + Display + FromStr + Debug>(
        data: &[Vec<N>],
        path: &Path,
    ) {
        let mut index = OnDiskMapIndex::<N>::new(open_db(path), CF_NAME);
        index.load().unwrap();
        for (idx, values) in data.iter().enumerate() {
            let index_values: HashSet<N> = HashSet::from_iter(
                index
                    .get_values(idx as PointOffsetType)
                    .unwrap()
                    .iter()
                    .cloned(),
            );
            let check_values: HashSet<N> = HashSet::from_iter(values.iter().cloned());
            assert_eq!(index_values, check_values);
        }
    }

    #[test]
    fn test_int_disk_map_index() {
        let data = vec![
            vec![1, 2, 3, 4, 5, 6],
            vec![1, 2, 3, 4, 5, 6],
            vec![13, 14, 15, 16, 17, 18],
            vec![19, 20, 21, 22, 23, 24],
            vec![25],
        ];

        let tmp_dir = TempDir::new("store_dir").unwrap();
        save_map_index(&data, tmp_dir.path());
        load_map_index(&data, tmp_dir.path());
    }

    #[test]
    fn test_string_disk_map_index() {
        let data = vec![
            vec![
                String::from("AABB"),
                String::from("UUFF"),
                String::from("IIBB"),
            ],
            vec![
                String::from("PPMM"),
                String::from("QQXX"),
                String::from("YYBB"),
            ],
            vec![
                String::from("FFMM"),
                String::from("IICC"),
                String::from("IIBB"),
            ],
            vec![
                String::from("AABB"),
                String::from("UUFF"),
                String::from("IIBB"),
            ],
            vec![String::from("PPGG")],
        ];

        let tmp_dir = TempDir::new("store_dir").unwrap();
        save_map_index(&data, tmp_dir.path());
        load_map_index(&data, tmp_dir.path());
    }
}
