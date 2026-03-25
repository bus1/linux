//! # Resource Accounting
//!
//! To safely communicate across user boundaries, bus1 needs to apply quotas to
//! pinned resources, so no user can exploit all resources of another. Every
//! bus1 operation must be performed on behalf of an actor, and every actor
//! operates on behalf of a user. Whenever an operation claims resources of a
//! foreign user, the accounting system will ensure that a given quota is not
//! exceeded.
//!
//! All actors that operate on behalf of the same user will share the same
//! limits, but a second layer quota ensures that the individual actors are
//! still semi-protected of each other (to protect against misbehaving actors
//! on the same security domain).
//!
//! Actors that operate on behalf of different users (and thus have different
//! security domains) have their interactions limited by a dynamic quota
//! system. This ensures operations that cross security domains cannot exhaust
//! the resources of another user.
//!
//! The accounting system ensures that dynamic quotas are applied and thus
//! grants all users a fair share of the available resources. The exact
//! algorithm is not part of the API guarantee, and thus a programmatic test
//! of the resource limits is not reliable or predictable. Instead, resource
//! limits are applied similar to mandatory access control: they ensure safe
//! operation, and should only ever affect unexpected or malicious operations.
//!
//! ## Atomicity
//!
//! The observable effect of resource accounting is not atomic. That is, two
//! parallel resource charges might both fail, even though each one
//! individually without the other might have passed. Moreover, an operation
//! that eventually fails might still temporarily claim (partial) resources.
//!
//! Given that the resource accounting provides no programmatic access, this
//! should not pose any issues. If underlying accounting techniques or public
//! APIs change, atomicity can be introduced if desired.

use core::mem::ManuallyDrop;

use kernel::prelude::*;
use kernel::alloc::AllocError;
use kernel::sync::{Arc, ArcBorrow, LockedBy};

use crate::capi;
use crate::util::{self, rb};

/// Representation of user IDs in the accounting system. This is guaranteed to
/// be a primitive unsigned integer big enough to hold Linux UIDs.
pub type Id = capi::b1_acct_id_t;

/// Representation of resource counters in the accounting system. This is
/// guaranteed to be a primitive unsigned integer and big enough to hold
/// values of type `usize`.
pub type Value = capi::b1_acct_value_t;

/// Number of resource slots in the accounting system. That is, it defines how
/// many different (and independent) resource types are in use and tracked by
/// the accounting system.
pub const N_SLOTS: usize = capi::_B1_ACCT_SLOT_N;

/// Errors that can be raised by a charging operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChargeError {
    /// Could not allocate required state-tracking.
    Alloc(AllocError),
    /// Charge would exceed the quota of the claiming user.
    UserQuota,
    /// Charge would exceed the quota of the claiming actor.
    ActorQuota,
}

/// An object to track how many resources a stage has available and claimed.
///
/// A [`Claim`] object is always attached to an entity that claims resources
/// (regardless whether it is a tail-node or an intermediate that re-shares
/// the resources). It tracks how many resources this entity currently has
/// claimed from its upper node, and how many it has available to its lower
/// nodes. The amount it granted to its lower nodes is the difference between
/// the two.
///
/// Tail nodes should never have resources available. They should always
/// request exactly the amount they claim, and they should always release all
/// resources back to the upper node if no longer needed.
///
/// Intermediate nodes will likely request more resources than they grant.
/// This allows guarantees to other sub-nodes about future resource claims,
/// without being required to request more from the upper nodes (which can
/// possibly fail).
///
/// The root node cannot release resource to upper nodes (there are no upper
/// nodes), and thus the available resource of the root node show exactly
/// the resources that have not been claimed by the system, yet.
struct Claim {
    available: [Value; N_SLOTS],
    claimed: [Value; N_SLOTS],
}

/// The root context of an independent accounting system.
///
/// All objects of the module are eventually tied to one [`Acct`] object.
/// Different such objects are fully independent.
///
/// The root context is used to gain access to [`User`] and [`Actor`] objects,
/// and provides the initial resource constraints of the system.
#[pin_data]
pub struct Acct {
    #[pin]
    inner: kernel::sync::Mutex<AcctInner>,
}

struct AcctInner {
    users: rb::Tree<rb::node_of!(Arc<User>, acct_rb)>,
    users_len: usize,
    maxima: [Value; N_SLOTS],
}

/// An actor is an entity of the accounting system that can lay claims on
/// resources.
///
/// Actors always operate on behalf of a user. Actors can either claim
/// resources of their own user, or of other users. In case of the latter,
/// the claim is subject to a quota.
pub struct Actor {
    user: UserRef,
}

/// Reference to a user in an accounting system.
///
/// A resource system is partitioned into independent users, which operate
/// fully independently, and each have an assigned resource limit.
///
/// This type acts like an `Arc<User>`.
///
/// An [`Actor`] can then claim resources of a user up to the limit of the
/// user. If the actor is tied to the user it claims from, then it can claim up
/// to the resource limit. If the actor is tied to another user, then its
/// resource claims are subject to a quota.
#[derive(Clone)]
pub struct UserRef {
    arc: ManuallyDrop<Arc<User>>,
}

#[pin_data]
struct User {
    acct: Arc<Acct>,
    acct_rb: rb::Node,
    id: Id,
    #[pin]
    inner: kernel::sync::Mutex<UserInner>,
}

util::field::impl_pin_field!(User, acct_rb, rb::Node);

struct UserInner {
    quotas: rb::Tree<rb::node_of!(Arc<Quota>, user_rb)>,
    quotas_len: usize,
    maxima: [Value; N_SLOTS],
    claim: Claim,
}

#[derive(Clone)]
struct QuotaRef {
    arc: ManuallyDrop<Arc<Quota>>,
}

struct Quota {
    user: UserRef,
    user_rb: rb::Node,
    id: Id,
    inner: LockedBy<QuotaInner, UserInner>,
}

// SAFETY: `user_rb` is structurally pinned and of type `Node`.
util::field::impl_pin_field!(Quota, user_rb, rb::Node);

struct QuotaInner {
    traces: rb::Tree<rb::node_of!(Arc<Trace>, quota_rb)>,
    traces_len: usize,
    claim: Claim,
}

#[derive(Clone)]
struct TraceRef {
    arc: ManuallyDrop<Arc<Trace>>,
}

struct Trace {
    quota: QuotaRef,
    quota_rb: rb::Node,
    actor: Arc<Actor>,
    inner: LockedBy<TraceInner, UserInner>,
}

// SAFETY: `quota_rb` is structurally pinned and of type `Node`.
util::field::impl_pin_field!(Trace, quota_rb, rb::Node);

struct TraceInner {
    claim: Claim,
}

/// An object to represent an active resource claim charged on the system.
#[repr(transparent)]
pub struct Charge {
    inner: capi::b1_acct_charge,
}

// Helper to convert from `usize` to `Value`.
fn value_from_usize(v: usize) -> Value {
    build_assert!(
        size_of::<Value>() >= size_of::<usize>(),
        "bit-size of values in the accounting system must exceed `usize`"
    );
    v as Value
}

// Calculate the ceiled base-2 logarithm, rounding up in case of missing
// precision.
//
// If `v` is 0, or negative, the function will produce a result, but does not
// give guarantees on its value (currently, it will produce the maximum
// logarithm representable).
fn log2_ceil(v: Value) -> Value {
    let length: u32 = Value::BITS;

    // This calculates the ceiled logarithm. What we do is count the leading
    // zeros of the value in question and then substract it from its
    // bit-length. By subtracting one from the value first we make sure the
    // value is ceiled.
    //
    // Hint: To calculate the floored value, you would do the subtraction of 1
    //       from the final value, rather than from the source value:
    //
    //           `length - v.leading_zeros() - 1`
    let log2: u32 = length - (v - 1).leading_zeros();

    // Convert the value back into the source type. Since the base-2 logarithm
    // only reduced in value, this cannot fail, even if the backing type is
    // changed.
    log2.into()
}

