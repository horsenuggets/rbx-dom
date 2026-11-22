use std::{
    borrow::Borrow,
    collections::{hash_map, HashMap, VecDeque},
    fmt,
    hash::{BuildHasher, Hash},
    iter::FromIterator,
    ops::{Deref, DerefMut},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Weak,
    },
};

use rbx_types::Variant;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use ustr::{Ustr, UstrMap};

/// An instance's complete property map, with copy-on-write ownership.
///
/// Cloning this collection shares immutable values. Mutable access detaches only this
/// collection, leaving the properties of other instances unchanged.
#[derive(Clone, Default)]
pub struct Properties(Arc<UstrMap<Variant>>, Option<PoolMembership>);

struct PoolMembership(Arc<AtomicUsize>);

impl PoolMembership {
    fn new(members: Arc<AtomicUsize>) -> Self {
        members.fetch_add(1, Ordering::Relaxed);
        Self(members)
    }
}

impl Clone for PoolMembership {
    fn clone(&self) -> Self {
        Self::new(Arc::clone(&self.0))
    }
}

impl Drop for PoolMembership {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}

impl fmt::Debug for Properties {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("Properties").field(&self.0).finish()
    }
}

impl PartialEq for Properties {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Properties {
    fn join_pool(&mut self, members: &Arc<AtomicUsize>) {
        if self
            .1
            .as_ref()
            .is_none_or(|membership| !Arc::ptr_eq(&membership.0, members))
        {
            self.1 = Some(PoolMembership::new(Arc::clone(members)));
        }
    }

    /// Returns mutable access to an existing value without copying an absent key's map.
    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut Variant>
    where
        Ustr: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        if self.0.contains_key(key) {
            self.deref_mut().get_mut(key)
        } else {
            None
        }
    }

    /// Removes an existing property without copying an absent key's map.
    pub fn remove<Q>(&mut self, key: &Q) -> Option<Variant>
    where
        Ustr: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        if self.0.contains_key(key) {
            self.deref_mut().remove(key)
        } else {
            None
        }
    }

    /// Removes an existing entry without copying an absent key's map.
    pub fn remove_entry<Q>(&mut self, key: &Q) -> Option<(Ustr, Variant)>
    where
        Ustr: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        if self.0.contains_key(key) {
            self.deref_mut().remove_entry(key)
        } else {
            None
        }
    }

    /// Consumes this collection, copying values only if another instance owns the map.
    pub fn into_inner(self) -> UstrMap<Variant> {
        Arc::unwrap_or_clone(self.0)
    }

    /// Consumes this collection and returns its owned keys.
    pub fn into_keys(self) -> hash_map::IntoKeys<Ustr, Variant> {
        self.into_inner().into_keys()
    }

    /// Consumes this collection and returns its owned values.
    pub fn into_values(self) -> hash_map::IntoValues<Ustr, Variant> {
        self.into_inner().into_values()
    }
}

impl Deref for Properties {
    type Target = UstrMap<Variant>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Properties {
    fn deref_mut(&mut self) -> &mut Self::Target {
        let membership = self.1.take();
        let values = Arc::make_mut(&mut self.0);
        drop(membership);
        values
    }
}

impl From<UstrMap<Variant>> for Properties {
    fn from(map: UstrMap<Variant>) -> Self {
        Self(Arc::new(map), None)
    }
}

impl FromIterator<(Ustr, Variant)> for Properties {
    fn from_iter<I: IntoIterator<Item = (Ustr, Variant)>>(iter: I) -> Self {
        UstrMap::from_iter(iter).into()
    }
}

impl Extend<(Ustr, Variant)> for Properties {
    fn extend<I: IntoIterator<Item = (Ustr, Variant)>>(&mut self, iter: I) {
        self.deref_mut().extend(iter);
    }
}

impl IntoIterator for Properties {
    type Item = (Ustr, Variant);
    type IntoIter = hash_map::IntoIter<Ustr, Variant>;

