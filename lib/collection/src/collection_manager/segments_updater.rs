use std::collections::{HashMap, HashSet};

use itertools::Itertools;
use parking_lot::{RwLock, RwLockWriteGuard};
use segment::data_types::named_vectors::NamedVectors;
use segment::entry::entry_point::{OperationResult, SegmentEntry};
use segment::types::{
    Filter, Payload, PayloadFieldSchema, PayloadKeyType, PayloadKeyTypeRef, PointIdType,
    SeqNumberType,
};

use crate::collection_manager::holders::segment_holder::SegmentHolder;
use crate::operations::payload_ops::PayloadOps;
use crate::operations::point_ops::{PointInsertOperations, PointOperations, PointStruct};
use crate::operations::types::{CollectionError, CollectionResult};
use crate::operations::FieldIndexOperations;

/// A collection of functions for updating points and payloads stored in segments

pub(crate) fn check_unprocessed_points(
    points: &[PointIdType],
    processed: &HashSet<PointIdType>,
) -> CollectionResult<usize> {
    let unprocessed_points = points
        .iter()
        .cloned()
        .filter(|p| !processed.contains(p))
        .collect_vec();
    let missed_point = unprocessed_points.iter().cloned().next();

    // ToDo: check pre-existing points

    match missed_point {
        None => Ok(processed.len()),
        Some(missed_point) => Err(CollectionError::PointNotFound {
            missed_point_id: missed_point,
        }),
    }
}

/// Tries to delete points from all segments, returns number of actually deleted points
pub(crate) fn delete_points(
    segments: &SegmentHolder,
    op_num: SeqNumberType,
    ids: &[PointIdType],
) -> CollectionResult<usize> {
    let res = segments.apply_points(ids, |id, _idx, write_segment| {
        write_segment.delete_point(op_num, id)
    })?;
    Ok(res)
}

pub(crate) fn set_payload(
    segments: &SegmentHolder,
    op_num: SeqNumberType,
    payload: &Payload,
    points: &[PointIdType],
) -> CollectionResult<usize> {
    let updated_points =
        segments.apply_points_to_appendable(op_num, points, |id, write_segment| {
            write_segment.set_payload(op_num, id, payload)?;
            Ok(true)
        })?;

    check_unprocessed_points(points, &updated_points)?;
    Ok(updated_points.len())
}

pub(crate) fn delete_payload(
    segments: &SegmentHolder,
    op_num: SeqNumberType,
    points: &[PointIdType],
    keys: &[PayloadKeyType],
) -> CollectionResult<usize> {
    let updated_points =
        segments.apply_points_to_appendable(op_num, points, |id, write_segment| {
            let mut res = true;
            for key in keys {
                res = write_segment.delete_payload(op_num, id, key)? && res;
            }
            Ok(res)
        })?;

    check_unprocessed_points(points, &updated_points)?;
    Ok(updated_points.len())
}

pub(crate) fn clear_payload(
    segments: &SegmentHolder,
    op_num: SeqNumberType,
    points: &[PointIdType],
) -> CollectionResult<usize> {
    let updated_points =
        segments.apply_points_to_appendable(op_num, points, |id, write_segment| {
            write_segment.clear_payload(op_num, id)
        })?;

    check_unprocessed_points(points, &updated_points)?;
    Ok(updated_points.len())
}

/// Clear Payloads from all segments matching the given filter
pub(crate) fn clear_payload_by_filter(
    segments: &SegmentHolder,
    op_num: SeqNumberType,
    filter: &Filter,
) -> CollectionResult<usize> {
    let mut points_to_clear: Vec<PointIdType> = Vec::new();

    segments.apply_segments(|s| {
        let points = s.read_filtered(None, None, Some(filter));
        points_to_clear.extend_from_slice(points.as_slice());
        Ok(true)
    })?;

    let updated_points = segments.apply_points_to_appendable(
        op_num,
        points_to_clear.as_slice(),
        |id, write_segment| write_segment.clear_payload(op_num, id),
    )?;

    Ok(updated_points.len())
}

pub(crate) fn create_field_index(
    segments: &SegmentHolder,
    op_num: SeqNumberType,
    field_name: PayloadKeyTypeRef,
    field_schema: Option<&PayloadFieldSchema>,
) -> CollectionResult<usize> {
    let res = segments.apply_segments(|write_segment| {
        write_segment.create_field_index(op_num, field_name, field_schema)
    })?;
    Ok(res)
}