// A resource allocator that provides exponential allocation guarantees. It
// ensures resource reserves are freely accessible, at the expense of granting
// only very limited guarantees to each entity.
//
// This allocator grants access to half of the remaining resources for every
// new allocation. As such, this allocator guarantees that each independent
// entity gets access to 1 over `2^n` of the total resources.
fn allocator_exponential(_users: Value) -> Option<Value> {
    Some(2)
}

// A resource allocator that provides polynomial allocation guarantees. It
// ensures resource reserves are easily accessible, at the expense of granting
// only limited guarantees to each entity.
//
// This allocator grants access to 1 over `n + 1` of the remaining resources
// for every new allocation (with `n` being the number of active entities). As
// such, this allocator guarantees that each independent entity gets access to
// 1 over `n^2` of the total resources.
#[allow(unused)]
fn allocator_polynomial(users: Value) -> Option<Value> {
    users.checked_add(1)
}

// A resource allocator that provides quasilinear allocation guarantees. It
// ensures strong guarantees to each entity, at the expense of heavily
// restricting resource reserves.
//
// This allocator grants access to 1 over `(n+1) log(n+1) + n+1` of the
// remaining resources for every new allocation (with `n` being the number of
// active entities). As such, this allocator guarantees that each independent
// entity gets access to 1 over `n log(n)^2` of the total resources.
fn allocator_quasilinear(users: Value) -> Option<Value> {
    let users1 = users.checked_add(1)?;
    let log_mul = log2_ceil(users1).checked_mul(users1)?;
    log_mul.checked_add(users1)
}

// Calculate the minimum reserve size required for an allocation request to
// pass the quota.
//
// Whenever an allocation request is checked against the quota, this function
// can be used to calculate how many resources must still be available in the
// reserve to allow the request. If this minimum reserve size matches the size
// of the request, the allocation would be allowed to consume all remaining
// resources. In most cases, this function returns a bigger minimum reserve
// size, to ensure future requests can be served as well.
//
// This function implements an algorithm to allow an unknown set of users to
// fairly share a fixed pool of resources. It considers the changing parameters
// and adjusts its quota check accordingly, ensuring that a growing or
// shrinking set of users get arbitrated access to the available resources
// in a fair manner.
//
// The quota-check is applied whenever a user requests resources from the
// shared resource pool. The following information is involved in checking a
// quota:
//
// * `remaining`: Amount of resources that are available for allocation
// * `n_users`: Number of users that are tracked, including the claimant user
// * `share`: Amount of resources the claimant user has already allocated
// * `charge`: Amount of resources that are requested by the claimant user
//
// ## Algorithm
//
// Ideally, every user on the system would get `1 / n_users` of the available
// resources. This would be a fair system, where everyone gets the same share.
// Unfortunately, we do not know the number of users that will participate
// upfront. Hence, we use an algorithm that guarantees something that comes
// close to this ideal.
//
// An allocation that was granted is never revoked, nor do we reclaim any
// resources that are actively used. Hence, the only place the algorithm is
// applied is when an allocation is requested. There, we look at the amount
// of resources that are still available, and then decide whether the user
// is allowed their request. We consider every allocation of a user as a
// `re-allocation` of their current share. That is, we pretend they release
// their currently held resources, then request a new allocation that is the
// size of the original request plus their previous share. This `re-allocation`
// then has to satisfy the following inequality:
//
// ```txt
//                       remaining + share
//     charge + share <= ~~~~~~~~~~~~~~~~~
//                           A(n_users)
// ```
//
// In other words, of the remaining resources, a user gets a fraction that
// only depends on the number of users that are active. Now depending on which
// function `A()` is chosen, a user can request more or less resources.
// However, selection of `A()` also affects the overall guarantees that the
// algorithm will provide.
//
// For example, consider `A(n): 2`. That is, an allocator that always grants
// half of the remaining resources to a user:
//
// ```txt
//                       remaining + share
//     charge + share <= ~~~~~~~~~~~~~~~~~
//                               2
// ```
//
// This will ensure that resources are easily available and little reserves
// are kept. However, for a single user, this allocation scheme can only
// guarantee that each user gets `1 / 2^n` of the available resources. So
// while it ensures that resources are easily available and not held back,
// it also prevents any meaningful guarantess for a single user, and as such
// denials of service can ensue.
//
// If, on the other hand, you pick `A(n): n + 1`, then only a share
// proportional to the number of currently active users is granted:
//
// ```txt
//                       remaining + share
//     charge + share <= ~~~~~~~~~~~~~~~~~
//                          n_users + 1
// ```
//
// (Note that `n_users` is not the total numbers of users involved in the
// system eventually, but merely at the given moment).
//
// With this allocator, much bigger reserves are kept as the number of users
// rises. Ultimately, this will guarantee that each users gets `1 / n^2` of
// the total resources. This is already much better than the exponential
// backoff of the previous allocator.
//
// Now, lastly, we consider `A(n): n log(n) + n`, or more precicesly
// `A(n): (n+1) log(n+1) + n+1`. With this allocator, resources are kept
// even tighter, but ultimately we get a quasilinear guarantee for each user
// with `1 / (n * log(n)^2)`. This is already pretty close to the ideal of
// `1 / n`.
//
// ## Hierarchy
//
// The algorithm can be applied to a hierarchical setup by simply chaining
// quota checks. For instance, one could first check whether a user can
// allocate resources from a global resource pool, and then check whether a
// claimant can allocate resources on that user. Depending on what guarantees
// are desired on each level, different allocators can be chosen. However,
// any hierarchical setup will also significantly reduce the guarantees, as
// each level operates only on the guarantees of the previous level.
fn quota_reserve<F>(
    allocator_fn: F,
    n_users: Value,
    share: Value,
    charge: Value,
) -> Option<Value>
where
    F: Fn(Value) -> Option<Value>,
{
    // For a quota check, we have to calculate:
    //
    //                       remaining + share
    //     charge + share <= ~~~~~~~~~~~~~~~~~
    //                           A(n_users)
    //
    // But to avoid the division, we instead calculate:
    //
    //     (charge + share) * A(n_users) - share <= remaining
    //
    // The inequality itself has to be checked by the caller. This function
    // merely computes the left half of the inequality and returns it.
    //
    // None of these partial calculations exceed the actual limit by a factor
    // of 2, and as such we expect all calculations to be possible within the
    // limits of the integer type. Any overflow will thus result in a quota
    // rejection.

    let allocator = allocator_fn(n_users)?;
    let charge_share = charge.checked_add(share)?;
    let limit = charge_share.checked_mul(allocator)?;
    let minimum = limit.checked_sub(share)?;

    Some(minimum)
}

