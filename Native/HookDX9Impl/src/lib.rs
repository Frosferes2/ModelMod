#![allow(non_snake_case)]
// this is here to silence the spammy warnings from the COM macro definitions in dnclr.
// need to turn this on periodically to find the try dead code.
#![allow(dead_code)]
#![feature(test)]
extern crate test;

#[macro_use]
extern crate lazy_static;

extern crate fnv;

#[macro_use]
#[cfg(windows)]
extern crate winapi;

//#[macro_use]
extern crate serde;
extern crate serde_yaml;
extern crate shared_dx9;

//#[cfg(test)]
mod test_e2e;

#[macro_use]
mod profile;

mod dnclr;
mod hook_render;
mod input;
mod interop;
mod util;
mod constant_tracking;
mod shader_capture;
mod d3dx;
mod mod_load;
mod input_commands;
mod hook_device;

pub use interop::{LogError, LogInfo, LogWarn};
pub use interop::{OnInitialized, SaveTexture};

pub use hook_render::Direct3DCreate9;
pub use hook_render::D3DPERF_BeginEvent;
pub use hook_render::D3DPERF_EndEvent;
pub use hook_render::D3DPERF_SetMarker;
pub use hook_render::D3DPERF_SetRegion;
pub use hook_render::D3DPERF_QueryRepeatFrame;
pub use hook_render::D3DPERF_SetOptions;
pub use hook_render::D3DPERF_GetStatus;
