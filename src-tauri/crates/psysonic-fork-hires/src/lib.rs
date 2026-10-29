//! The fork's hi-res mix engine. In Phase 0 it only holds the test kit and the
//! baseline tests that pin down what rodio does to audio today.

pub mod format;
pub mod hq_resample;
pub mod rate_plan;

#[cfg(any(test, feature = "testkit"))]
pub mod testkit;

#[cfg(test)]
mod baseline_tests;
#[cfg(test)]
mod format_tests;
#[cfg(test)]
mod hq_resample_tests;
#[cfg(test)]
mod rate_plan_tests;
