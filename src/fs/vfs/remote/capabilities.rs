// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! A transport-local negotiation. Legacy dispatchers may close on unknown
//! requests, so a normal reply must advertise support before querying.

use hyper_filesystem::protocol::{Capabilities, Error, ResponseFeatures};

pub(super) enum Negotiation {
    Unknown,
    Advertised,
    Legacy,
    Known(Capabilities),
}

impl Negotiation {
    pub(super) fn observe(&mut self, features: ResponseFeatures) {
        if matches!(self, Self::Unknown) && features.supports_capability_query() {
            *self = Self::Advertised;
        }
    }

    pub(super) fn needs_query(&self) -> bool {
        matches!(self, Self::Advertised)
    }

    pub(super) fn resolve(&mut self, result: Result<u64, Error>) -> Result<(), Error> {
        *self = match result {
            Ok(bits) => Self::Known(Capabilities::from_bits(bits)),
            Err(Error::Unsupported) => Self::Legacy,
            Err(error) => return Err(error),
        };
        Ok(())
    }

    pub(super) fn read_extents(&self) -> bool {
        match self {
            Self::Known(capabilities) => capabilities.supports_read_extents(),
            Self::Unknown | Self::Advertised | Self::Legacy => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_replies_never_cause_an_unknown_request() {
        let mut state = Negotiation::Unknown;
        for _ in 0..3 {
            state.observe(ResponseFeatures::NONE);
            assert!(!state.needs_query());
            assert!(state.read_extents());
        }
    }

    #[test]
    fn absent_extent_support_is_cached_for_this_transport() -> Result<(), Error> {
        let mut state = Negotiation::Unknown;
        state.observe(ResponseFeatures::CAPABILITY_QUERY);
        assert!(state.needs_query());
        state.resolve(Ok(Capabilities::NONE.bits()))?;
        for _ in 0..3 {
            state.observe(ResponseFeatures::CAPABILITY_QUERY);
            assert!(!state.needs_query());
            assert!(!state.read_extents());
        }
        // A replacement transport starts with no assumptions about its provider.
        assert!(Negotiation::Unknown.read_extents());
        Ok(())
    }

    #[test]
    fn unsupported_query_keeps_legacy_fallback_without_retrying() -> Result<(), Error> {
        let mut state = Negotiation::Advertised;
        state.resolve(Err(Error::Unsupported))?;
        state.observe(ResponseFeatures::CAPABILITY_QUERY);
        assert!(!state.needs_query());
        assert!(state.read_extents());
        Ok(())
    }

    #[test]
    fn unknown_bits_do_not_hide_known_support_or_trigger_queries() -> Result<(), Error> {
        let mut state = Negotiation::Unknown;
        state.observe(ResponseFeatures::from_bits(1 << 63));
        assert!(!state.needs_query());
        state.observe(ResponseFeatures::from_bits(1 | 1 << 63));
        assert!(state.needs_query());
        state.resolve(Ok(Capabilities::READ_EXTENTS.bits() | 1 << 63))?;
        assert!(state.read_extents());
        assert!(!state.needs_query());
        Ok(())
    }

    #[test]
    fn query_failure_is_not_silently_downgraded() {
        let mut state = Negotiation::Advertised;
        assert_eq!(state.resolve(Err(Error::Corrupt)), Err(Error::Corrupt));
        assert_eq!(state.resolve(Err(Error::Closed)), Err(Error::Closed));
    }
}
