// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Explicit protocol annotations. These scalars are untrusted observations, not
//! requests to inspect another endpoint, capture authority, or ambient context.

use hyper::abi::native::*;

pub(crate) fn annotate(connection: u64, args: &[u64; 6]) -> super::Result<[u64; 2]> {
    let [
        phase,
        request,
        detail,
        generation,
        parent_connection,
        parent_request,
    ] = *args;
    if phase > HYPER_NATIVE_RPC_TRACE_SERVER_END
        || (phase == HYPER_NATIVE_RPC_TRACE_QUERY && args[1..].iter().any(|value| *value != 0))
        || (phase != HYPER_NATIVE_RPC_TRACE_QUERY && request == 0)
        || (matches!(
            phase,
            HYPER_NATIVE_RPC_TRACE_CLIENT_END | HYPER_NATIVE_RPC_TRACE_SERVER_END
        ) && (parent_connection != 0 || parent_request != 0))
        || (matches!(
            phase,
            HYPER_NATIVE_RPC_TRACE_CLIENT_BEGIN | HYPER_NATIVE_RPC_TRACE_SERVER_BEGIN
        ) && !((generation == 0 && parent_connection == 0 && parent_request == 0)
            || (generation != 0 && parent_connection != 0 && parent_request != 0)))
    {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    let Some(ticket) = hyper_trace::enabled(hyper_trace::Category::Ipc) else {
        return Ok([0, connection]);
    };
    if connection == 0 {
        return Ok([0, 0]);
    }
    let current = ticket.generation();
    // Use the same admission ticket for every record: stop/start between this
    // check and emit cannot leak an old completion into a replacement session.
    match phase {
        HYPER_NATIVE_RPC_TRACE_CLIENT_BEGIN => crate::tracepoint!(
            ticket = ticket,
            Ipc,
            "rpc_client_begin",
            connection = connection,
            request = request,
            thread = super::actor(),
            operation = detail
        ),
        HYPER_NATIVE_RPC_TRACE_SERVER_BEGIN => crate::tracepoint!(
            ticket = ticket,
            Ipc,
            "rpc_server_begin",
            connection = connection,
            request = request,
            thread = super::actor(),
            operation = detail
        ),
        HYPER_NATIVE_RPC_TRACE_CLIENT_END | HYPER_NATIVE_RPC_TRACE_SERVER_END
            if generation != current =>
        {
            return Ok([0, connection]);
        }
        HYPER_NATIVE_RPC_TRACE_CLIENT_END => crate::tracepoint!(
            ticket = ticket,
            Ipc,
            "rpc_client_end",
            connection = connection,
            request = request,
            thread = super::actor(),
            status = detail
        ),
        HYPER_NATIVE_RPC_TRACE_SERVER_END => crate::tracepoint!(
            ticket = ticket,
            Ipc,
            "rpc_server_end",
            connection = connection,
            request = request,
            thread = super::actor(),
            status = detail
        ),
        _ => return Ok([current, connection]),
    }
    if matches!(
        phase,
        HYPER_NATIVE_RPC_TRACE_CLIENT_BEGIN | HYPER_NATIVE_RPC_TRACE_SERVER_BEGIN
    ) && generation == current
        && parent_connection != 0
    {
        crate::tracepoint!(
            ticket = ticket,
            Ipc,
            "rpc_parent",
            connection = connection,
            request = request,
            parent_connection = parent_connection,
            parent_request = parent_request
        );
    }
    Ok([current, connection])
}
