use crate::entry::entry_point::OperationResult;
use crate::index::field_index::btree_index::NumericIndex;
use crate::index::field_index::on_disk_geo_index::OnDiskGeoMapIndex;
use crate::index::field_index::on_disk_map_index::OnDiskMapIndex;
use crate::index::field_index::{CardinalityEstimation, PayloadBlockCondition};
use crate::types::{
    FieldCondition, FloatPayloadType, IntPayloadType, PayloadKeyType, PointOffsetType,
};
use serde_json::Value;

pub trait PayloadFieldIndex {
    /// Load index from disk.
    fn load(&mut self) -> OperationResult<()>;

    /// Flush all pending updates to disk.
    fn flush(&self) -> OperationResult<()>;

    /// Get iterator over points fitting given `condition`
    fn filter(
        &self,
        condition: &FieldCondition,
    ) -> Option<Box<dyn Iterator<Item = PointOffsetType> + '_>>;

    /// Return estimation of points amount which satisfy given condition
    fn estimate_cardinality(&self, condition: &FieldCondition) -> Option<CardinalityEstimation>;

    /// Iterate conditions for payload blocks with minimum size of `threshold`
    /// Required for building HNSW index
    fn payload_blocks(
        &self,
        threshold: usize,
        key: PayloadKeyType,
    ) -> Box<dyn Iterator<Item = PayloadBlockCondition> + '_>;

    /// Returns an amount of unique indexed points
    fn count_indexed_points(&self) -> usize;
}

pub trait ValueIndexer<T> {
    /// Add multiple values associated with a single point
    fn add_many(&mut self, id: PointOffsetType, values: Vec<T>);

    /// Extract index-able value from payload `Value`
    fn get_value(&self, value: &Value) -> Option<T>;

    /// Add point with payload to index
    fn add_point(&mut self, id: PointOffsetType, payload: &Value) {
        match payload {
            Value::Array(values) => {
                self.add_many(id, values.iter().flat_map(|x| self.get_value(x)).collect())
            }
            _ => {
                if let Some(x) = self.get_value(payload) {
                    self.add_many(id, vec![x])
                }
            }
        }
    }

    /// remove a point from the index
    // TODO(gvelo): instead of using only PointOffsetType use the full key ( value + offset )
    // to avoid a index full-scan
    fn remove_point(&mut self, id: PointOffsetType);
}

pub trait PayloadFieldIndexBuilder {
    fn add(&mut self, id: PointOffsetType, value: &Value);
}

/// Common interface for all possible types of field indexes
/// Enables polymorphism on field indexes
/// TODO: Rename with major release
#[allow(clippy::enum_variant_names)]
pub enum FieldIndex {
    IntIndex(NumericIndex<IntPayloadType>),
    IntMapIndex(OnDiskMapIndex<IntPayloadType>),
    KeywordIndex(OnDiskMapIndex<String>),
    FloatIndex(NumericIndex<FloatPayloadType>),
    GeoIndex(OnDiskGeoMapIndex),
}

impl FieldIndex {
    fn get_payload_field_index(&self) -> &dyn PayloadFieldIndex {
        match self {
            FieldIndex::IntIndex(payload_field_index) => payload_field_index,
            FieldIndex::IntMapIndex(payload_field_index) => payload_field_index,
            FieldIndex::KeywordIndex(payload_field_index) => payload_field_index,
            FieldIndex::FloatIndex(payload_field_index) => payload_field_index,
            FieldIndex::GeoIndex(payload_field_index) => payload_field_index,
        }
    }
    fn get_payload_field_index_mut(&mut self) -> &mut dyn PayloadFieldIndex {
        match self {
            FieldIndex::IntIndex(ref mut payload_field_index) => payload_field_index,
            FieldIndex::IntMapIndex(ref mut payload_field_index) => payload_field_index,
            FieldIndex::KeywordIndex(ref mut payload_field_index) => payload_field_index,
            FieldIndex::FloatIndex(ref mut payload_field_index) => payload_field_index,
            FieldIndex::GeoIndex(ref mut payload_field_index) => payload_field_index,
        }
    }

    pub fn load(&mut self) -> OperationResult<()> {
        self.get_payload_field_index_mut().load()
    }

    pub fn flush(&self) -> OperationResult<()> {
        self.get_payload_field_index().flush()
    }

    pub fn filter(
        &self,
        condition: &FieldCondition,
    ) -> Option<Box<dyn Iterator<Item = PointOffsetType> + '_>> {
        self.get_payload_field_index().filter(condition)
    }

    pub fn estimate_cardinality(
        &self,
        condition: &FieldCondition,
    ) -> Option<CardinalityEstimation> {
        self.get_payload_field_index()
            .estimate_cardinality(condition)
    }

    pub fn payload_blocks(
        &self,
        threshold: usize,
        key: PayloadKeyType,
    ) -> Box<dyn Iterator<Item = PayloadBlockCondition> + '_> {
        self.get_payload_field_index()
            .payload_blocks(threshold, key)
    }

    pub fn count_indexed_points(&self) -> usize {
        self.get_payload_field_index().count_indexed_points()
    }

    pub fn add_point(&mut self, id: PointOffsetType, payload: &Value) {
        match self {
            FieldIndex::IntIndex(ref mut payload_field_index) => {
                payload_field_index.add_point(id, payload)
            }
            FieldIndex::IntMapIndex(ref mut payload_field_index) => {
                payload_field_index.add_point(id, payload)
            }
            FieldIndex::KeywordIndex(ref mut payload_field_index) => {
                payload_field_index.add_point(id, payload)
            }
            FieldIndex::FloatIndex(ref mut payload_field_index) => {
                payload_field_index.add_point(id, payload)
            }
            FieldIndex::GeoIndex(ref mut payload_field_index) => {
                payload_field_index.add_point(id, payload)
            }
        }
    }

    pub fn remove_point(&mut self, point_id: PointOffsetType) {
        // TODO(gvelo): refactor remove_point and remove unwrap()
        match self {
            FieldIndex::IntIndex(index) => index.remove_point(point_id).unwrap(),
            FieldIndex::IntMapIndex(index) => index.remove_point(point_id),
            FieldIndex::KeywordIndex(index) => index.remove_point(point_id),
            FieldIndex::FloatIndex(index) => index.remove_point(point_id).unwrap(),
            FieldIndex::GeoIndex(index) => index.remove_point(point_id),
        }
    }
}