    fn into_iter(self) -> Self::IntoIter {
        self.into_inner().into_iter()
    }
}

impl<'a> IntoIterator for &'a Properties {
    type Item = (&'a Ustr, &'a Variant);
    type IntoIter = hash_map::Iter<'a, Ustr, Variant>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a> IntoIterator for &'a mut Properties {
    type Item = (&'a Ustr, &'a mut Variant);
    type IntoIter = hash_map::IterMut<'a, Ustr, Variant>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

impl Serialize for Properties {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Properties {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        UstrMap::deserialize(deserializer).map(Self::from)
    }
}

/// An explicitly owned, weak cache for sharing exactly identical property maps.
///
/// The pool never keeps property values alive. Use [`Self::prune_after_drop`] after
/// document destruction for ownership-aware cleanup, or [`Self::prune`] for a complete
/// sweep when the owner finishes using the pool. [`crate::WeakDom::share_properties`] also performs
/// cleanup proportional to its own batch, not to all retained documents.
#[derive(Debug, Default)]
pub struct PropertyPool {
    buckets: HashMap<u64, Vec<Weak<UstrMap<Variant>>>>,
    maintenance: VecDeque<u64>,
    members: Arc<AtomicUsize>,
}

impl PropertyPool {
    fn share(&mut self, properties: &mut Properties) {
        if properties.is_empty() {
            return;
        }
        if let Ok(bytes) = canonical_bytes(properties) {
            let hash = self.buckets.hasher().hash_one(&bytes);
            self.share_encoded(properties, hash, &bytes);
        }
    }

    fn share_encoded(&mut self, properties: &mut Properties, hash: u64, bytes: &[u8]) {
        let candidates = match self.buckets.entry(hash) {
            hash_map::Entry::Occupied(entry) => entry.into_mut(),
            hash_map::Entry::Vacant(entry) => {
                self.maintenance.push_back(hash);
                entry.insert(Vec::new())
            }
        };
        candidates.retain(|candidate| candidate.strong_count() > 0);
        for candidate in candidates.iter().filter_map(Weak::upgrade) {
            // Hash equality only selects candidates. Serialized typed values preserve
            // float bits and referents, unlike Variant's ordinary PartialEq contract.
            if Arc::ptr_eq(&candidate, &properties.0)
                || canonical_bytes(&candidate).is_ok_and(|candidate| candidate == bytes)
            {
                properties.0 = candidate;
                properties.join_pool(&self.members);
                return;
            }
        }
        properties.join_pool(&self.members);
        candidates.push(Arc::downgrade(&properties.0));
    }

    /// Releases dead cache entries after inspecting at most `budget` hash buckets.
    ///
    /// Live buckets rotate to the back, so later calls eventually inspect every entry
    /// without repeatedly scanning all retained documents for a tiny load or teardown.
    pub fn prune_some(&mut self, budget: usize) {
        for _ in 0..budget.min(self.maintenance.len()) {
            let hash = self.maintenance.pop_front().unwrap();
            let candidates = self.buckets.get_mut(&hash).unwrap();
            candidates.retain(|candidate| candidate.strong_count() > 0);
            if candidates.is_empty() {
                self.buckets.remove(&hash);
            } else {
                self.maintenance.push_back(hash);
            }
        }
    }

    /// Performs proportional cleanup while unchanged participants remain in this pool.
    ///
    /// Once all participants have been dropped, mutated or shared through another pool,
    /// performs a complete sweep instead. Call this after destruction, not after merely
    /// removing a document from an external registry. Live candidates are always preserved.
    pub fn prune_after_drop(&mut self, budget: usize) {
        if self.members.load(Ordering::Acquire) == 0 {
            self.prune();
        } else {
            self.prune_some(budget);
        }
    }

    /// Inspects every cache bucket once, releasing dead keys and weak control blocks.
    ///
    /// An empty pool also releases its backing storage. Live candidates remain cached.
    pub fn prune(&mut self) {
        self.prune_some(self.maintenance.len());
        if self.buckets.is_empty() {
            self.buckets = HashMap::new();
            self.maintenance = VecDeque::new();
        }
    }

    pub(crate) fn share_all<'a>(
        &mut self,
        properties: impl ExactSizeIterator<Item = &'a mut Properties>,
    ) -> usize {
        let work = properties.len();
        self.prune_some(work);
        for properties in properties {
            self.share(properties);
        }
        self.prune_some(work);
        work
    }
}

fn canonical_bytes(properties: &UstrMap<Variant>) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    let mut entries: Vec<_> = properties
        .iter()
        .map(|(key, value)| (key.as_str(), value))
        .collect();
    entries.sort_unstable_by_key(|(key, _)| *key);
    rmp_serde::to_vec(&entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InstanceBuilder, WeakDom};
    use rbx_types::{BinaryString, Ref};

    fn properties(value: Variant) -> Properties {
        std::iter::once((Ustr::from("Value"), value)).collect()
    }

