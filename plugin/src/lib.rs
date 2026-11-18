//! The host's hardware sensors as a Yoke Plugin: hwmon, the thermal zones and their cooling devices, and
//! IIO, read from sysfs.
//!
//! `backend` reads the sensors and follows what the kernel signals, and knows nothing of Yoke; `netlink`
//! is the kernel's thermal events it follows; `plugin` is the Plugin around them.

pub mod backend;
pub mod netlink;
pub mod plugin;
