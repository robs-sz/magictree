//! Repository discovery: extract facts, then ask about what cannot be known.
//!
//! Discovery never executes anything from the repository and never guesses. It
//! returns facts with a confidence and a closed list of unknowns. `init` refuses
//! to write a manifest while any unknown is unanswered, so the manifest is only
//! ever produced from information that was actually determined.

pub mod extractors;
pub mod report;

pub use extractors::extract;
pub use report::{
    Answer, AnswerSet, AppFacts, Fact, FactData, Report, Scope, Unknown, UnknownKind,
    ANSWERS_VERSION, REPORT_VERSION,
};
