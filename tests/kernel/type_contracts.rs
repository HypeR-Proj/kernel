// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Type-check the real ownership tokens in every kernel-self-test build.
//!
//! These checks never construct or execute a token. Unlike source matching,
//! they include manually implemented traits and changes in nested field types.

use crate::hal::{exception, user, vm};
use crate::kernel::irq::interrupt;
use crate::kernel::task::scheduler;

// Inference has one answer until the forbidden trait is implemented. Each
// assertion uses its own marker, including traits that are not dyn-compatible.
macro_rules! assert_not_impl {
    ($type:ty: $($bound:path),+ $(,)?) => {
        $(
            const _: fn() = || {
                trait AmbiguousIfImpl<Marker> {
                    fn marker() {}
                }
                impl<T: ?Sized> AmbiguousIfImpl<()> for T {}
                struct Implemented;
                impl<T: ?Sized + $bound> AmbiguousIfImpl<Implemented> for T {}
                let _ = <$type as AmbiguousIfImpl<_>>::marker;
            };
        )+
    };
}

// Clone rejection also excludes Copy, whose supertrait is Clone.
assert_not_impl!(interrupt::Registration: Clone);
assert_not_impl!(interrupt::PreparedRegistration: Clone);
assert_not_impl!(interrupt::ActivationFailure: Clone);
assert_not_impl!(interrupt::DiscardFailure: Clone);
assert_not_impl!(interrupt::UnregisterFailure: Clone);
assert_not_impl!(vm::StoppedVcpuRun: Clone);
assert_not_impl!(vm::StoppedDetachFailure: Clone);
assert_not_impl!(user::ReturnCapability<'static>: Clone);
assert_not_impl!(user::CompletionFailure<'static>: Clone);
assert_not_impl!(hyper::mm::UpdateCut<4>: Clone);
assert_not_impl!(hyper::mm::RetirementCut<4>: Clone);

// These capabilities describe the current CPU or Thread continuation.
assert_not_impl!(exception::IrqTailCapability: Clone, Send, Sync);
assert_not_impl!(user::PreparedKernelAccess<'static>: Clone, Send, Sync);
assert_not_impl!(user::ActiveAddressSpace<'static>: Clone, Send, Sync);
assert_not_impl!(user::StoppedUser<'static, 'static>: Clone, Send, Sync);
assert_not_impl!(scheduler::PreemptionGuard: Clone, Send, Sync);
assert_not_impl!(scheduler::UserRunGuard: Clone, Send, Sync);
assert_not_impl!(scheduler::WaitRegistration: Clone, Send, Sync);

// Check error recovery consumes the token and returns that same token type.
// Borrowing or erasing the owner would fail these signature checks.
const _: fn(
    interrupt::PreparedRegistration,
) -> Result<interrupt::Registration, interrupt::ActivationFailure> = interrupt::activate;
const _: fn(interrupt::ActivationFailure) -> (interrupt::Error, interrupt::PreparedRegistration) =
    interrupt::ActivationFailure::into_parts;
const _: fn(interrupt::Registration) -> Result<(), interrupt::UnregisterFailure> =
    interrupt::unregister;
const _: fn(interrupt::UnregisterFailure) -> (interrupt::Error, interrupt::Registration) =
    interrupt::UnregisterFailure::into_parts;
const _: fn(interrupt::PreparedRegistration) -> Result<(), interrupt::DiscardFailure> =
    interrupt::discard_prepared;
const _: fn(interrupt::DiscardFailure) -> (interrupt::Error, interrupt::PreparedRegistration) =
    interrupt::DiscardFailure::into_parts;