    #[test]
    fn pool_membership_follows_clones_mutation_absent_keys_and_consumption() {
        let mut pool = PropertyPool::default();
        let mut original = properties(Variant::Int32(1));
        pool.share(&mut original);
        let mut clone = original.clone();
        pool.share(&mut clone);
        assert_eq!(pool.members.load(Ordering::Acquire), 2);
        assert!(clone.get_mut(&Ustr::from("Missing")).is_none());
        assert!(clone.remove(&Ustr::from("Missing")).is_none());
        assert!(clone.remove_entry(&Ustr::from("Missing")).is_none());
        assert_eq!(pool.members.load(Ordering::Acquire), 2);
        clone.insert(Ustr::from("Value"), Variant::Int32(2));
        assert_eq!(pool.members.load(Ordering::Acquire), 1);
        assert_eq!(original[&Ustr::from("Value")], Variant::Int32(1));
        let owned = original.into_inner();
        assert_eq!(pool.members.load(Ordering::Acquire), 0);
        assert_eq!(owned[&Ustr::from("Value")], Variant::Int32(1));
        pool.prune_after_drop(0);
        assert!(pool.buckets.is_empty());
    }

    #[test]
    fn zero_credit_cleanup_is_bounded_until_the_last_actual_participant_drops() {
        let mut pool = PropertyPool::default();
        let mut retained: Vec<_> = (0..100)
            .map(|value| properties(Variant::Int32(value)))
            .collect();
        let mut transient = properties(Variant::Int32(100));
        pool.share_all(retained.iter_mut());
        pool.share(&mut transient);
        drop(transient);
        let buckets = pool.buckets.len();
        for _ in 0..64 {
            pool.prune_after_drop(0);
            assert_eq!(pool.buckets.len(), buckets);
        }
        drop(retained);
        pool.prune_after_drop(0);
        assert!(pool.buckets.is_empty());
        assert_eq!(pool.buckets.capacity(), 0);
        assert_eq!(pool.maintenance.capacity(), 0);
    }

    #[test]
    fn concurrent_teardown_cleans_up_after_the_last_actual_destructor() {
        use std::sync::{mpsc, Mutex};
        use std::thread;

        let mut pool = PropertyPool::default();
        let batch = |start| {
            (start..start + 64)
                .map(|value| properties(Variant::Int32(value)))
                .collect::<Vec<_>>()
        };
        let mut first = batch(0);
        let mut second = batch(64);
        let mut third = batch(128);
        for (hash, map) in (0..).zip(
            first
                .iter_mut()
                .chain(second.iter_mut())
                .chain(third.iter_mut()),
        ) {
            let bytes = canonical_bytes(map).unwrap();
            pool.share_encoded(map, hash, &bytes);
        }
        pool.prune_some(64);
        let pool = Arc::new(Mutex::new(pool));
        let (first_start, first_ready) = mpsc::channel();
        let (second_start, second_ready) = mpsc::channel();
        let shared = Arc::clone(&pool);
        let first_thread = thread::spawn(move || {
            first_ready.recv().unwrap();
            drop(first);
            shared.lock().unwrap().prune_after_drop(64);
            second_start.send(()).unwrap();
        });
        let shared = Arc::clone(&pool);
        let second_thread = thread::spawn(move || {
            second_ready.recv().unwrap();
            drop(second);
            shared.lock().unwrap().prune_after_drop(64);
        });
        let shared = Arc::clone(&pool);
        let third_thread = thread::spawn(move || {
            drop(third);
            shared.lock().unwrap().prune_after_drop(64);
            first_start.send(()).unwrap();
        });
        first_thread.join().unwrap();
        second_thread.join().unwrap();
        third_thread.join().unwrap();
        let pool = pool.lock().unwrap();
        assert!(pool.buckets.is_empty());
        assert_eq!(pool.buckets.capacity(), 0);
        assert_eq!(pool.maintenance.capacity(), 0);
        assert_eq!(pool.members.load(Ordering::Acquire), 0);
    }

    #[test]
    fn sharing_through_another_pool_preserves_live_candidates_and_payloads() {
        let mut first = PropertyPool::default();
        let mut second = PropertyPool::default();
        let mut map = properties(Variant::Int32(1));
        first.share(&mut map);
        second.share(&mut map);
        assert_eq!(first.members.load(Ordering::Acquire), 0);
        assert_eq!(second.members.load(Ordering::Acquire), 1);
        first.prune_after_drop(0);
        assert_eq!(first.buckets.len(), 1);
        assert_eq!(map[&Ustr::from("Value")], Variant::Int32(1));
        drop(map);
        first.prune_after_drop(0);
        second.prune_after_drop(0);
        assert!(first.buckets.is_empty());
        assert!(second.buckets.is_empty());
    }