/// Find an entry in a lookup tree, or insert a new one if not found.
///
/// This will use `cmp_fn` to look for an entry in `tree`. If found, the entry
/// is returned. Otherwise, `new_fn` is used to create a new entry and store
/// it in `tree` at the same position.
///
/// To ensure tree order, the caller should make sure the newly created entry
/// is consistent with the comparator `cmp_fn`.
///
/// ## Safety
///
/// When inserting a new entry, this splits a single reference count across 2
/// `Arc`s. One that is returned to the caller, and one that is stored in the
/// lookup tree. The caller must ensure to use `drop_or_unlink()` when dropping
/// the last reference, or otherwise merge those `Arc`s back together.
unsafe fn find_or_insert<T, Lrt, CmpFn, NewFn>(
    tree: Pin<&mut rb::Tree<Lrt>>,
    tree_len: &mut usize,
    cmp_fn: CmpFn,
    mut new_fn: NewFn,
) -> Result<Arc<T>, AllocError>
where
    Lrt: util::intrusive::Link<rb::Node, Ref = Arc<T>, Target = T>,
    CmpFn: FnMut(Pin<&T>) -> core::cmp::Ordering,
    NewFn: FnMut() -> Result<Arc<T>, AllocError>,
{
    let slot = tree.find_slot_by(cmp_fn);
    if let Some(v) = slot.entry_clone() {
        Ok(util::arc_unpin(v))
    } else {
        let raw = Arc::into_raw(new_fn()?);
        // SAFETY: The single reference-count is split here. `drop_or_unlink()`
        //     must be used by the caller to ensure it is merged when dropped.
        let (new, link) = unsafe {
            (Arc::from_raw(raw), Arc::from_raw(raw))
        };
        slot.link(util::arc_pin(link));
        *tree_len += 1;
        Ok(new)
    }
}

/// Unlink an `Arc` from a tree, if it is the last.
///
/// This tries to drop an `Arc`, but only if it is not the last `Arc`. If
/// the drop goes through, `None` is returned. If not, the `Arc` is unlinked
/// from `tree` and returned to the caller.
///
/// The `Arc` stored in the tree is leaked via `Arc::into_raw()`. To prevent
/// this leak, the caller should have stored a shared reference in the tree
/// in the first place.
fn drop_or_unlink<T, Lrt>(
    tree: Pin<&mut rb::Tree<Lrt>>,
    tree_len: &mut usize,
    ent: Arc<T>,
) -> Option<Arc<T>>
where
    Lrt: util::intrusive::Link<rb::Node, Ref = Arc<T>, Target = T>,
{
    let ent = util::arc_pin(Arc::drop_unless_unique(ent)?);

    if let Some(cur) = tree.try_claim_mut(ent.as_ref()) {
        if let Some(v) = cur.try_unlink() {
            let _v = Arc::into_raw(util::arc_unpin(v));
            *tree_len -= 1;
        }
    }

    Some(util::arc_unpin(ent))
}

impl core::convert::From<AllocError> for ChargeError {
    /// Create a charge error from an allocation error.
    ///
    /// This will wrap the allocation error as a [`ChargeError::Alloc`].
    fn from(v: AllocError) -> Self {
        Self::Alloc(v)
    }
}

impl Claim {
    fn with(available: &[Value; N_SLOTS]) -> Self {
        Claim {
            available: *available,
            claimed: *available,
        }
    }

    fn new() -> Self {
        Self::with(&[0; N_SLOTS])
    }
}

impl Acct {
    /// Create a new accounting system.
    ///
    /// All accounting systems are fully independent of each other, and this
    /// object serves as root context for a given accounting system.
    pub fn new(
        maxima: &[Value; N_SLOTS],
    ) -> Result<Arc<Self>, AllocError> {
        match Arc::pin_init(
            pin_init!(Self {
                inner <- kernel::sync::new_mutex!(
                    AcctInner {
                        users: Default::default(),
                        users_len: 0,
                        maxima: *maxima,
                    },
                ),
            }),
            GFP_KERNEL,
        ) {
            Ok(v) => Ok(v),
            Err(_) => Err(AllocError),
        }
    }

    /// Turn the reference into a raw pointer.
    ///
    /// This will leak the reference and any pinned resources, unless the
    /// original object is recreated via `Self::from_raw()`.
    fn into_raw(this: Arc<Self>) -> *mut capi::b1_acct {
        Arc::into_raw(this).cast_mut().cast()
    }

    /// Recreate the reference from its raw pointer.
    ///
    /// ## Safety
    ///
    /// The caller must guarantee this pointer was acquired via
    /// `Self::into_raw()`, and they must refrain from using the pointer any
    /// further.
    unsafe fn from_raw(this: *mut capi::b1_acct) -> Arc<Self> {
        // SAFETY: Delegated to caller.
        unsafe { Arc::from_raw(this.cast::<Self>()) }
    }

    /// Get a user object for a given user ID.
    ///
    /// Query the accounting system for the user object of the given user ID.
    /// This will always return an existing user object, if there is one.
    /// Otherwise, it will create a new one.
    ///
    /// It is never possible to have two [`User`] objects with the same user ID
    /// but assigned to the same [`Acct`] object. This lookup function ensures
    /// that user objects are always shared.
    pub fn get_user(
        self: ArcBorrow<'_, Acct>,
        id: Id,
    ) -> Result<UserRef, AllocError> {
        let mut acct_guard = self.inner.lock();
        let (users, users_len, maxima) = acct_guard.as_mut().unfold_mut();

        // SAFETY: The new `Arc` is immediately wrapped in `UserRef`, which
        //     ensures to merge back the split `Arc` of `find_or_insert()`
        //     on drop.
        let user = unsafe {
            find_or_insert(
                users,
                users_len,
                |ent| id.cmp(&ent.id),
                || User::new(self, id, maxima),
            )?
        };

        Ok(UserRef::new(user))
    }
}

impl AcctInner {
    #[allow(clippy::type_complexity)]
    fn unfold_mut(
        self: Pin<&mut Self>,
    ) -> (
        Pin<&mut rb::Tree<rb::node_of!(Arc<User>, acct_rb)>>,
        &mut usize,
        &mut [Value; N_SLOTS],
    ) {
        // SAFETY: Only `AcctInner.users` is structurally pinned.
        unsafe {
            let inner = Pin::into_inner_unchecked(self);
            (
                Pin::new_unchecked(&mut inner.users),
                &mut inner.users_len,
                &mut inner.maxima,
            )
        }
    }
}

impl Actor {
    /// Create a new actor assigned to the given user.
    ///
    /// Any amount of actors can be created for a given user, and they can be
    /// shared between different operations, if desired. Actors always operate
    /// on behalf of the user they were created for. As such, actors of
    /// differnt users never share any quota. Additionally, multiple actors
    /// assigned to the same user will be protected via a second-level quota.
    ///
    /// Long story short: Resource use of each actor can be safely traced and
    /// is tracked by the resource accounting.
    pub fn with(
        user: UserRef,
    ) -> Result<Arc<Self>, AllocError> {
        Arc::new(
            Self {
                user,
            },
            GFP_KERNEL,
        )
    }

    /// Turn the reference into a raw pointer.
    ///
    /// This will leak the reference and any pinned resources, unless the
    /// original object is recreated via `Self::from_raw()`.
    fn into_raw(this: Arc<Self>) -> *mut capi::b1_acct_actor {
        Arc::into_raw(this).cast_mut().cast()
    }

    /// Recreate the reference from its raw pointer.
    ///
    /// ## Safety
    ///
    /// The caller must guarantee this pointer was acquired via
    /// `Self::into_raw()`, and they must refrain from using the pointer any
    /// further.
    unsafe fn from_raw(this: *mut capi::b1_acct_actor) -> Arc<Self> {
        // SAFETY: Delegated to caller.
        unsafe { Arc::from_raw(this.cast::<Self>()) }
    }

    // Return the memory address of the actor as integer.
    //
    // The same value can be obtained via `(&raw const *actor).addr()`. Note
    // that this address is stable for the lifetime of an `Arc<Actor>`, since
    // kernel `Arc` is always pinned.
    fn addr(self: ArcBorrow<'_, Self>) -> usize {
        // In the kernel, `Arc` is always pinned, so the address can be used as
        // stable indicator for this actor.
        util::ptr_addr(core::ptr::from_ref(&*self))
    }

