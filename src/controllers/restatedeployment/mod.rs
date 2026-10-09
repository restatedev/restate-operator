pub(crate) mod cleanup;
pub mod controller;
pub(crate) mod registration;
pub(crate) mod retry;

mod reconcilers;

#[cfg(test)]
mod e2e;

pub use controller::run;
