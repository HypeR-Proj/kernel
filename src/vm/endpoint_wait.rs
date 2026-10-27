// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Pure generation state for one durable vCPU wait endpoint.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    EpochExhausted,
    WaiterAlreadyRegistered,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Registration {
    Changed,
    Registered,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CompletionError {
    NotificationNotPublished,
    MismatchedWaiter,
}

pub(crate) struct WaitPublication<T> {
    epoch: u64,
    waiter: Option<T>,
}

impl<T: Copy + Eq> WaitPublication<T> {
    pub(crate) const fn new() -> Self {
        Self {
            epoch: 0,
            waiter: None,
        }
    }

    pub(crate) const fn snapshot(&self) -> u64 {
        self.epoch
    }

    pub(crate) fn register(
        &mut self,
        expected_epoch: u64,
        waiter: T,
    ) -> Result<Registration, Error> {
        if self.epoch != expected_epoch {
            return Ok(Registration::Changed);
        }
        if self.waiter.is_some() {
            return Err(Error::WaiterAlreadyRegistered);
        }
        self.waiter = Some(waiter);
        Ok(Registration::Registered)
    }

    pub(crate) fn signal(&mut self) -> Result<Option<T>, Error> {
        self.epoch = self.epoch.checked_add(1).ok_or(Error::EpochExhausted)?;
        Ok(self.waiter.take())
    }

    pub(crate) fn complete(
        &mut self,
        waiter: T,
        notification_won: bool,
    ) -> Result<(), CompletionError> {
        match self.waiter {
            Some(current) if current != waiter => Err(CompletionError::MismatchedWaiter),
            Some(_) if notification_won => Err(CompletionError::NotificationNotPublished),
            Some(_) => {
                self.waiter = None;
                Ok(())
            }
            // A signal always consumes the endpoint ticket before resolving
            // the scheduler wait. It may subsequently lose to cancellation;
            // either scheduler outcome therefore permits an empty slot.
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CompletionError, Error, Registration, WaitPublication};

    #[test]
    fn signal_before_registration_closes_the_old_epoch() {
        let mut publication = WaitPublication::new();
        let epoch = publication.snapshot();

        assert_eq!(publication.signal(), Ok(None));
        assert_eq!(
            publication.register(epoch, 7_u32),
            Ok(Registration::Changed)
        );
    }

    #[test]
    fn signal_returns_the_exact_registered_waiter_once() {
        let mut publication = WaitPublication::new();
        let epoch = publication.snapshot();

        assert_eq!(
            publication.register(epoch, 11_u32),
            Ok(Registration::Registered)
        );
        assert_eq!(publication.signal(), Ok(Some(11)));
        assert_eq!(publication.signal(), Ok(None));
    }

    #[test]
    fn duplicate_registration_is_failure_atomic() {
        let mut publication = WaitPublication::new();
        let epoch = publication.snapshot();

        assert_eq!(
            publication.register(epoch, 13_u32),
            Ok(Registration::Registered)
        );
        assert_eq!(
            publication.register(epoch, 17_u32),
            Err(Error::WaiterAlreadyRegistered)
        );
        assert_eq!(publication.signal(), Ok(Some(13)));
    }

    #[test]
    fn cancellation_before_signal_withdraws_the_exact_waiter() {
        let mut publication = WaitPublication::new();
        let epoch = publication.snapshot();
        assert_eq!(
            publication.register(epoch, 19_u32),
            Ok(Registration::Registered)
        );

        assert_eq!(
            publication.complete(23, false),
            Err(CompletionError::MismatchedWaiter)
        );
        assert_eq!(publication.complete(19, false), Ok(()));
        assert_eq!(publication.signal(), Ok(None));
    }

    #[test]
    fn signal_before_completion_confirms_notification() {
        let mut publication = WaitPublication::new();
        let epoch = publication.snapshot();
        assert_eq!(
            publication.register(epoch, 29_u32),
            Ok(Registration::Registered)
        );

        assert_eq!(publication.signal(), Ok(Some(29)));
        assert_eq!(publication.complete(29, true), Ok(()));
    }

    #[test]
    fn cancellation_then_losing_signal_is_benign_at_resume() {
        let mut publication = WaitPublication::new();
        let epoch = publication.snapshot();
        assert_eq!(
            publication.register(epoch, 31_u32),
            Ok(Registration::Registered)
        );

        // The scheduler cancellation wins without touching this condition
        // object. A later interrupt still consumes the exact endpoint ticket,
        // but loses scheduler arbitration.
        assert_eq!(publication.signal(), Ok(Some(31)));
        assert_eq!(publication.complete(31, false), Ok(()));
    }

    #[test]
    fn notified_completion_requires_prior_endpoint_signal() {
        let mut publication = WaitPublication::new();
        let epoch = publication.snapshot();
        assert_eq!(
            publication.register(epoch, 37_u32),
            Ok(Registration::Registered)
        );

        assert_eq!(
            publication.complete(37, true),
            Err(CompletionError::NotificationNotPublished)
        );
    }
}