    #[test]
    fn complete_cleanup_releases_empty_backing_storage_and_preserves_live_maps() {
        let mut pool = PropertyPool::default();
        let mut maps: Vec<_> = (0..128)
            .map(|value| properties(Variant::Int32(value)))
            .collect();
        pool.share_all(maps.iter_mut());
        assert!(pool.buckets.capacity() > 0);
        assert!(pool.maintenance.capacity() > 0);
        let retained = maps.pop().unwrap();
        drop(maps);
        pool.prune();
        assert_eq!(pool.buckets.len(), 1);
        assert_eq!(pool.maintenance.len(), 1);
        let mut equivalent = properties(Variant::Int32(127));
        pool.share(&mut equivalent);
        assert!(Arc::ptr_eq(&equivalent.0, &retained.0));
        drop(equivalent);
        drop(retained);
        pool.prune();
        assert!(pool.buckets.is_empty());
        assert!(pool.maintenance.is_empty());
        assert_eq!(pool.buckets.capacity(), 0);
        assert_eq!(pool.maintenance.capacity(), 0);
        pool.prune();
        let mut next = properties(Variant::Int32(42));
        pool.share(&mut next);
        assert_eq!(pool.buckets.len(), 1);
        assert_eq!(pool.maintenance.len(), 1);
    }

    #[test]
    fn complete_cleanup_drains_dead_buckets_left_by_rotated_document_budgets() {
        let mut pool = PropertyPool::default();
        let batch = |start| {
            (start..start + 64)
                .map(|value| properties(Variant::Int32(value)))
                .collect::<Vec<_>>()
        };
        let mut first = batch(0);
        let mut second = batch(64);
        for (hash, map) in (0..).zip(first.iter_mut().chain(second.iter_mut())) {
            let bytes = canonical_bytes(map).unwrap();
            pool.share_encoded(map, hash, &bytes);
        }
        pool.prune_some(64);
        drop(first);
        pool.prune_some(64);
        drop(second);
        pool.prune_some(64);
        assert_eq!(pool.buckets.len(), 64);
        pool.prune();
        assert!(pool.buckets.is_empty());
        assert!(pool.maintenance.is_empty());
        assert_eq!(pool.buckets.capacity(), 0);
        assert_eq!(pool.maintenance.capacity(), 0);
    }

    #[test]
    fn maintenance_obeys_its_budget_without_timing_assumptions() {
        let mut pool = PropertyPool::default();
        let mut maps: Vec<_> = (0..32)
            .map(|value| properties(Variant::Int32(value)))
            .collect();
        assert_eq!(pool.share_all(maps.iter_mut()), maps.len());
        let buckets = pool.buckets.len();
        assert!(buckets > 1);
        assert_eq!(pool.maintenance.len(), buckets);
        drop(maps);
        pool.prune_some(0);
        assert_eq!(pool.buckets.len(), buckets);
        pool.prune_some(1);
        assert_eq!(pool.buckets.len(), buckets - 1);
        assert_eq!(pool.maintenance.len(), buckets - 1);
        pool.prune_some(usize::MAX);
        assert!(pool.buckets.is_empty());
        assert!(pool.maintenance.is_empty());
    }

    #[test]
    fn tiny_batches_bound_metadata_while_large_native_maps_stay_alive() {
        let mut pool = PropertyPool::default();
        let mut retained: Vec<_> = (0..100)
            .map(|value| properties(Variant::Int32(value)))
            .collect();
        pool.share_all(retained.iter_mut());
        for value in 100..2100 {
            let mut transient = properties(Variant::Int32(value));
            assert_eq!(pool.share_all(std::iter::once(&mut transient)), 1);
            drop(transient);
            pool.prune_some(1);
            assert!(pool.buckets.len() <= 2 * retained.len() + 1);
            assert_eq!(pool.buckets.len(), pool.maintenance.len());
        }
        drop(retained);
        pool.prune();
        assert!(pool.buckets.is_empty());
        assert!(pool.maintenance.is_empty());
    }

    #[test]
    fn consuming_map_methods_preserve_shared_owners() {
        let original = properties(Variant::String("original".into()));
        assert_eq!(
            original.clone().into_keys().collect::<Vec<_>>(),
            vec![Ustr::from("Value")]
        );
        assert_eq!(
            original.clone().into_values().collect::<Vec<_>>(),
            vec![Variant::String("original".into())]
        );
        assert_eq!(
            original[&Ustr::from("Value")],
            Variant::String("original".into())
        );
        assert_eq!(Properties::default().into_keys().count(), 0);
        assert_eq!(Properties::default().into_values().count(), 0);
    }

