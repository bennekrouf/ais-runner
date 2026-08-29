//! Everything that talks to Azure over the network.
//!
//! The boundary this module draws is **remote vs. local**, not "Azure-related":
//! Azurite, the Service Bus emulator, and the SQL emulator are all Azure
//! technologies but run on the developer's machine, so they live outside.
//! Every module in here shells out to `az` (see [`cli`]) and needs a logged-in
//! account to return anything.
//!
//! Dependencies point one way — inward to [`cli`], never back out to the
//! runner's local services. Keeping that true is what would let this subtree
//! become its own crate.

pub mod app_config;
pub mod auth;
pub mod cli;
pub mod devops_cli;
pub mod env_compare;
pub mod eventgrid_check;
pub mod security_compare;
pub mod servicebus;
pub mod sync;