pub(crate) fn delete_field_index(
    segments: &SegmentHolder,
    op_num: SeqNumberType,
    field_name: PayloadKeyTypeRef,
) -> CollectionResult<usize> {
    let res = segments
        .apply_segments(|write_segment| write_segment.delete_field_index(op_num, field_name))?;
    Ok(res)
}

///
/// Returns
/// - Ok(true) if the operation was successful and point replaced existing value
/// - Ok(false) if the operation was successful and point was inserted
/// - Err if the operation failed
fn upsert_with_payload(
    segment: &mut RwLockWriteGuard<dyn SegmentEntry>,
    op_num: SeqNumberType,
    point_id: PointIdType,
    vectors: &NamedVectors,
    payload: Option<&Payload>,
) -> OperationResult<bool> {
    let mut res = segment.upsert_vector(op_num, point_id, vectors)?;
    if let Some(full_payload) = payload {
        res &= segment.set_payload(op_num, point_id, full_payload)?;
    }
    Ok(res)
}

/// Sync points within a given range
///
/// 1. Retrieve existing points for a range
/// 2. Remove points, which are not present in the sync operation
/// 3. Retrieve overlapping points, detect which one of them are changed
/// 4. Select new points
/// 5. Upsert points which differ from the stored ones
///
/// Returns:
///     (number of deleted points, number of new points, number of updated points)
pub(crate) fn sync_points(
    segments: &SegmentHolder,
    op_num: SeqNumberType,
    from_id: Option<PointIdType>,
    to_id: Option<PointIdType>,
    points: &[PointStruct],
) -> CollectionResult<(usize, usize, usize)> {
    let id_to_point = points
        .iter()
        .map(|p| (p.id, p))
        .collect::<HashMap<PointIdType, &PointStruct>>();
    let sync_points: HashSet<_> = points.iter().map(|p| p.id).collect();
    // 1. Retrieve existing points for a range
    let stored_point_ids: HashSet<_> = segments
        .iter()
        .flat_map(|(_, segment)| segment.get().read().read_range(from_id, to_id))
        .collect();
    // 2. Remove points, which are not present in the sync operation
    let points_to_remove: Vec<_> = stored_point_ids.difference(&sync_points).copied().collect();
    let deleted = delete_points(segments, op_num, points_to_remove.as_slice())?;
    // 3. Retrieve overlapping points, detect which one of them are changed
    let existing_point_ids: Vec<_> = stored_point_ids
        .intersection(&sync_points)
        .copied()
        .collect();

    let mut points_to_update: Vec<_> = Vec::new();
    let _num_updated = segments.read_points(existing_point_ids.as_slice(), |id, segment| {
        let all_vectors = segment.all_vectors(id)?;
        let payload = segment.payload(id)?;
        let point = id_to_point.get(&id).unwrap();
        if point.get_vectors() != all_vectors {
            points_to_update.push(*point);
            Ok(true)
        } else {
            let payload_match = match point.payload {
                Some(ref p) => p == &payload,
                None => Payload::default() == payload,
            };
            if !payload_match {
                points_to_update.push(*point);
                Ok(true)
            } else {
                Ok(false)
            }
        }
    })?;

    // 4. Select new points
    let num_updated = points_to_update.len();
    let mut num_new = 0;
    sync_points
        .difference(&stored_point_ids)
        .copied()
        .for_each(|id| {
            num_new += 1;
            points_to_update.push(*id_to_point.get(&id).unwrap());
        });

    // 5. Upsert points which differ from the stored ones
    let num_replaced = upsert_points(segments, op_num, points_to_update)?;
    debug_assert_eq!(num_replaced, num_updated);

    Ok((deleted, num_new, num_updated))
}