    #[test]
    fn composite_serializers_preserve_each_float_field_bit_for_bit() {
        use rbx_types::{
            Attributes, CFrame, Color3, ColorSequence, ColorSequenceKeypoint,
            CustomPhysicalProperties, Matrix3, NumberRange, NumberSequence, NumberSequenceKeypoint,
            PhysicalProperties, Ray, Rect, Region3, UDim, UDim2, Vector2, Vector3,
        };

        fn frame(p: &[f32]) -> CFrame {
            CFrame::new(
                Vector3::new(p[0], p[1], p[2]),
                Matrix3::new(
                    Vector3::new(p[3], p[4], p[5]),
                    Vector3::new(p[6], p[7], p[8]),
                    Vector3::new(p[9], p[10], p[11]),
                ),
            )
        }
        type FloatFixture = (&'static str, usize, fn(&[f32]) -> Variant);
        let fixtures: &[FloatFixture] = &[
            ("Vector2", 2, |p| Vector2::new(p[0], p[1]).into()),
            ("Vector3", 3, |p| Vector3::new(p[0], p[1], p[2]).into()),
            ("Color3", 3, |p| Color3::new(p[0], p[1], p[2]).into()),
            ("CFrame", 12, |p| frame(p).into()),
            ("OptionalCFrame", 12, |p| {
                Variant::OptionalCFrame(Some(frame(p)))
            }),
            ("Ray", 6, |p| {
                Ray::new(
                    Vector3::new(p[0], p[1], p[2]),
                    Vector3::new(p[3], p[4], p[5]),
                )
                .into()
            }),
            ("Rect", 4, |p| {
                Rect::new(Vector2::new(p[0], p[1]), Vector2::new(p[2], p[3])).into()
            }),
            ("Region3", 6, |p| {
                Region3::new(
                    Vector3::new(p[0], p[1], p[2]),
                    Vector3::new(p[3], p[4], p[5]),
                )
                .into()
            }),
            ("UDim", 1, |p| UDim::new(p[0], 42).into()),
            ("UDim2", 2, |p| {
                UDim2::new(UDim::new(p[0], 42), UDim::new(p[1], -42)).into()
            }),
            ("NumberRange", 2, |p| NumberRange::new(p[0], p[1]).into()),
            ("NumberSequence", 6, |p| {
                NumberSequence {
                    keypoints: vec![
                        NumberSequenceKeypoint::new(p[0], p[1], p[2]),
                        NumberSequenceKeypoint::new(p[3], p[4], p[5]),
                    ],
                }
                .into()
            }),
            ("ColorSequence", 8, |p| {
                ColorSequence {
                    keypoints: vec![
                        ColorSequenceKeypoint::new(p[0], Color3::new(p[1], p[2], p[3])),
                        ColorSequenceKeypoint::new(p[4], Color3::new(p[5], p[6], p[7])),
                    ],
                }
                .into()
            }),
            ("PhysicalProperties", 6, |p| {
                Variant::PhysicalProperties(PhysicalProperties::Custom(
                    CustomPhysicalProperties::new(p[0], p[1], p[2], p[3], p[4], p[5]),
                ))
            }),
            ("NestedAttributes", 4, |p| {
                Attributes::new()
                    .with(
                        "Outer",
                        Attributes::new().with("Scalar", p[0]).with(
                            "Sequence",
                            NumberSequence {
                                keypoints: vec![NumberSequenceKeypoint::new(p[1], p[2], p[3])],
                            },
                        ),
                    )
                    .into()
            }),
        ];
        let states = [
            0.0,
            -0.0,
            f32::from_bits(0x7fc00001),
            f32::from_bits(0x7fc00002),
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::from_bits(1),
        ];
        for &(name, count, fixture) in fixtures {
            for field in 0..count {
                let mut encodings = std::collections::HashSet::new();
                let mut owners = Vec::new();
                let mut pool = PropertyPool::default();
                for value in states {
                    let mut input = vec![1.0; count];
                    input[field] = value;
                    let mut original = properties(fixture(&input));
                    let bytes = canonical_bytes(&original).unwrap();
                    assert!(
                        encodings.insert(bytes.clone()),
                        "The \"{}\" field {} must retain distinct float bits.",
                        name,
                        field
                    );
                    let wire = rmp_serde::to_vec(&original).unwrap();
                    let mut decoded: Properties = rmp_serde::from_slice(&wire).unwrap();
                    assert_eq!(
                        canonical_bytes(&decoded).unwrap(),
                        bytes,
                        "The \"{}\" field {} must round-trip all float bits.",
                        name,
                        field
                    );
                    pool.share(&mut original);
                    pool.share(&mut decoded);
                    assert!(Arc::ptr_eq(&original.0, &decoded.0));
                    owners.push(original);
                }
                assert_eq!(owners.len(), states.len());
                assert_eq!(encodings.len(), states.len());
            }
        }
    }

    #[test]
    fn compound_identity_retains_optional_fields_nested_types_and_content_refs() {
        use rbx_types::{
            Attributes, Content, EnumItem, Font, FontStyle, FontWeight, MaterialColors,
            NetAssetRef, PhysicalProperties, SharedString, Tags, TerrainMaterials,
        };

        let regular = Font::regular("fixture");
        let mut explicit_default = MaterialColors::new();
        explicit_default.set_color(
            TerrainMaterials::Grass,
            TerrainMaterials::Grass.default_color(),
        );
        let values = vec![
            Variant::Font(regular.clone()),
            Variant::Font(Font {
                family: "other".into(),
                ..regular.clone()
            }),
            Variant::Font(Font {
                weight: FontWeight::Bold,
                ..regular.clone()
            }),
            Variant::Font(Font {
                style: FontStyle::Italic,
                ..regular.clone()
            }),
            Variant::Font(Font {
                cached_face_id: Some(String::new()),
                ..regular.clone()
            }),
            Variant::Font(Font {
                cached_face_id: Some("face".into()),
                ..regular
            }),
            Variant::Content(Content::none()),
            Variant::Content(Content::from_uri("")),
            Variant::Content(Content::from_uri("fixture")),
            Variant::Content(Content::from_referent(Ref::none())),
            Variant::Content(Content::from_referent(Ref::new())),
            Variant::Content(Content::from_referent(Ref::new())),
            Variant::EnumItem(EnumItem {
                ty: "First".into(),
                value: 0,
            }),
            Variant::EnumItem(EnumItem {
                ty: "Second".into(),
                value: 0,
            }),
            Variant::EnumItem(EnumItem {
                ty: "First".into(),
                value: 1,
            }),
            Variant::MaterialColors(MaterialColors::new()),
            Variant::MaterialColors(explicit_default),
            Variant::PhysicalProperties(PhysicalProperties::Default),
            Variant::Attributes(Attributes::new()),
            Variant::Attributes(Attributes::new().with("Value", "")),
            Variant::Attributes(
                Attributes::new().with("Value", BinaryString::from(Vec::<u8>::new())),
            ),
            Variant::SharedString(SharedString::new(Vec::new())),
            Variant::SharedString(SharedString::new(vec![0, 1, 255])),
            Variant::NetAssetRef(NetAssetRef::new(Vec::new())),
            Variant::NetAssetRef(NetAssetRef::new(vec![0, 1, 255])),
            Variant::Tags(Tags::new()),
            Variant::Tags(Tags::from(vec![String::new()])),
            Variant::Tags(Tags::from(vec![String::new(), String::new()])),
            Variant::Tags(Tags::from(vec!["first".into(), "second".into()])),
            Variant::Tags(Tags::from(vec!["second".into(), "first".into()])),
        ];
        let mut pool = PropertyPool::default();
        let mut encodings = std::collections::HashSet::new();
        let mut owners = Vec::new();
        for value in values {
            let mut original = properties(value);
            let bytes = canonical_bytes(&original).unwrap();
            assert!(encodings.insert(bytes.clone()));
            let encoded = rmp_serde::to_vec(&original).unwrap();
            let mut decoded: Properties = rmp_serde::from_slice(&encoded).unwrap();
            assert_eq!(canonical_bytes(&decoded).unwrap(), bytes);
            pool.share(&mut original);
            pool.share(&mut decoded);
            assert!(Arc::ptr_eq(&original.0, &decoded.0));
            owners.push(original);
        }
        assert_eq!(owners.len(), encodings.len());
    }

    #[test]
    fn collection_operations_preserve_other_owners() {
        let original = properties(Variant::String("original".into()));
        let mut changed = original.clone();
        let missing = Ustr::from("Missing");
        assert!(changed.get_mut(&missing).is_none());
        assert!(changed.remove(&missing).is_none());
        assert!(changed.remove_entry(&missing).is_none());
        assert!(Arc::ptr_eq(&original.0, &changed.0));
        let Variant::String(value) = changed.get_mut(&Ustr::from("Value")).unwrap() else {
            panic!("The property must keep its type.");
        };
        value.push_str(" changed");
        changed.extend([(Ustr::from("Added"), Variant::Bool(true))]);
        for (_, value) in &mut changed {
            if let Variant::Bool(value) = value {
                *value = false;
            }
        }
        assert_eq!(
            original[&Ustr::from("Value")],
            Variant::String("original".into())
        );
        assert_eq!(
            changed.remove(&Ustr::from("Added")),
            Some(Variant::Bool(false))
        );
        assert_eq!(changed.len(), 1);
        assert_eq!((&changed).into_iter().count(), 1);
        assert_eq!(changed.clone().into_iter().count(), 1);
        assert_eq!(changed.into_inner().len(), 1);
        assert_eq!(original.into_inner().len(), 1);
    }

    #[test]
    fn exact_identity_survives_forced_hash_collisions() {
        let mut pool = PropertyPool::default();
        let mut first = properties(Variant::Int32(1));
        let mut equal = properties(Variant::Int32(1));
        let mut different = properties(Variant::Int32(2));
        for map in [&mut first, &mut equal, &mut different] {
            let bytes = canonical_bytes(map).unwrap();
            pool.share_encoded(map, 0, &bytes);
        }
        assert!(Arc::ptr_eq(&first.0, &equal.0));
        assert!(!Arc::ptr_eq(&first.0, &different.0));
        assert_eq!(pool.buckets[&0].len(), 2);
        drop((first, equal, different));
        pool.prune();
        assert!(pool.buckets.is_empty());
    }

    #[test]
    fn identity_preserves_float_bits_types_keys_and_referents() {
        let values = vec![
            Variant::Float32(0.0),
            Variant::Float32(-0.0),
            Variant::Float32(f32::from_bits(0x7fc00001)),
            Variant::Float32(f32::from_bits(0x7fc00002)),
            Variant::Float64(0.0),
            Variant::Float64(-0.0),
            Variant::Float64(f64::from_bits(0x7ff8000000000001)),
            Variant::Float64(f64::from_bits(0x7ff8000000000002)),
            Variant::Int32(0),
            Variant::Int64(0),
            Variant::Ref(Ref::none()),
            Variant::Ref(Ref::new()),
            Variant::Ref(Ref::new()),
            Variant::String(String::new()),
            Variant::BinaryString(BinaryString::from(Vec::<u8>::new())),
        ];
        let maps: Vec<_> = values.into_iter().map(properties).collect();
        let bytes: Vec<_> = maps
            .iter()
            .map(|map| canonical_bytes(map).unwrap())
            .collect();
        for (index, value) in bytes.iter().enumerate() {
            assert_eq!(*value, canonical_bytes(&maps[index]).unwrap());
            for other in &bytes[index + 1..] {
                assert_ne!(value, other);
            }
        }
        assert_ne!(canonical_bytes(&UstrMap::default()).unwrap(), bytes[0]);
        let renamed: UstrMap<_> =
            std::iter::once((Ustr::from("Other"), Variant::Float32(0.0))).collect();
        assert_ne!(canonical_bytes(&renamed).unwrap(), bytes[0]);
    }

    #[test]
    fn batch_sharing_preserves_documents_and_prunes_mutated_maps() {
        let builder = || InstanceBuilder::new("Folder").with_property("Value", "original");
        let mut first = WeakDom::new(builder());
        let mut second = WeakDom::new(builder());
        let mut pool = PropertyPool::default();
        first.share_properties(&mut pool);
        second.share_properties(&mut pool);
        assert!(Arc::ptr_eq(
            &first.root().properties.0,
            &second.root().properties.0
        ));
        first
            .root_mut()
            .properties
            .insert(Ustr::from("Value"), "changed".into());
        assert_eq!(
            second.root().properties[&Ustr::from("Value")],
            Variant::String("original".into())
        );
        second
            .root_mut()
            .properties
            .insert(Ustr::from("Value"), "also changed".into());
        pool.prune();
        assert!(pool.buckets.is_empty());
        first.share_properties(&mut pool);
        second.share_properties(&mut pool);
        assert_eq!(pool.buckets.len(), 2);
        drop((first, second));
        pool.prune();
        assert!(pool.buckets.is_empty());
    }

    #[test]
    fn nested_mutation_and_key_order_preserve_exact_identity() {
        let nested = Variant::Attributes(rbx_types::Attributes::new().with(
            "Nested",
            rbx_types::Attributes::new().with("Number", -0.0_f32),
        ));
        let mut first: Properties = vec![
            (Ustr::from("A"), nested.clone()),
            (Ustr::from("B"), Variant::Bool(true)),
        ]
        .into_iter()
        .collect();
        let mut second: Properties = vec![
            (Ustr::from("B"), Variant::Bool(true)),
            (Ustr::from("A"), nested),
        ]
        .into_iter()
        .collect();
        let expected = canonical_bytes(&second).unwrap();
        let mut pool = PropertyPool::default();
        pool.share(&mut first);
        pool.share(&mut second);
        assert!(Arc::ptr_eq(&first.0, &second.0));
        let Variant::Attributes(attributes) = first.get_mut(&Ustr::from("A")).unwrap() else {
            panic!("The attribute property must keep its type.");
        };
        attributes.insert("Nested".into(), Variant::Bool(false));
        assert_eq!(canonical_bytes(&second).unwrap(), expected);
        assert_ne!(canonical_bytes(&first).unwrap(), expected);
    }

    #[test]
    fn sharing_preserves_colliding_document_refs_clone_rewrites_transfer_and_raw_access() {
        let root_ref = Ref::new();
        let child_ref = Ref::new();
        let builder = || {
            InstanceBuilder::new("Folder")
                .with_referent(root_ref)
                .with_property("Target", child_ref)
                .with_child(
                    InstanceBuilder::new("ObjectValue")
                        .with_referent(child_ref)
                        .with_property("Value", Ref::none()),
                )
        };
        let mut first = WeakDom::new(builder());
        let mut second = WeakDom::new(builder());
        let mut pool = PropertyPool::default();
        first.share_properties(&mut pool);
        second.share_properties(&mut pool);
        assert!(Arc::ptr_eq(
            &first.root().properties.0,
            &second.root().properties.0
        ));
        let mut destination = WeakDom::new(InstanceBuilder::new("Folder"));
        let cloned_root = first.clone_into_external(root_ref, &mut destination);
        let cloned_child = destination.get_by_ref(cloned_root).unwrap().children()[0];
        assert_ne!(cloned_child, child_ref);
        assert_eq!(
            destination.get_by_ref(cloned_root).unwrap().properties[&Ustr::from("Target")],
            Variant::Ref(cloned_child)
        );
        assert_eq!(
            second.root().properties[&Ustr::from("Target")],
            Variant::Ref(child_ref)
        );
        let destination_root = destination.root_ref();
        first.transfer(child_ref, &mut destination, destination_root);
        assert!(first.get_by_ref(child_ref).is_none());
        assert!(second.get_by_ref(child_ref).is_some());
        destination
            .get_by_ref_mut(child_ref)
            .unwrap()
            .properties
            .insert(Ustr::from("Value"), Variant::Ref(destination_root));
        assert_eq!(
            second.get_by_ref(child_ref).unwrap().properties[&Ustr::from("Value")],
            Variant::Ref(Ref::none())
        );
        let (root, raw) = destination.into_raw();
        let restored = WeakDom::from_raw(root, raw);
        assert_eq!(restored.get_by_ref(child_ref).unwrap().parent(), root);
        assert_eq!(
            restored.get_by_ref(child_ref).unwrap().properties[&Ustr::from("Value")],
            Variant::Ref(root)
        );
        drop((first, second, restored));
        pool.prune();
        assert!(pool.buckets.is_empty());
    }

    #[test]
    fn concurrent_document_batches_and_teardown_leave_no_owned_cache_payloads() {
        let pool = Arc::new(std::sync::Mutex::new(PropertyPool::default()));
        let workers: Vec<_> = (0..8)
            .map(|worker| {
                let pool = Arc::clone(&pool);
                std::thread::spawn(move || {
                    for _ in 0..32 {
                        let mut dom = WeakDom::new(
                            InstanceBuilder::new("Folder").with_property("Value", "original"),
                        );
                        dom.share_properties(&mut pool.lock().unwrap());
                        dom.root_mut()
                            .properties
                            .insert(Ustr::from("Value"), Variant::Int32(worker));
                        assert_eq!(
                            dom.root().properties[&Ustr::from("Value")],
                            Variant::Int32(worker)
                        );
                        drop(dom);
                        pool.lock().unwrap().prune();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let mut pool = pool.lock().unwrap();
        pool.prune();
        assert!(pool.buckets.is_empty());
    }

    #[test]
    fn property_serialization_matches_the_original_map() {
        let map: UstrMap<_> =
            std::iter::once((Ustr::from("Value"), Variant::Float32(-0.0))).collect();
        let wrapped = Properties::from(map.clone());
        let encoded = rmp_serde::to_vec(&wrapped).unwrap();
        assert_eq!(encoded, rmp_serde::to_vec(&map).unwrap());
        let decoded: Properties = rmp_serde::from_slice(&encoded).unwrap();
        assert_eq!(
            canonical_bytes(&decoded).unwrap(),
            canonical_bytes(&map).unwrap()
        );
    }
}
