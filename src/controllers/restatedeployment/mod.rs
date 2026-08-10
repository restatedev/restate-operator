pub(crate) mod cleanup;
pub mod controller;
pub(crate) mod registration;

mod reconcilers;

#[cfg(test)]
mod e2e;

pub use controller::run;