    /// Charge resources on the user of this actor with the given claimant
    /// actor.
    ///
    /// See [`User::charge()`] for details on the charge operation.
    pub fn charge(
        self: ArcBorrow<'_, Self>,
        claimant: ArcBorrow<'_, Self>,
        amount: &[Value; N_SLOTS],
    ) -> Result<Charge, ChargeError> {
        self.user.charge(
            claimant,
            amount,
        )
    }
}

impl UserRef {
    fn new(arc: Arc<User>) -> Self {
        Self {
            arc: ManuallyDrop::new(arc),
        }
    }

    /// Turn the reference into a raw pointer.
    ///
    /// This will leak the reference and any pinned resources, unless the
    /// original object is recreated via `Self::from_raw()`.
    fn into_raw(mut this: Self) -> *mut capi::b1_acct_user {
        // SAFETY: The drop-handler is skipped if we leak the value, which
        //     we do here. We also do not expose access to the refcount, so
        //     either the value is leaked, or recreated via `Self::from_raw()`.
        let arc = unsafe { ManuallyDrop::take(&mut this.arc) };
        core::mem::forget(this);
        Arc::into_raw(arc).cast_mut().cast()
    }

    /// Recreate the reference from its raw pointer.
    ///
    /// ## Safety
    ///
    /// The caller must guarantee this pointer was acquired via
    /// `Self::into_raw()`, and they must refrain from using the pointer any
    /// further.
    unsafe fn from_raw(this: *mut capi::b1_acct_user) -> Self {
        // SAFETY: Delegated to caller.
        Self::new(unsafe { Arc::from_raw(this.cast()) })
    }

    fn charge_claims(
        user_inner: Pin<&mut UserInner>,
        quota_inner: Pin<&mut QuotaInner>,
        trace_inner: Pin<&mut TraceInner>,
        amount: &[Value; N_SLOTS],
        cross_user: bool,
    ) -> Result<(), ChargeError> {
        //
        // Fetch all relevant information needed for the quota operation.
        // This simplifies the accessors and avoid dealing with pinning in
        // the quota calculations.
        //

        let n_users = value_from_usize(user_inner.as_ref().quotas_len);
        let n_actors = value_from_usize(quota_inner.as_ref().traces_len);
        // SAFETY: `UserInner.claim` is not structurally pinned.
        let claim_user = unsafe { &mut Pin::into_inner_unchecked(user_inner).claim };
        // SAFETY: `QuotaInner.claim` is not structurally pinned.
        let claim_quota = unsafe { &mut Pin::into_inner_unchecked(quota_inner).claim };
        // SAFETY: `TraceInner.claim` is not structurally pinned.
        let claim_trace = unsafe { &mut Pin::into_inner_unchecked(trace_inner).claim };

        // Remember the charge amount for each level / claim-object.
        let mut reqs = [[0; 3]; N_SLOTS];

        // First check each slot independently, but apply nothing.
        for (slot, req) in reqs.iter_mut().enumerate() {
            let mut minimum;

            req[0] = amount[slot];

            // Direct allocation
            //
            // We start the allocation request on the trace object, and
            // work our way upwards for as long as the request was not
            // fulfilled.
            //
            // A trace object is a leaf node in the allocation tree, and as
            // such cannot have any overallocated resources. Verify this!
            // Then delegate the request to the next level.

            assert_eq!(claim_trace.available[slot], 0);
            req[1] = req[0];

            // Trace allocation
            //
            // Since the trace object cannot serve the request, we delegate
            // the allocation and request more resources for this trace
            // object from the user. This uses a very lenient allocator,
            // since no user boundaries are crossed, but we merely share
            // resources across actors of the same user.
            //
            // We first calculate how big the reserve of the user has to be
            // to serve the request, and then check whether the user
            // already has enough resources available. If not, we delegate
            // the allocation request to the next level.
            //
            // If trace boundaries are not crossed, we grant full access to
            // all resources.

            minimum = quota_reserve(
                allocator_exponential,
                n_actors,
                claim_trace.claimed[slot],
                req[1],
            ).ok_or(ChargeError::ActorQuota)?;

            if claim_quota.available[slot] >= minimum {
                continue;
            }

            req[2] = minimum - claim_quota.available[slot];

            // User allocation
            //
            // The reserve of the user was not big enough to serve the
            // request, so we have to request more resources for this user.
            // If this crosses user-boundaries, we have to ensure that a
            // strong allocator is used. But if this does not cross
            // user-boundaries, we grant full access to all resources of
            // the user.

            minimum = if cross_user {
                quota_reserve(
                    allocator_quasilinear,
                    n_users,
                    claim_quota.claimed[slot],
                    req[2],
                ).ok_or(ChargeError::UserQuota)?
            } else {
                req[2]
            };

            if claim_user.available[slot] >= minimum {
                continue;
            }

            // Root allocation
            //
            // The resources of the user were exhausted. We do not support
            // further propagation, but only provide per-user limits.
            // Hence, we have to fail the request.

            return Err(ChargeError::UserQuota);
        }

        // With all quotas checked, apply the charge to each slot.
        for (slot, req) in reqs.iter().enumerate() {
            claim_user.available[slot] -= req[2];
            claim_quota.claimed[slot] += req[2];
            claim_quota.available[slot] += req[2];

            claim_quota.available[slot] -= req[1];
            claim_trace.claimed[slot] += req[1];
            claim_trace.available[slot] += req[1];

            claim_trace.available[slot] -= req[0];
        }

        Ok(())
    }

    /// Charge resources on this user with the given actor.
    pub fn charge(
        &self,
        claimant: ArcBorrow<'_, Actor>,
        amount: &[Value; N_SLOTS],
    ) -> Result<Charge, ChargeError> {
        // First we lock the relevant user. This lock is used for all quota
        // and trace lookups underneath a single user.
        //
        // Then try to resolve the quota and trace objects, and pin their inner
        // objects. We then pass those pinned objects to `charge_claims()`,
        // which performs the actual quota calculations.
        //
        // This might allocates a new quota or trace object. And in case the
        // quota-check fails, those can then trigger their drop handler. Ensure
        // that both `quota` and `trace` are declared before `user_guard`, to
        // prevent them from being dropped with the lock held.
        let quota: QuotaRef;
        let trace: TraceRef;
        let mut user_guard = self.arc.inner.lock();
        let mut user_inner = user_guard.as_mut();

        // SAFETY: `user_inner` and `self` refer to the same object.
        quota = unsafe {
            user_inner.as_mut().get_quota(self, claimant.user.arc.id)?
        };
        // SAFETY: `Quota.inner` is structurally pinned and `user_inner` is
        //     held sufficiently long. `access_mut_unchecked()` is only called
        //     once on this object.
        let mut quota_inner = unsafe {
            Pin::new_unchecked(
                quota.arc.inner.access_mut_unchecked(
                    Pin::into_inner_unchecked(user_inner.as_mut()),
                )
            )
        };
        // SAFETY: `quota_inner` and `quota` refer to the same object.
        trace = unsafe {
            quota_inner.as_mut().get_trace(&quota, claimant)?
        };
        // SAFETY: `Trace.inner` is structurally pinned and `user_inner` is
        //     held sufficiently long. `access_mut_unchecked()` is only called
        //     once on this object.
        let trace_inner = unsafe {
            Pin::new_unchecked(
                trace.arc.inner.access_mut_unchecked(
                    Pin::into_inner_unchecked(user_inner.as_mut()),
                )
            )
        };

        match Self::charge_claims(
            user_inner,
            quota_inner,
            trace_inner,
            amount,
            quota.cross_user(),
        ) {
            Ok(()) => Ok(Charge::with(trace, amount)),
            Err(e) => Err(e),
        }

    }
}

