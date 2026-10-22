mod controller;
mod discovery;
mod gc;

pub use controller::{promote, run};
pub use discovery::{DiscoveryClient, DiscoveryError, discover_url};
