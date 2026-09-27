//! Local engine supervision: the `yue-server` process from yue2.cpp, the
//! `mm-server` process from minimaxmusic.cpp, and the process group every
//! child of the studio belongs to.

pub mod mm_server;
pub mod mm_train;
pub mod model;
pub mod process_group;
pub mod yue_server;
pub mod yue_train;