impl core::ops::Drop for UserRef {
    fn drop(&mut self) {
        // SAFETY: The value is always valid, and only cleared here.
        let this = unsafe { ManuallyDrop::take(&mut self.arc) };

        // Try dropping the reference. But if this is the last reference,
        // take the lock first and retry. If it is still the last, unlink
        // from the lookup tree and then drop it.
        if let Some(this) = Arc::drop_unless_unique(this) {
            let acct = this.acct.clone();
            let mut acct_guard = acct.inner.lock();
            let (users, users_len, _) = acct_guard.as_mut().unfold_mut();
            let this = drop_or_unlink(users, users_len, this);
            drop(acct_guard);
            drop(this); // Drop outside of the lock to reduce contention.
        }
    }
}

impl User {
    fn new(
        acct: ArcBorrow<'_, Acct>,
        id: Id,
        maxima: &[Value; N_SLOTS],
    ) -> Result<Arc<Self>, AllocError> {
        match Arc::pin_init(
            pin_init!(Self {
                acct: acct.into(),
                acct_rb: Default::default(),
                id: id,
                inner <- kernel::sync::new_mutex!(
                    UserInner {
                        quotas: Default::default(),
                        quotas_len: 0,
                        maxima: *maxima,
                        claim: Claim::with(maxima),
                    },
                ),
            }),
            GFP_KERNEL,
        ) {
            Ok(v) => Ok(v),
            Err(_) => Err(AllocError),
        }
    }
}

impl UserInner {
    #[allow(clippy::type_complexity)]
    fn unfold_mut(
        self: Pin<&mut Self>,
    ) -> (
        Pin<&mut rb::Tree<rb::node_of!(Arc<Quota>, user_rb)>>,
        &mut usize,
        &mut [Value; N_SLOTS],
        &mut Claim,
    ) {
        // SAFETY: Only `UserInner.quotas` is structurally pinned.
        unsafe {
            let inner = Pin::into_inner_unchecked(self);
            (
                Pin::new_unchecked(&mut inner.quotas),
                &mut inner.quotas_len,
                &mut inner.maxima,
                &mut inner.claim,
            )
        }
    }

    /// Find a quota object, or create a new one.
    ///
    /// ## Safety
    ///
    /// `self_ref` and `self` must refer to the same `User` object.
    unsafe fn get_quota(
        self: Pin<&mut Self>,
        self_ref: &UserRef,
        id: Id,
    ) -> Result<QuotaRef, AllocError> {
        let (quotas, quotas_len, _, _) = self.unfold_mut();

        // SAFETY: The new `Arc` is immediately wrapped in `QuotaRef`, which
        //     ensures to merge back the split `Arc` of `find_or_insert()`
        //     on drop.
        //     Caller guarantees `self_ref` refers to the correct user.
        let quota = unsafe {
            find_or_insert(
                quotas,
                quotas_len,
                |ent| id.cmp(&ent.id),
                || Quota::new(self_ref.clone(), id),
            )?
        };

        Ok(QuotaRef::new(quota))
    }
}

impl QuotaRef {
    fn new(arc: Arc<Quota>) -> Self {
        Self {
            arc: ManuallyDrop::new(arc),
        }
    }

    fn cross_user(&self) -> bool {
        self.arc.id != self.arc.user.arc.id
    }
}

impl core::ops::Drop for QuotaRef {
    fn drop(&mut self) {
        // SAFETY: The value is always valid, and only cleared here.
        let this = unsafe { ManuallyDrop::take(&mut self.arc) };

        // Try dropping the reference. But if this is the last reference,
        // take the lock first and retry. If it is still the last, unlink
        // from the lookup tree and then drop it.
        if let Some(this) = Arc::drop_unless_unique(this) {
            let user = this.user.clone();
            let mut user_guard = user.arc.inner.lock();
            let (quotas, quotas_len, _, _) = user_guard.as_mut().unfold_mut();
            let this = drop_or_unlink(quotas, quotas_len, this);
            drop(user_guard);
            drop(this); // Drop outside of the lock to reduce contention.
        }
    }
}

impl Quota {
    fn new(
        user: UserRef,
        id: Id,
    ) -> Result<Arc<Self>, AllocError> {
        let inner = LockedBy::new(
            &user.arc.inner,
            QuotaInner {
                traces: Default::default(),
                traces_len: 0,
                claim: Claim::new(),
            },
        );
        Arc::new(
            Self {
                user,
                user_rb: Default::default(),
                id,
                inner,
            },
            GFP_KERNEL,
        )
    }
}

impl QuotaInner {
    /// Find a trace object, or create a new one.
    ///
    /// ## Safety
    ///
    /// `self_ref` and `self` must refer to the same `Quota` object.
    unsafe fn get_trace(
        self: Pin<&mut Self>,
        self_ref: &QuotaRef,
        actor: ArcBorrow<'_, Actor>,
    ) -> Result<TraceRef, AllocError> {
        let actor_addr = actor.addr();

        // SAFETY: `Quota::inner` and `QuotaInner::traces` are
        //     structurally pinned.
        let (traces, traces_len) = unsafe {
            let inner = Pin::into_inner_unchecked(self);
            (
                Pin::new_unchecked(&mut inner.traces),
                &mut inner.traces_len,
            )
        };

        // SAFETY: The new `Arc` is immediately wrapped in `QuotaRef`, which
        //     ensures to merge back the split `Arc` of `find_or_insert()`
        //     on drop.
        //     Caller guarantees `self_ref` refers to the correct quota.
        let trace = unsafe {
            find_or_insert(
                traces,
                traces_len,
                |ent| actor_addr.cmp(&ent.actor.as_arc_borrow().addr()),
                || Trace::new(self_ref.clone(), actor.into()),
            )?
        };

        Ok(TraceRef::new(trace))
    }
}

impl TraceRef {
    fn new(arc: Arc<Trace>) -> Self {
        Self {
            arc: ManuallyDrop::new(arc),
        }
    }

    /// Turn the reference into a raw pointer.
    ///
    /// This will leak the reference and any pinned resources, unless the
    /// original object is recreated via `Self::from_raw()`.
    fn into_raw(mut this: Self) -> *mut capi::b1_acct_trace {
        // SAFETY: The drop-handler can be skipped if we leak the value, which
        //     we do here. We also do not expose access to the refcount, so
        //     either the value is leaked, or recreated via `Self::from_raw()`.
        let arc = unsafe { ManuallyDrop::take(&mut this.arc) };
        core::mem::forget(this);
        Arc::into_raw(arc).cast_mut().cast()
    }

    /// Recreate the reference from its raw pointer.
    ///
    /// ## Safety
    ///
    /// The caller must guarantee this pointer was acquired via
    /// `Self::into_raw()`, and they must refrain from using the pointer any
    /// further.
    unsafe fn from_raw(trace: *mut capi::b1_acct_trace) -> Self {
        // SAFETY: Delegated to caller.
        Self::new(unsafe { Arc::from_raw(trace.cast()) })
    }

