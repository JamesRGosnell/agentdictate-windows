//! Hands stopped dictations to a processing worker.

use agentdictate_runtime::{RecordingJob, Transcriber};

/// A transcriber the daemon can hand one stopped dictation to. The
/// dictation is then transcribed on the processing worker while the daemon
/// keeps serving commands and records the next one.
pub trait ProcessingTranscriber: Transcriber {
    type Job: Transcriber + Send + 'static;

    /// A transcriber of its own for `job`, taking over what was prepared
    /// while it recorded, such as a live transcription stream. It runs
    /// under the daemon lock, so it must not block.
    fn for_job(&mut self, job: &RecordingJob) -> Self::Job;
}
