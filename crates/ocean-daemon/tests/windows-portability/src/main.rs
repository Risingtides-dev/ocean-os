#![deny(warnings)]
#![allow(dead_code)]

use std::{collections::HashSet, path::PathBuf};

// These are the production modules, not copied portability substitutes. The
// dedicated feature removes only daemon AppState/Axum route coupling from the
// common registry module while this Windows target compiles its Nt* reader.
#[path = "../../../src/extension_registry.rs"]
mod extension_registry;
#[path = "../../../src/extension_service_unsupported.rs"]
mod extension_service;

#[tokio::main]
async fn main() {
    // Type-check and code-generate the actual unsupported supervisor's complete
    // start/shutdown path. It calls the actual coherent common reader and has no
    // child-process API or native activation path.
    let supervisor = extension_service::ExtensionSupervisor::new();
    supervisor.start(PathBuf::from("."), HashSet::new()).await;
    supervisor.shutdown().await;
    let _ = supervisor.status_cache().snapshot();
}