    fn discharge(&self, amount: &[Value; N_SLOTS]) {
        //
        // First we lock the owning user. The entire chain from `self` to
        // `self.arc.quota.arc.user` is protected by the user lock.
        //
        // Put the remaining code into its own block to ensure any result of
        // `LockedBy::access_mut_unchecked()` is released before `user_guard`.
        //

        let mut user_guard = self.arc.quota.arc.user.arc.inner.lock();
        let mut user_inner = user_guard.as_mut();

        {
            //
            // With the user locked, find or create the quota and trace objects
            // for the claiming actor.
            //

            // SAFETY: `Quota.inner` is structurally pinned.
            let quota_inner = unsafe {
                Pin::new_unchecked(
                    self.arc.quota.arc.inner.access_mut_unchecked(
                        Pin::into_inner_unchecked(user_inner.as_mut()),
                    )
                )
            };
            // SAFETY: `Trace.inner` is structurally pinned.
            let trace_inner = unsafe {
                Pin::new_unchecked(
                    self.arc.inner.access_mut_unchecked(
                        Pin::into_inner_unchecked(user_inner.as_mut()),
                    )
                )
            };

            //
            // Fetch all relevant information needed for the quota operation.
            // This simplifies the accessors and avoid dealing with pinning in
            // the quota calculations.
            //

            let n_actors = value_from_usize(quota_inner.as_ref().traces_len);
            // SAFETY: `UserInner.claim` is not structurally pinned.
            let claim_user = unsafe { &mut Pin::into_inner_unchecked(user_inner).claim };
            // SAFETY: `QuotaInner.claim` is not structurally pinned.
            let claim_quota = unsafe { &mut Pin::into_inner_unchecked(quota_inner).claim };
            // SAFETY: `TraceInner.claim` is not structurally pinned.
            let claim_trace = unsafe { &mut Pin::into_inner_unchecked(trace_inner).claim };

            //
            // Everything is prepared. Discharge each slot individually. This
            // will drop the specified amount from the charge slot of the actor
            // and then propagate resources back through the trace and quota to
            // the user.
            //

            for (slot, amount_slot) in amount.iter().enumerate() {
                let mut n;

                // Release the claimed amount on the trace-object and make it
                // available as cached resources. We do this for completeness
                // reasons, but we immediately propagate the resources in the
                // next step.

                n = *amount_slot;

                claim_trace.available[slot] += n;

                // Release all unused resources on the trace object and grant
                // them back to the quota. We do not want to cache any
                // resources on the trace object, yet, but always ensure
                // everything is properly returned to the lower levels.

                n = claim_trace.available[slot];

                claim_trace.available[slot] -= n;
                claim_trace.claimed[slot] -= n;
                claim_quota.available[slot] += n;

                // We now want to release unused resources on the quota object
                // back to the user. We cannot release all unused resources,
                // since other charges might require reserves. Hence, we
                // re-calculate how big the reserve shall be and then shrink
                // down to this size.
                //
                // To figure out how big the reserve needs to be, we calculate
                // the required reserve if a new user came along and requested
                // the average size of all allocated resources (i.e., the ideal
                // 1/n partition). We then use this as minimum reserve. This
                // also ensures that if the average is 0, the minimum reserve
                // will also be 0.

                let claimed = claim_quota.claimed[slot];
                let available = claim_quota.available[slot];

                let average = claimed
                    .checked_sub(available)
                    .unwrap()
                    .checked_div(n_actors)
                    .unwrap();

                let minimum = quota_reserve(
                    allocator_exponential,
                    n_actors,
                    average,
                    0,
                ).unwrap_or(Value::MAX);

                if minimum < claim_quota.available[slot] {
                    n = claim_quota.available[slot] - minimum;
                    claim_quota.available[slot] -= n;
                    claim_quota.claimed[slot] -= n;
                    claim_user.available[slot] += n;
                }
            }
        }
    }
}

impl core::ops::Drop for TraceRef {
    fn drop(&mut self) {
        // SAFETY: The value is always valid, and only cleared here.
        let this = unsafe { ManuallyDrop::take(&mut self.arc) };

        // Try dropping the reference. But if this is the last reference,
        // take the lock first and retry. If it is still the last, unlink
        // from the lookup tree and then drop it.
        if let Some(this) = Arc::drop_unless_unique(this) {
            let quota = this.quota.clone();
            let mut user_guard = quota.arc.user.arc.inner.lock();
            // SAFETY: `Quota::inner` and `QuotaInner::traces` are
            //     structurally pinned.
            let (traces, traces_len) = unsafe {
                let inner = quota.arc.inner.access_mut(
                    Pin::into_inner_unchecked(user_guard.as_mut())
                );
                (
                    Pin::new_unchecked(&mut inner.traces),
                    &mut inner.traces_len,
                )
            };
            let this = drop_or_unlink(traces, traces_len, this);
            drop(user_guard);
            drop(this); // Drop outside of the lock to reduce contention.
        }
    }
}

impl Trace {
    fn new(
        quota: QuotaRef,
        actor: Arc<Actor>,
    ) -> Result<Arc<Self>, AllocError> {
        let inner = LockedBy::new(
            &quota.arc.user.arc.inner,
            TraceInner {
                claim: Claim::new(),
            },
        );
        Arc::new(
            Self {
                quota,
                quota_rb: Default::default(),
                actor,
                inner,
            },
            GFP_KERNEL,
        )
    }
}

impl Charge {
    fn with(
        trace: TraceRef,
        amount: &[Value; N_SLOTS],
    ) -> Self {
        Self {
            inner: capi::b1_acct_charge {
                trace: TraceRef::into_raw(trace).cast(),
                amount: *amount,
            },
        }
    }

    /// Wrap the C API charge representation as a `Charge`.
    ///
    /// `Charge` is a transparent wrapper around `capi::b1_acct_charge`. This
    /// method allows interpreting any raw C API charges as a `Charge` object.
    ///
    /// This does not move the data, nor does it pin the contents. The caller
    /// can swap out of the mutable reference, if desired.
    ///
    /// ## Safety
    ///
    /// `capi` must be convertible to a mutable reference for the lifetime `'a`
    /// (which can be freely chosen by the caller). `capi.trace` must either be
    /// `NULL` or a valid value acquired via [`TraceRef::into_raw()`].
    ///
    /// `capi.amount` should reflect the actual charge values, otherwise a
    /// discharge might panic due to integer overflows (but it does not violate
    /// safety requirements).
    pub unsafe fn from_capi<'a>(
        capi: *mut capi::b1_acct_charge,
    ) -> &'a mut Self {
        // SAFETY: Delegated to caller.
        unsafe {
            &mut *capi.cast::<Self>()
        }
    }

    /// Discharge the claimed resources.
    pub fn discharge(&mut self) {
        let trace_ptr = core::mem::take(&mut self.inner.trace);
        if let Some(trace_nn) = core::ptr::NonNull::new(trace_ptr) {
            // SAFETY: `self.inner.trace` was acquired via
            //     `TraceRef::into_raw()`. Re-use is prevented by always using
            //     `core::mem::take()`.
            let trace = unsafe { TraceRef::from_raw(trace_nn.as_ptr().cast()) };
            trace.discharge(&self.inner.amount);
        }
    }
}

impl core::ops::Drop for Charge {
    fn drop(&mut self) {
        self.discharge();
    }
}

/// Create a new accounting system.
///
/// This is the C API for [`Arc::new()`]. It returns an error pointer for
/// `ENOMEM`, or a valid reference to the newly created [`Acct`] object.
///
/// ## Safety
///
/// `maxima` must be convertible to a shared reference.
#[export_name = "b1_acct_new"]
pub unsafe extern "C" fn acct_new(
    maxima: *const [Value; N_SLOTS],
) -> *mut capi::b1_acct {
    // SAFETY: Delegated to caller.
    match Acct::new(unsafe { &*maxima }) {
        Ok(v) => Acct::into_raw(v),
        Err(AllocError) => Error::from_errno(capi::B1_ACCT_ERROR_OOM).to_ptr(),
    }
}

