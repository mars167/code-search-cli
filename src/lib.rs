pub mod cli;
pub mod commands;
pub mod completions;
pub mod index;
pub mod output;
pub mod scip;
pub mod scip_index;
pub mod scip_proto;
pub mod search;
pub mod snapshot_store;
pub mod syntax;
pub mod text_index;
pub mod workspace;

pub mod graph;
pub type AppResult<T> = anyhow::Result<T>;
