#![allow(non_camel_case_types)]
#![allow(non_upper_case_globals)]
#![allow(non_snake_case)]

use crate::kernel_metadata::siginfo_str_repr;
use bitflags::_core::fmt::Formatter;
use std::fmt;
use std::fmt::Debug;

include!(concat!(env!("OUT_DIR"), "/signal_bindings_generated.rs"));

impl Debug for siginfo_t {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&siginfo_str_repr(self))
    }
}