/// Create a new reference to an accounting system.
///
/// This increases the reference count of the accounting system by one. If
/// `NULL` is passed, this is a no-op.
///
/// This always returns back the same pointer as was passed.
///
/// ## Safety
///
/// If non-NULL, `acct` must refer to a valid accounting system and the caller
/// must hold a reference to it.
#[export_name = "b1_acct_ref"]
pub unsafe extern "C" fn acct_ref(
    this: *mut capi::b1_acct,
) -> *mut capi::b1_acct {
    if let Some(this_nn) = core::ptr::NonNull::new(this) {
        // Ensure `this_ref` is not dropped on panic.
        let this_ref = ManuallyDrop::new(
            // SAFETY: Delegated to caller.
            unsafe { Acct::from_raw(this_nn.as_ptr()) }
        );
        let r = (*this_ref).clone();
        let _ = Acct::into_raw(ManuallyDrop::into_inner(this_ref));
        Acct::into_raw(r)
    } else {
        this
    }
}

/// Drop a reference to an accounting system.
///
/// This decreases the reference count of the accounting system by one. If
/// `NULL` is passed, this is a no-op. If this drops the last reference, the
/// entire accounting system is deallocated.
///
/// Note that actors and users also own a reference to the accounting system.
///
/// This always returns `NULL`.
///
/// ## Safety
///
/// If non-NULL, `acct` must refer to a valid accounting system and the caller
/// must hold a reference to it.
#[export_name = "b1_acct_unref"]
pub unsafe extern "C" fn acct_unref(
    this: *mut capi::b1_acct,
) -> *mut capi::b1_acct {
    if let Some(this_nn) = core::ptr::NonNull::new(this) {
        // SAFETY: Delegated to caller.
        let _ = unsafe { Acct::from_raw(this_nn.as_ptr()) };
    }
    core::ptr::null_mut()
}

/// Create a new actor for a given user.
///
/// This always creates a new actor, which will act on behalf of the given
/// user.
///
/// This can return `ENOMEM` as error pointer on failure.
///
/// ## Safety
///
/// `user` must refer to a valid user and the caller must hold a reference
/// to it.
#[export_name = "b1_acct_actor_new"]
pub unsafe extern "C" fn actor_new(
    user: *mut capi::b1_acct_user,
) -> *mut capi::b1_acct_actor {
    let user_nn = core::ptr::NonNull::new(user).unwrap();
    // Ensure `user_ref` is not dropped on panic.
    let user_ref = ManuallyDrop::new(
        // SAFETY: Delegated to caller.
        unsafe { UserRef::from_raw(user_nn.as_ptr()) }
    );
    let r = match Actor::with((*user_ref).clone()) {
        Ok(v) => Actor::into_raw(v),
        Err(AllocError) => Error::from_errno(capi::B1_ACCT_ERROR_OOM).to_ptr(),
    };
    let _ = UserRef::into_raw(ManuallyDrop::into_inner(user_ref));
    r
}

/// Create a new reference to an actor.
///
/// This increases the reference count of the actor by one. If `NULL` is
/// passed, this is a no-op.
///
/// This always returns back the same pointer as was passed.
///
/// ## Safety
///
/// If non-NULL, `actor` must refer to a valid actor and the caller must hold a
/// reference to it.
#[export_name = "b1_acct_actor_ref"]
pub unsafe extern "C" fn actor_ref(
    this: *mut capi::b1_acct_actor,
) -> *mut capi::b1_acct_actor {
    if let Some(this_nn) = core::ptr::NonNull::new(this) {
        // Ensure `this_arc` is not dropped on panic.
        let this_ref = ManuallyDrop::new(
            // SAFETY: Delegated to caller.
            unsafe { Actor::from_raw(this_nn.as_ptr()) }
        );
        let r = (*this_ref).clone();
        let _ = Actor::into_raw(ManuallyDrop::into_inner(this_ref));
        Actor::into_raw(r)
    } else {
        this
    }
}

/// Drop a reference to an actor.
///
/// This decreases the reference count of the actor by one. If `NULL` is
/// passed, this is a no-op. If this drops the last reference, the
/// entire actor is deallocated.
///
/// This always returns `NULL`.
///
/// ## Safety
///
/// If non-NULL, `actor` must refer to a valid actor and the caller must hold a
/// reference to it.
#[export_name = "b1_acct_actor_unref"]
pub unsafe extern "C" fn actor_unref(
    this: *mut capi::b1_acct_actor,
) -> *mut capi::b1_acct_actor {
    if let Some(this_nn) = core::ptr::NonNull::new(this) {
        // SAFETY: Delegated to caller.
        let _ = unsafe { Actor::from_raw(this_nn.as_ptr()) };
    }
    core::ptr::null_mut()
}

/// Charge an actor.
///
/// Charge the actor `this` for the resource amount given in `amount`. The
/// claimant actor is given as `claimant`. The charge is recorded in `charge`.
///
/// If `charge` is already used, this will return `EINVAL`. If the user quota
/// is exceeded, this will return `EDQUOT`. If the actor quota is exceeded,
/// this will return `EXFULL`.
///
/// ## Safety
///
/// `this` must refer to a valid actor and the caller must hold a reference
/// to it.
///
/// `claimant` must refer to a valid actor and the caller must hold a reference
/// to it.
///
/// `charge` must refer to an initialized and valid charge object.
///
/// `amount` must refer to a valid array of charge values.
#[export_name = "b1_acct_actor_charge"]
pub unsafe extern "C" fn actor_charge(
    this: *mut capi::b1_acct_actor,
    charge: *mut capi::b1_acct_charge,
    claimant: *mut capi::b1_acct_actor,
    amount: *const [Value; N_SLOTS],
) -> c_int {
    let this_nn = core::ptr::NonNull::new(this).unwrap();
    let charge_nn = core::ptr::NonNull::new(charge).unwrap();
    let claimant_nn = core::ptr::NonNull::new(claimant).unwrap();
    let amount_nn = core::ptr::NonNull::new(amount.cast_mut()).unwrap();

    // Ensure `this_ref` is not dropped on panic.
    let this_ref = ManuallyDrop::new(
        // SAFETY: Delegated to caller.
        unsafe { Actor::from_raw(this_nn.as_ptr()) }
    );
    // Ensure `claimant_ref` is not dropped on panic.
    let claimant_ref = ManuallyDrop::new(
        // SAFETY: Delegated to caller.
        unsafe { Actor::from_raw(claimant_nn.as_ptr()) }
    );
    // SAFETY: Delegated to caller.
    let charge_ref = unsafe { Charge::from_capi(charge_nn.as_ptr()) };
    // SAFETY: Delegated to caller.
    let amount_ref = unsafe { amount_nn.as_ref() };

    let r = if !charge_ref.inner.trace.is_null() {
        capi::B1_ACCT_ERROR_INVALID
    } else {
        match this_ref.as_arc_borrow().charge(
            claimant_ref.as_arc_borrow(),
            amount_ref,
        ) {
            Ok(mut v) => {
                core::mem::swap(&mut v, charge_ref);
                0
            },
            Err(ChargeError::Alloc(AllocError)) => {
                capi::B1_ACCT_ERROR_OOM
            },
            Err(ChargeError::UserQuota) => {
                capi::B1_ACCT_ERROR_USER_QUOTA
            },
            Err(ChargeError::ActorQuota) => {
                capi::B1_ACCT_ERROR_ACTOR_QUOTA
            },
        }
    };

    let _ = Actor::into_raw(ManuallyDrop::into_inner(claimant_ref));
    let _ = Actor::into_raw(ManuallyDrop::into_inner(this_ref));
    r
}

