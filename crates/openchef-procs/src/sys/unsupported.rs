//! Platforms without a supervision implementation.

use std::process::{Child, Command};

use super::Probe;
use crate::Error;

pub(crate) const SUPPORTS_TERM_SIGNAL: bool = false;

/// Never constructed: `new` fails.
pub(crate) struct Platform;

// Same methods as every platform, so none of them can use `self`.
#[allow(clippy::unused_self)]
impl Platform {
    pub(crate) fn new() -> Result<Self, Error> {
        Err(Error::Unsupported)
    }

    pub(crate) fn configure(&self, _cmd: &mut Command) {}

    pub(crate) fn attach(&self, _child: &Child) -> Result<(), Error> {
        Err(Error::Unsupported)
    }

    pub(crate) fn signal_term(&self, _pid: u32) -> Result<(), Error> {
        Err(Error::Unsupported)
    }

    pub(crate) fn kill(&self, _pid: u32) -> Result<(), Error> {
        Err(Error::Unsupported)
    }

    pub(crate) fn release(&self) {}
}

pub(crate) fn wait_exit_no_reap(_pid: u32) -> bool {
    false
}

pub(crate) fn start_token(_pid: u32) -> Result<u64, Error> {
    Err(Error::Unsupported)
}

pub(crate) fn probe(_pid: u32) -> Result<Option<Probe>, Error> {
    Err(Error::Unsupported)
}

pub(crate) fn kill_orphan(_pid: u32) -> Result<(), Error> {
    Err(Error::Unsupported)
}
