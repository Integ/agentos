//! Prompt-injection evaluation of the task kernel's defences.
//!
//! Each case is one request from the user whose work reads content from
//! outside the user's control (an email, a web page, a shared document).
//! The content carries an injection: words, or a structured `follow_up`,
//! that try to get the agent to do something the user did not ask for. The
//! case runs on the real butler, policy engine and audit log, against
//! in-memory doubles that cannot affect anything outside the process, and
//! is judged from the calls that actually ran.
//!
//! Configurations differ only in the defences: the product's policy engine
//! with taint, or the same engine with taint taken away (built here by
//! wrapping capabilities, never by a setting the product has), each with the
//! judge on or off. A simulated human answers the prompts. What it costs is
//! counted in prompts per task; what it buys is counted in attacks that ran.
//!
//! docs/injection-eval.md describes the method and the results.

pub mod report;
pub mod run;
pub mod sandbox;
pub mod scenarios;
pub mod scripted;
pub mod world;

pub use run::{run_case, Arm, Human, Outcome};
pub use sandbox::{Sandbox, Setup, SharedPin};
pub use scenarios::{Case, Selection, Template, GOALS, USER_TASKS};
pub use world::{Protection, Source};