/// Get a user object from an accounting system.
///
/// This either creates a new user object, or returns the existing user object
/// for the given ID in this accounting system.
///
/// This can return `ENOMEM` as error pointer on failure.
///
/// ## Safety
///
/// `acct` must refer to a valid accounting system and the caller must hold
/// a reference to it.
#[export_name = "b1_acct_get_user"]
pub unsafe extern "C" fn acct_get_user(
    this: *mut capi::b1_acct,
    id: Id,
) -> *mut capi::b1_acct_user {
    let this_nn = core::ptr::NonNull::new(this).unwrap();
    // Ensure `this_ref` is not dropped on panic.
    let this_ref = ManuallyDrop::new(
        // SAFETY: Delegated to caller.
        unsafe { Acct::from_raw(this_nn.as_ptr()) }
    );
    let r = match this_ref.as_arc_borrow().get_user(id) {
        Ok(v) => UserRef::into_raw(v),
        Err(AllocError) => Error::from_errno(capi::B1_ACCT_ERROR_OOM).to_ptr(),
    };
    let _ = Acct::into_raw(ManuallyDrop::into_inner(this_ref));
    r
}

/// Create a new reference to a user.
///
/// This increases the reference count of the user by one. If `NULL` is
/// passed, this is a no-op.
///
/// This always returns back the same pointer as was passed.
///
/// ## Safety
///
/// If non-NULL, `user` must refer to a valid user and the caller must hold a
/// reference to it.
#[export_name = "b1_acct_user_ref"]
pub unsafe extern "C" fn user_ref(
    this: *mut capi::b1_acct_user,
) -> *mut capi::b1_acct_user {
    if let Some(this_nn) = core::ptr::NonNull::new(this) {
        // Ensure `this_ref` is not dropped on panic.
        let this_ref = ManuallyDrop::new(
            // SAFETY: Delegated to caller.
            unsafe { UserRef::from_raw(this_nn.as_ptr()) }
        );
        let r = (*this_ref).clone();
        let _ = UserRef::into_raw(ManuallyDrop::into_inner(this_ref));
        UserRef::into_raw(r)
    } else {
        this
    }
}

/// Drop a reference to a user.
///
/// This decreases the reference count of the user by one. If `NULL` is
/// passed, this is a no-op. If this drops the last reference, the
/// entire user is deallocated.
///
/// This always returns `NULL`.
///
/// ## Safety
///
/// If non-NULL, `user` must refer to a valid user and the caller must hold a
/// reference to it.
#[export_name = "b1_acct_user_unref"]
pub unsafe extern "C" fn user_unref(
    this: *mut capi::b1_acct_user,
) -> *mut capi::b1_acct_user {
    if let Some(this_nn) = core::ptr::NonNull::new(this) {
        // SAFETY: Delegated to caller.
        let _ = unsafe { UserRef::from_raw(this_nn.as_ptr()) };
    }
    core::ptr::null_mut()
}

/// Initialize a charge object.
///
/// This initializes the possibly uninitialized charge object. Any previous
/// value is leaked and replaced.
///
/// On return, the charge object will be cleared and contain no charge. Any
/// discharge operation will thus be a no-op, unless the object is charged
/// in between.
///
/// ## Safety
///
/// `this` must refer to a charge object, but can be uninitialized.
#[export_name = "b1_acct_charge_init"]
pub unsafe extern "C" fn charge_init(
    this: *mut capi::b1_acct_charge,
) {
    let this_nn = core::ptr::NonNull::new(this).unwrap();
    unsafe {
        this_nn.write(capi::b1_acct_charge {
            trace: core::ptr::null_mut(),
            amount: [0; _],
        });
    }
}

/// Deinitialize a charge object.
///
/// This deinitializes a charge object. Any charged are discharged and the
/// object is put into a freshly initialized state, ready to be re-used or
/// dropped.
///
/// ## Safety
///
/// `this` must refer to a valid charge object.
#[export_name = "b1_acct_charge_deinit"]
pub unsafe extern "C" fn charge_deinit(
    this: *mut capi::b1_acct_charge,
) {
    let this_nn = core::ptr::NonNull::new(this).unwrap();
    // SAFETY: Delegated to caller.
    unsafe {
        Charge::from_capi(this_nn.as_ptr()).discharge();
    }
}

#[kunit_tests(bus1_acct)]
mod test {
    use super::*;

    #[test]
    fn basic() -> Result<(), AllocError> {
        let acct = Acct::new(&[1024; N_SLOTS])?;
        let u0 = acct.as_arc_borrow().get_user(0)?;
        let u1 = acct.as_arc_borrow().get_user(1)?;
        let u2 = acct.as_arc_borrow().get_user(2)?;
        let u0a0 = Actor::with(u0.clone())?;
        let u0a1 = Actor::with(u0.clone())?;
        let u1a0 = Actor::with(u1.clone())?;
        let u1a1 = Actor::with(u1.clone())?;
        let u2a0 = Actor::with(u2.clone())?;
        let u2a1 = Actor::with(u2.clone())?;
        let u0a0b = u0a0.as_arc_borrow();
        let u0a1b = u0a1.as_arc_borrow();
        let u1a0b = u1a0.as_arc_borrow();
        let u1a1b = u1a1.as_arc_borrow();
        let u2a0b = u2a0.as_arc_borrow();
        let u2a1b = u2a1.as_arc_borrow();

        // Perform a self-charge and verify that half of the resources is the
        // maximum that can be requested. Also verify that this grants full
        // access to all user resources (i.e., requests of other users always
        // fail).
        // Then verify that releasing a chunk allows reclaiming the resources.
        {
            let _c0 = u0a0b.charge(u0a0b, &[256; N_SLOTS]).unwrap();
            let c1 = u0a0b.charge(u0a0b, &[256; N_SLOTS]).unwrap();
            assert!(u0a0b.charge(u0a0b, &[1; N_SLOTS]).is_err());
            assert!(u0a0b.charge(u1a0b, &[1; N_SLOTS]).is_err());

            drop(c1);
            let _c2 = u0a0b.charge(u0a0b, &[256; N_SLOTS]).unwrap();
        }

        // Perform a self-charge with two actors, and verify the first gets
        // half, and the second gets half of that. Also ensure foreign actors
        // cannot claim anything, since this series requires the full resource
        // set of the user.
        {
            let _c0 = u0a0b.charge(u0a0b, &[512; N_SLOTS]).unwrap();
            assert!(u0a0b.charge(u0a0b, &[1; N_SLOTS]).is_err());
            assert!(u0a0b.charge(u1a0b, &[1; N_SLOTS]).is_err());

            let _c1 = u0a0b.charge(u0a1b, &[256; N_SLOTS]).unwrap();
            assert!(u0a0b.charge(u0a0b, &[1; N_SLOTS]).is_err());
            assert!(u0a0b.charge(u0a1b, &[1; N_SLOTS]).is_err());
        }

        // Perform a foreign-charge with two actors of the same UID, and verify
        // that they share the foreign charge (i.e., the second charge is half
        // of the first charge, if both charge to their limit).
        // Then perform a foreign-charge of another two actors of yet another
        // UID. Again, charge to their limit (which must be non-zero, since the
        // quota separates them from the previous charges) and verify it is
        // the expected limit.
        {
            let _c0 = u0a0b.charge(u1a0b, &[128; N_SLOTS]).unwrap();
            assert!(u0a0b.charge(u1a0b, &[1; N_SLOTS]).is_err());
            let _c1 = u0a0b.charge(u1a1b, &[64; N_SLOTS]).unwrap();
            assert!(u0a0b.charge(u1a0b, &[1; N_SLOTS]).is_err());
            let _c2 = u0a0b.charge(u2a0b, &[42; N_SLOTS]).unwrap();
            assert!(u0a0b.charge(u2a0b, &[1; N_SLOTS]).is_err());
            let _c3 = u0a0b.charge(u2a1b, &[21; N_SLOTS]).unwrap();
            assert!(u0a0b.charge(u2a1b, &[1; N_SLOTS]).is_err());
        }
        Ok(())
    }
}
