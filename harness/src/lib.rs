//! Stand-ins for the external systems the invoicing service talks to.
//! Library + thin binaries, so integration tests can run them in-process
//! and inspect what they received.

pub mod mock_psp;
pub mod webhook_sink;
