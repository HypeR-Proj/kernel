<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# Native object interfaces

The compiler-checked [schema](../../../sdk/abi/schema/native.rs) owns receiver kinds,
object-local selectors, arguments, required rights and completion classes.
Every trap names one receiver handle. Common handle and object operations share
selectors across kinds; other operations are decoded using the receiver kind.
The [machine contract](syscall-abi.md#native-machine-abi) defines the registers.

## Group by authority and ownership

| Interface | Boundary that remains explicit |
| --- | --- |
| Handles and object observation | Process-local authority versus object identity and waitable state |
| ExecutionContext | Current execution and waits, bound to the calling Process |
| ObjectFactory | Local object construction charged to the calling Process |
| SystemInfo | Read-only ABI, configuration and clock queries |
| Process builders and task accounting | Reversible construction versus publication, execution and stop |
| Threads and WaitSets | Creation versus runnable admission; distinct wait ownership and completion |
| Directories and files | Namespace authority versus an opened file; positional data versus metadata |
| VMO and VMAR | Backing ownership and provenance versus address-space placement |
| Pending VM, VM and vCPU | Construction versus installed lifecycle; per-vCPU execution and exit completion |
| Physical devices and DMA | Claim authority, mapped resources and lifetime-bound physical access |
| Guest transport and block service | Memory grants, notifications and service publication remain separate capabilities |

The target selector removes repeated receiver arguments, but does not erase typed
contracts. Each operation retains explicit argument shapes, rights, buffer bounds,
reserved-zero validation, blocking and cancellation behavior. The kernel captures
receiver authority once, then dispatches on its kind and operation. All calls run
on the Thread's kernel stack with ordinary IRQ delivery, including blocking calls.

Directory open options remain a path-resolution policy; they do not define a new
object kind. `directory_remove_if` additionally checks expected node identity and
must preserve that lookup/remove race protection. File, directory and VM queries
return distinct typed records. Sharing record copying is useful; replacing these
records with an untyped byte-control API is not required by object invocation.
Task and object inspectors likewise retain separately typed visibility grants.

Schema, generated C/Rust bindings, kernel routing, installed SDK consumers and
contract tests must change together. Pre-release ABI revision remains zero; it
does not permit partial producer/consumer migrations.

## Reserved executable conversion

`ExecutableAuthority` and `CREATE_EXECUTABLE` are reserved foundations, not an
available Native issuance/conversion API. The callable file path uses
`file_protect` and `vmar_map_file`: File READ|EXECUTE plus active content
protection authorizes executable mappings, without exposing a snapshot VMO.
Converting arbitrary
writable bytes requires a separately issued authority, immutable snapshot and
architecture instruction-publication proof. Do not expose it implicitly through
ordinary VMO write/map rights. There is no current app consumer that justifies
adding a new authority-distribution policy as part of syscall regrouping.