/// Checks point id in each segment, update point if found.
/// All not found points are inserted into random segment.
/// Returns: number of updated points.
pub(crate) fn upsert_points<'a, T>(
    segments: &SegmentHolder,
    op_num: SeqNumberType,
    points: T,
) -> CollectionResult<usize>
where
    T: IntoIterator<Item = &'a PointStruct>,
{
    let mut ids: Vec<PointIdType> = vec![];
    let mut points_map: HashMap<PointIdType, &PointStruct> = Default::default();
    points.into_iter().for_each(|p| {
        ids.push(p.id);
        points_map.insert(p.id, p);
    });

    // Update points in writable segments
    let updated_points =
        segments.apply_points_to_appendable(op_num, &ids, |id, write_segment| {
            let point = points_map[&id];
            upsert_with_payload(
                write_segment,
                op_num,
                id,
                &point.get_vectors(),
                point.payload.as_ref(),
            )
        })?;

    let mut res = updated_points.len();
    // Insert new points, which was not updated or existed
    let new_point_ids = ids
        .iter()
        .cloned()
        .filter(|x| !(updated_points.contains(x)));

    {
        let default_write_segment =
            segments
                .random_appendable_segment()
                .ok_or(CollectionError::ServiceError {
                    error: "No segments exists, expected at least one".to_string(),
                })?;

        let segment_arc = default_write_segment.get();
        let mut write_segment = segment_arc.write();
        for point_id in new_point_ids {
            let point = points_map[&point_id];
            res += upsert_with_payload(
                &mut write_segment,
                op_num,
                point_id,
                &point.get_vectors(),
                point.payload.as_ref(),
            )? as usize;
        }
        RwLockWriteGuard::unlock_fair(write_segment);
    };

    Ok(res)
}

pub(crate) fn process_point_operation(
    segments: &RwLock<SegmentHolder>,
    op_num: SeqNumberType,
    point_operation: PointOperations,
) -> CollectionResult<usize> {
    match point_operation {
        PointOperations::DeletePoints { ids, .. } => delete_points(&segments.read(), op_num, &ids),
        PointOperations::UpsertPoints(operation) => {
            let points: Vec<_> = match operation {
                PointInsertOperations::PointsBatch(batch) => {
                    let all_vectors = batch.vectors.into_all_vectors(batch.ids.len());
                    let vectors_iter = batch.ids.into_iter().zip(all_vectors.into_iter());
                    match batch.payloads {
                        None => vectors_iter
                            .map(|(id, vectors)| PointStruct {
                                id,
                                vector: vectors.into(),
                                payload: None,
                            })
                            .collect(),
                        Some(payloads) => vectors_iter
                            .zip(payloads.into_iter())
                            .map(|((id, vectors), payload)| PointStruct {
                                id,
                                vector: vectors.into(),
                                payload,
                            })
                            .collect(),
                    }
                }
                PointInsertOperations::PointsList(points) => points,
            };
            let res = upsert_points(&segments.read(), op_num, points.iter())?;
            Ok(res)
        }
        PointOperations::DeletePointsByFilter(filter) => {
            delete_points_by_filter(&segments.read(), op_num, &filter)
        }
        PointOperations::SyncPoints(operation) => {
            let (deleted, new, updated) = sync_points(
                &segments.read(),
                op_num,
                operation.from_id,
                operation.to_id,
                &operation.points,
            )?;
            Ok(deleted + new + updated)
        }
    }
}

pub(crate) fn process_payload_operation(
    segments: &RwLock<SegmentHolder>,
    op_num: SeqNumberType,
    payload_operation: PayloadOps,
) -> CollectionResult<usize> {
    match payload_operation {
        PayloadOps::SetPayload(sp) => {
            let payload: Payload = sp.payload;
            set_payload(&segments.read(), op_num, &payload, &sp.points)
        }
        PayloadOps::DeletePayload(dp) => {
            delete_payload(&segments.read(), op_num, &dp.points, &dp.keys)
        }
        PayloadOps::ClearPayload { ref points, .. } => {
            clear_payload(&segments.read(), op_num, points)
        }
        PayloadOps::ClearPayloadByFilter(ref filter) => {
            clear_payload_by_filter(&segments.read(), op_num, filter)
        }
    }
}

pub(crate) fn process_field_index_operation(
    segments: &RwLock<SegmentHolder>,
    op_num: SeqNumberType,
    field_index_operation: &FieldIndexOperations,
) -> CollectionResult<usize> {
    match field_index_operation {
        FieldIndexOperations::CreateIndex(index_data) => create_field_index(
            &segments.read(),
            op_num,
            &index_data.field_name,
            index_data.field_schema.as_ref(),
        ),
        FieldIndexOperations::DeleteIndex(field_name) => {
            delete_field_index(&segments.read(), op_num, field_name)
        }
    }
}

/// Deletes points from all segments matching the given filter
pub(crate) fn delete_points_by_filter(
    segments: &SegmentHolder,
    op_num: SeqNumberType,
    filter: &Filter,
) -> CollectionResult<usize> {
    let mut deleted = 0;
    segments.apply_segments(|s| {
        deleted += s.delete_filtered(op_num, filter)?;
        Ok(true)
    })?;
    Ok(deleted)
}
