//! Message formatting. Mention pills and markup are shared fleet-wide via
//! `mxbot_common::format`.

pub use mxbot_common::format::{
    extract_mxids, fetch_names, mentionify, mentionify_rich, mentionify_with_names,
};
