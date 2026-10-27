// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Runtime composition checks for init's typed authority objects.

use crate::authority::Rights;
use {
    crate::accounting::ResourceDomain, crate::accounting::ResourceDomainObject,
    crate::accounting::ResourceDomainObjectError, crate::accounting::ResourceLimits,
};
use {
    crate::capability::HandleError, crate::capability::HandleFlags,
    crate::capability::PreparedHandle,
};
use {crate::mm::user_space::ExecutableAuthority, crate::mm::user_space::VmoObject};
use {crate::object::KernelObject, crate::object::ObjectPublication};
use {crate::process::TaskFactory, crate::process::TaskGroup, crate::process::TaskGroupObject};

pub(crate) enum Error {
    Domain(ResourceDomainObjectError),
    DuplicateDomainPublicationAccepted,
    ExecutableAuthority(crate::mm::user_space::ExecutableAuthorityError),
    Factory(crate::process::TaskObjectError),
    Handle(HandleError),
    Memory(crate::mm::user_space::MemoryObjectError),
    Object(crate::object::ObjectCreationError),
    Rights,
}

impl core::fmt::Debug for Error {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Domain(error) => formatter.debug_tuple("Domain").field(error).finish(),
            Self::DuplicateDomainPublicationAccepted => {
                formatter.write_str("DuplicateDomainPublicationAccepted")
            }
            Self::ExecutableAuthority(error) => formatter
                .debug_tuple("ExecutableAuthority")
                .field(error)
                .finish(),
            Self::Factory(error) => formatter.debug_tuple("Factory").field(error).finish(),
            Self::Handle(error) => formatter.debug_tuple("Handle").field(error).finish(),
            Self::Memory(error) => formatter.debug_tuple("Memory").field(error).finish(),
            Self::Object(error) => formatter.debug_tuple("Object").field(error).finish(),
            Self::Rights => formatter.write_str("Rights"),
        }
    }
}

impl From<ResourceDomainObjectError> for Error {
    fn from(error: ResourceDomainObjectError) -> Self {
        Self::Domain(error)
    }
}

impl From<crate::process::TaskObjectError> for Error {
    fn from(error: crate::process::TaskObjectError) -> Self {
        Self::Factory(error)
    }
}

impl From<HandleError> for Error {
    fn from(error: HandleError) -> Self {
        Self::Handle(error)
    }
}

impl From<crate::mm::user_space::ExecutableAuthorityError> for Error {
    fn from(error: crate::mm::user_space::ExecutableAuthorityError) -> Self {
        Self::ExecutableAuthority(error)
    }
}

impl From<crate::mm::user_space::MemoryObjectError> for Error {
    fn from(error: crate::mm::user_space::MemoryObjectError) -> Self {
        Self::Memory(error)
    }
}

impl From<crate::object::ObjectCreationError> for Error {
    fn from(error: crate::object::ObjectCreationError) -> Self {
        Self::Object(error)
    }
}

pub(crate) fn run() -> Result<(), Error> {
    let domain = ResourceDomain::try_new_root(ResourceLimits::UNLIMITED)
        .map_err(ResourceDomainObjectError::Resource)?;
    let domain_object = ResourceDomainObject::try_publication(domain.clone())?;
    require_rights(
        domain_object.supported_rights(),
        ResourceDomainObject::SUPPORTED_RIGHTS,
    )?;
    if !matches!(
        ResourceDomainObject::try_publication(domain.clone()),
        Err(ResourceDomainObjectError::AlreadyPublished)
    ) {
        return Err(Error::DuplicateDomainPublicationAccepted);
    }

    let factory = ObjectPublication::try_new(TaskFactory::try_new(&domain)?)?;
    require_rights(factory.supported_rights(), TaskFactory::SUPPORTED_RIGHTS)?;
    let group = TaskGroupObject::try_publication(
        TaskGroup::try_new(&domain).map_err(crate::process::TaskObjectError::TaskGroup)?,
    )?;
    require_rights(group.supported_rights(), TaskGroupObject::SUPPORTED_RIGHTS)?;

    let executable_authority = ObjectPublication::try_new(ExecutableAuthority::try_new(&domain)?)?;
    require_rights(
        executable_authority.supported_rights(),
        ExecutableAuthority::SUPPORTED_RIGHTS,
    )?;

    let writable =
        ObjectPublication::try_new(VmoObject::try_new_writable(hyper::mm::PAGE_SIZE, &domain)?)?;
    let writable_rights = writable.supported_rights();
    if !writable_rights.contains(Rights::WRITE) || writable_rights.contains(Rights::EXECUTE) {
        return Err(Error::Rights);
    }
    if !matches!(
        PreparedHandle::try_from_new_object(writable, Rights::EXECUTE, HandleFlags::NONE),
        Err(HandleError::UnsupportedRights)
    ) {
        return Err(Error::Rights);
    }
    Ok(())
}

fn require_rights(actual: Rights, expected: Rights) -> Result<(), Error> {
    if actual == expected {
        Ok(())
    } else {
        Err(Error::Rights)
    }
}
