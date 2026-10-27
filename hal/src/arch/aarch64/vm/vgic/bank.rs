// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Runtime bank ownership across local edits and outer vCPU cleanup.

enum State {
    Fresh,
    Saved,
    Live,
    Invalid,
}

pub(crate) struct Bank {
    state: State,
    reused_save: bool,
}

impl Bank {
    pub(crate) const fn new() -> Self {
        Self {
            state: State::Fresh,
            reused_save: false,
        }
    }

    pub(crate) fn load<E>(
        &mut self,
        invalid: E,
        load: impl FnOnce() -> Result<(), E>,
    ) -> Result<(), E> {
        if !matches!(self.state, State::Fresh | State::Saved) {
            return Err(invalid);
        }
        // A partially failed hardware operation must not become a valid snapshot.
        self.state = State::Invalid;
        load()?;
        self.state = State::Live;
        Ok(())
    }

    pub(crate) fn save<E>(
        &mut self,
        invalid: E,
        save: impl FnOnce() -> Result<(), E>,
    ) -> Result<(), E> {
        match self.state {
            State::Saved => {
                // A local edit failed after saving. Outer cleanup reuses that
                // snapshot instead of reading the already-disabled hardware.
                self.reused_save = true;
                return Ok(());
            }
            State::Live => {}
            State::Fresh | State::Invalid => return Err(invalid),
        }
        self.state = State::Invalid;
        save()?;
        self.state = State::Saved;
        Ok(())
    }

    pub(crate) fn take_reused_save(&mut self) -> bool {
        core::mem::take(&mut self.reused_save)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_local_edit_cleanup_preserves_first_snapshot() {
        let mut bank = Bank::new();
        let mut snapshot = 0;
        assert_eq!(bank.load((), || Ok(())), Ok(()));
        assert_eq!(
            bank.save((), || {
                snapshot = 7;
                Ok(())
            }),
            Ok(())
        );
        // Model failure between save and restore, followed by outer cleanup.
        assert_eq!(
            bank.save((), || {
                snapshot = 0;
                Ok(())
            }),
            Ok(())
        );
        assert_eq!(snapshot, 7);
        assert!(bank.take_reused_save());
        assert!(!bank.take_reused_save());
        assert_eq!(bank.load((), || Ok(())), Ok(()));
        assert_eq!(
            bank.save((), || {
                snapshot = 9;
                Ok(())
            }),
            Ok(())
        );
        assert_eq!(snapshot, 9);
        assert!(!bank.take_reused_save());
    }

    #[test]
    fn invalid_transitions_do_not_access_hardware_or_reuse_partial_snapshots() {
        let mut bank = Bank::new();
        assert_eq!(bank.save(1, || panic!("fresh save")), Err(1));
        assert_eq!(bank.load(1, || Ok(())), Ok(()));
        assert_eq!(bank.load(1, || panic!("double load")), Err(1));
        assert_eq!(bank.save(1, || Err(2)), Err(2));
        assert_eq!(bank.save(1, || panic!("partial save reused")), Err(1));
        assert_eq!(bank.load(1, || panic!("partial save restored")), Err(1));
        assert!(!bank.take_reused_save());
        let mut bank = Bank::new();
        assert_eq!(bank.load(1, || Err(2)), Err(2));
        assert_eq!(bank.save(1, || panic!("failed load saved")), Err(1));
    }
}
