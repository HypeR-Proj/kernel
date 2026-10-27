// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native executable loading into unpublished Process address spaces.

use hyper::mm::{PAGE_SIZE, UniqueFallibleArc};
use {crate::process::formats::bootstrap::Image, crate::process::formats::bootstrap::Machine};

use super::{ImageError, MachineAbi, ProcessImage};
use crate::fs::vfs::ExecutableSnapshot;
use {crate::accounting::ResourceDomain, crate::accounting::ResourceError};
use {
    crate::mm::user_space::MachineError, crate::mm::user_space::NativeAddressSpace,
    crate::mm::user_space::NativeImageSegment, crate::mm::user_space::Permissions,
    crate::mm::user_space::UserAddress, crate::mm::user_space::UserSlice,
};

const LOADER_BASE: u64 = 0x10_0000;
const STACK_SIZE: u64 = 128 * 1024;
const STACK_TOP: u64 = 0x3f_f000;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BootstrapLayout {
    pub(crate) stack_base: u64,
    pub(crate) stack_size: u64,
    pub(crate) loader_base: u64,
    pub(crate) loader_size: u64,
}

#[derive(Debug)]
pub(crate) enum Error {
    Address,
    Elf(
        #[allow(
            dead_code,
            reason = "Retains bootstrap validation details for diagnostics"
        )]
        crate::process::formats::bootstrap::Error,
    ),
    Image(
        #[expect(
            dead_code,
            reason = "Retains image failure details for Debug diagnostics"
        )]
        ImageError,
    ),
    Machine(MachineError),
    Resource(ResourceError),
    UnsupportedMachine,
}

impl From<MachineError> for Error {
    fn from(error: MachineError) -> Self {
        Self::Machine(error)
    }
}

impl From<ResourceError> for Error {
    fn from(error: ResourceError) -> Self {
        Self::Resource(error)
    }
}

pub(crate) struct LoadedProcessImage {
    pub(crate) image: ProcessImage,
    pub(crate) address_space: UniqueFallibleArc<NativeAddressSpace>,
}

/// Validates and installs the caller-authorized loader snapshot and a guarded
/// stack. Application ELF interpretation and loader selection belong to userspace.
pub(crate) fn load_native(
    loader: &ExecutableSnapshot,
    domain: ResourceDomain,
) -> Result<LoadedProcessImage, Error> {
    let image = Image::parse(loader.bytes()).map_err(Error::Elf)?;
    validate_host_machine(image.machine)?;
    let address_space = create_address_space(domain)?;
    match prepare_address_space(loader, &image, &address_space) {
        Ok(image) => Ok(LoadedProcessImage {
            image,
            address_space,
        }),
        Err(error) => {
            retire_failed_address_space(address_space);
            Err(error)
        }
    }
}

// Constructing the unpublished address space has larger machine and accounting
// temporaries than mapping it. Do not retain that scratch on the mapping call chain.
#[inline(never)]
fn create_address_space(
    domain: ResourceDomain,
) -> Result<UniqueFallibleArc<NativeAddressSpace>, Error> {
    let limit = crate::hal::user::address_space_plan()
        .map_err(MachineError::from)?
        .application_limit();
    let root = UserSlice::new(
        UserAddress::new(PAGE_SIZE),
        limit.checked_sub(PAGE_SIZE).ok_or(Error::Address)?,
    )
    .map_err(|_| Error::Address)?;
    NativeAddressSpace::try_new(domain, root).map_err(Error::from)
}

// Loader parsing finishes before mapping preparation creates its transaction
// owners. Keep those temporary lifetimes in separate kernel-stack frames.
#[inline(never)]
fn prepare_address_space(
    loader: &ExecutableSnapshot,
    image: &Image,
    address_space: &NativeAddressSpace,
) -> Result<ProcessImage, Error> {
    let stack_base = STACK_TOP - STACK_SIZE;
    let reservation = UserSlice::new(
        UserAddress::new(stack_base - PAGE_SIZE),
        STACK_SIZE + 2 * PAGE_SIZE,
    )
    .map_err(|_| Error::Address)?;
    let stack_vmar = address_space.reserve_initial_stack(reservation)?;
    // Segment installation pins only executable cache maintenance. Mapping
    // commit remains sleepable when concurrent loaders contend for transport.
    for segment in image.segments() {
        let range = UserSlice::new(
            UserAddress::new(LOADER_BASE + segment.address),
            segment.size,
        )
        .map_err(|_| Error::Address)?;
        let permissions = if segment.executable {
            Permissions::read_execute()
        } else if segment.writable {
            Permissions::read_write()
        } else {
            Permissions::read_only()
        };
        let bytes = segment.backing_bytes(loader.bytes()).map_err(Error::Elf)?;
        let prepared = NativeImageSegment::try_from_bytes(
            address_space,
            range,
            permissions,
            bytes,
            segment.data_offset,
        )?;
        prepared.install(address_space)?;
    }
    let stack = NativeImageSegment::try_new(
        address_space,
        UserSlice::new(UserAddress::new(stack_base), STACK_SIZE).map_err(|_| Error::Address)?,
        Permissions::read_write(),
    )?;
    stack.install_in_vmar(address_space, stack_vmar)?;
    ProcessImage::try_native(
        machine_abi(image.machine),
        UserAddress::new(LOADER_BASE + image.entry),
        UserAddress::new(STACK_TOP),
        UserAddress::new(0),
    )
    .map(|process_image| {
        process_image.with_bootstrap(
            stack_vmar,
            BootstrapLayout {
                stack_base,
                stack_size: STACK_SIZE,
                loader_base: LOADER_BASE,
                loader_size: image.size,
            },
        )
    })
    .map_err(Error::Image)
}

fn machine_abi(machine: Machine) -> MachineAbi {
    match machine {
        Machine::Aarch64 => MachineAbi::Aarch64,
        Machine::Riscv64 => MachineAbi::Riscv64,
    }
}

fn validate_host_machine(machine: Machine) -> Result<(), Error> {
    use crate::hal::user::HostMachine;
    if matches!(
        (machine, crate::hal::user::host_machine()),
        (Machine::Aarch64, HostMachine::Aarch64) | (Machine::Riscv64, HostMachine::Riscv64)
    ) {
        Ok(())
    } else {
        Err(Error::UnsupportedMachine)
    }
}

fn retire_failed_address_space(address_space: UniqueFallibleArc<NativeAddressSpace>) {
    if let Err(failure) = NativeAddressSpace::retire_unpublished(address_space) {
        let (error, retained) = failure.into_parts();
        crate::pr_err!(
            "HypeR: retaining a failed Native image address space after cleanup error: {error:?}"
        );
        drop(retained);
    }
}
